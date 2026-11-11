//! What the bot remembers about the economy beyond one goal: the server's last refusal, the craft
//! cast, trade-pack payment mails that wait to be collected, and routes that did not work.
//!
//! The [`Ledger`] lives in [`BotState`](crate::bot::state::BotState) and is fed with every server
//! event ([`Ledger::observe`]). The payment mail of a sold pack can arrive minutes or hours after
//! the sale (the server delays it, `TradePackMailDelayInMinutes`), long after the trade-run goal
//! ended, so collecting it is a background duty: [`Ledger::tick`] runs on every brain tick whatever
//! the bot is doing and walks each payment mail through *read* and *take attachments* without ever
//! blocking.

use aa_proto::cs::{self, Out};
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::{mail_type, ActivityEvent};
use aa_proto::sc_world::WorldEvent;
use smallvec::SmallVec;

use crate::bot::state::dist2d;
use crate::bot::watch::PayFault;

/// `ErrorMessageType` of "the craft cannot go on"; the second field of the packet is the reason.
pub const ERR_CRAFT_STOPPED: i16 = 148;
/// Stands for a refused skill start in [`Ledger::error_since`]; the reason is the `SkillResult`.
pub const ERR_SKILL_REFUSED: i16 = i16::MIN;

/// A request that got no answer is repeated after this long.
const RETRY_MS: u64 = 4_000;
/// A request is sent at most this many times.
const MAX_TRIES: u8 = 3;
/// While payments are outstanding the mailbox is listed this often (the server pushes new mail
/// by itself while the bot is online; the listing covers mail that came due while it was not).
const LIST_EVERY_MS: u64 = 300_000;
/// Payment mails and completed ones that are remembered at once.
const MAX_DUE: usize = 4;
const MAX_DONE: usize = 4;
/// Quarantined routes remembered at once.
const MAX_SKIPS: usize = 6;
/// A dropped trade pack is gone back for this long after the death. The doodad itself lasts days
/// (the server's `DoodadFuncFinal.After`), but a bot that cannot reach it in this time is better
/// off crafting a new pack.
pub const RECOVERY_TTL_MS: u64 = 45 * 60_000;
/// A pack doodad this close to the death spot is the one the bot dropped (metres).
pub const PACK_NEAR_M: f32 = 20.0;
/// The bot walks back to the death spot from farther than this (metres).
pub const PACK_RETURN_M: f32 = 25.0;
/// A pack doodad that has not shown up this long after the bot arrived is gone.
pub const PACK_APPEAR_MS: u64 = 10_000;

/// Which kind of thing a quarantine entry names (ids of different tables may coincide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipKind {
    /// A craft id.
    Craft = 1,
    /// A buyer NPC template.
    Buyer = 2,
    /// A bench spawn, see [`spot_key`].
    Bench = 3,
}

/// A key for a bench spawn: its position in 8 m cells.
pub fn spot_key(x: f32, y: f32) -> u32 {
    (((x / 8.0) as i32 as u32) << 16) | ((y / 8.0) as i32 as u32 & 0xffff)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Stage {
    /// Announced, nothing sent yet.
    #[default]
    New,
    /// `CSReadMail` sent, waiting for the body.
    ReadSent,
    /// `CSTakeAttachmentSequentially` sent, waiting for `SCAttachmentTaken`.
    TakeSent,
}

/// A payment mail on its way to the purse.
#[derive(Debug, Clone, Copy, Default)]
struct Due {
    mail_id: i64,
    stage: Stage,
    /// The body arrived.
    known: bool,
    tries: u8,
    items: u8,
    money: i32,
    /// Bot clock of the last request.
    at_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct Skip {
    key: u64,
    until_ms: u64,
}

/// A trade pack the bot dropped when it died.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LostPack {
    /// The pack item template.
    pub item: u32,
    /// Where the bot died.
    pub pos: [f32; 3],
    /// Instance the death happened in (0 = the main world).
    pub inst: u32,
    /// Bot clock of the death.
    pub at_ms: u64,
    /// Bot clock after which the pack is written off.
    pub until_ms: u64,
}

/// Why a dropped pack is not coming back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackGone {
    /// The bot died in another world (an instance) and cannot get back to it.
    OtherWorld,
    /// The recovery took too long.
    Expired,
    /// The doodad is not there (somebody took it, or it decayed).
    Taken,
}

impl PackGone {
    /// A short word for the log.
    pub fn word(self) -> &'static str {
        match self {
            PackGone::OtherWorld => "in another world",
            PackGone::Expired => "not reached in time",
            PackGone::Taken => "taken or decayed",
        }
    }
}

/// What to do about a dropped pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Walk back to the death spot.
    Return,
    /// At the death spot: wait for the doodad to show up.
    Wait,
    /// The pack doodad is there: use it.
    Pick(u32),
    /// It is gone.
    Gone(PackGone),
}

/// The decision for a dropped pack at `now_ms`: the bot is in world `inst` at `me`, `doodad` is a
/// pack doodad in view near the death spot `(object id, distance to the bot)`, and `waited_ms` is
/// how long the bot has stood at the spot without seeing one. Pure, so the whole after-death
/// decision is tested without a server.
pub fn recovery_step(
    lost: &LostPack,
    now_ms: u64,
    inst: u32,
    me: [f32; 3],
    doodad: Option<(u32, f32)>,
    waited_ms: u64,
) -> Recovery {
    if now_ms >= lost.until_ms {
        return Recovery::Gone(PackGone::Expired);
    }
    if inst != lost.inst {
        return Recovery::Gone(PackGone::OtherWorld);
    }
    if let Some((obj, _)) = doodad {
        return Recovery::Pick(obj);
    }
    if dist2d(me, lost.pos) > PACK_RETURN_M {
        return Recovery::Return;
    }
    if waited_ms < PACK_APPEAR_MS {
        Recovery::Wait
    } else {
        Recovery::Gone(PackGone::Taken)
    }
}

/// A payment that was collected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Paid {
    /// Mail id.
    pub mail_id: i64,
    /// Copper in the mail.
    pub money: i32,
    /// Attached items (specialty coins, ...).
    pub items: u8,
}

/// The parts that are empty most of the time live behind one pointer that is allocated on first
/// use, which keeps the ledger (part of every bot's state) small.
#[derive(Debug, Clone, Default)]
struct Cold {
    due: SmallVec<[Due; 2]>,
    /// Mail ids that were collected (a repeated announcement is ignored).
    done: SmallVec<[i64; 4]>,
    skips: SmallVec<[Skip; 4]>,
    paid: Option<Paid>,
    /// The pack waiting at the spot the bot died at.
    lost: Option<LostPack>,
}

impl Cold {
    /// Remembers a mail that is finished with.
    fn remember(&mut self, mail_id: i64) {
        if self.done.len() >= MAX_DONE {
            self.done.remove(0);
        }
        self.done.push(mail_id);
    }
}

/// See the module docs.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    /// The last `SCErrorMsg` or refused skill start: bot clock, error, reason.
    last_error: Option<(u64, i16, u32)>,
    /// Bot clock of the last own skill start the server accepted.
    cast_ms: u64,
    last_list_ms: u64,
    cold: Option<Box<Cold>>,
    /// Copper collected from payment mails.
    pub earned: i64,
    /// Payment mails collected.
    pub payments: u32,
    /// Packs sold.
    pub packs_sold: u32,
    /// Packs that were dropped at a death and never recovered.
    pub packs_lost: u32,
    /// Unread received mail according to the server's counters.
    unread: i32,
    /// Packs sold whose payment has not been collected.
    awaiting: u8,
    /// The mailbox was listed because of unread mail at login.
    listed_for_unread: bool,
    /// Bot clock at which a trade run that carried a pack was interrupted, see
    /// [`Ledger::note_interrupted_run`].
    run_interrupted_ms: Option<u64>,
    /// A payment mail the bot had to give up on, until [`Ledger::take_fault`].
    fault: Option<PayFault>,
}

impl Ledger {
    // ---- events -------------------------------------------------------------------------

    /// Feeds the ledger one server event; `me_obj` is the bot's object id (0 before the spawn).
    pub fn observe(&mut self, ev: &ServerEvent, now_ms: u64, me_obj: u32) {
        match ev {
            ServerEvent::World(WorldEvent::ErrorMsg {
                err, err2, kind, ..
            }) => {
                // The reason is the second error field when it differs from the first (the craft
                // stop reason), else the type field.
                let reason = if err2 != err && *err2 > 0 {
                    *err2 as u32
                } else {
                    *kind
                };
                self.last_error = Some((now_ms, *err, reason));
            }
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj, result, ..
            }) if *caster_obj == me_obj && me_obj != 0 => {
                if *result == 0 {
                    self.cast_ms = now_ms;
                } else {
                    self.last_error = Some((now_ms, ERR_SKILL_REFUSED, u32::from(*result)));
                }
            }
            ServerEvent::Activity(a) => self.observe_mail(a, now_ms),
            _ => {}
        }
    }

    fn observe_mail(&mut self, ev: &ActivityEvent, now_ms: u64) {
        match ev {
            ActivityEvent::CountUnreadMail { unread } => self.unread = unread_received(unread),
            ActivityEvent::MailReceived {
                mail,
                unread,
                is_cancel,
                ..
            } => {
                self.unread = unread_received(unread);
                if mail.mail_type != mail_type::SYS_SELL_BACKPACK {
                    return;
                }
                if *is_cancel {
                    if let Some(c) = self.cold.as_deref_mut() {
                        c.due.retain(|d| d.mail_id != mail.mail_id);
                    }
                } else if !self.knows(mail.mail_id) {
                    let due = &mut self.cold().due;
                    if due.len() >= MAX_DUE {
                        due.remove(0);
                    }
                    due.push(Due {
                        mail_id: mail.mail_id,
                        at_ms: now_ms,
                        ..Due::default()
                    });
                }
            }
            ActivityEvent::MailBody { body, unread, .. } => {
                self.unread = unread_received(unread);
                if let Some(d) = self
                    .cold
                    .as_deref_mut()
                    .and_then(|c| c.due.iter_mut().find(|d| d.mail_id == body.mail_id))
                {
                    d.known = true;
                    d.money = body.money;
                    d.items = u8::try_from(body.items.len()).unwrap_or(u8::MAX);
                }
            }
            ActivityEvent::AttachmentTaken { mail_id, .. } => {
                let taken = self.cold.as_deref_mut().and_then(|c| {
                    let i = c.due.iter().position(|d| d.mail_id == *mail_id)?;
                    Some(c.due.remove(i))
                });
                if let Some(d) = taken {
                    self.finish(d);
                }
            }
            ActivityEvent::MailDeleted {
                mail_id, unread, ..
            } => {
                self.unread = unread_received(unread);
                if let Some(c) = self.cold.as_deref_mut() {
                    c.due.retain(|d| d.mail_id != *mail_id);
                }
            }
            _ => {}
        }
    }

    fn cold(&mut self) -> &mut Cold {
        self.cold.get_or_insert_with(Default::default)
    }

    fn knows(&self, mail_id: i64) -> bool {
        self.cold.as_deref().is_some_and(|c| {
            c.due.iter().any(|d| d.mail_id == mail_id) || c.done.contains(&mail_id)
        })
    }

    fn finish(&mut self, d: Due) {
        self.awaiting = self.awaiting.saturating_sub(1);
        self.earned += i64::from(d.money.max(0));
        self.payments += 1;
        let c = self.cold();
        c.remember(d.mail_id);
        c.paid = Some(Paid {
            mail_id: d.mail_id,
            money: d.money,
            items: d.items,
        });
    }

    // ---- queries ------------------------------------------------------------------------

    /// The last refusal the server sent at or after bot clock `since_ms`: `(error, reason)`.
    pub fn error_since(&self, since_ms: u64) -> Option<(i16, u32)> {
        self.last_error
            .filter(|e| e.0 >= since_ms)
            .map(|e| (e.1, e.2))
    }

    /// Whether the server accepted an own skill start at or after bot clock `since_ms`.
    pub fn cast_since(&self, since_ms: u64) -> bool {
        self.cast_ms != 0 && self.cast_ms >= since_ms
    }

    /// Packs sold whose payment is still outstanding.
    pub fn awaiting(&self) -> u8 {
        self.awaiting
    }

    /// Payment mails that are being collected.
    pub fn collecting(&self) -> usize {
        self.cold.as_deref().map_or(0, |c| c.due.len())
    }

    // ---- duties -------------------------------------------------------------------------

    /// A trade run that carried a pack was cut short at `now_ms`.
    pub fn note_interrupted_run(&mut self, now_ms: u64) {
        self.run_interrupted_ms = Some(now_ms);
    }

    /// Whether a trade run with a pack was interrupted at this very tick `now_ms` (so by the goal
    /// that is starting now); the note is used up either way.
    pub fn take_interrupted_run(&mut self, now_ms: u64) -> bool {
        self.run_interrupted_ms.take() == Some(now_ms)
    }

    /// A pack was sold: a payment is on its way.
    pub fn note_sale(&mut self) {
        self.awaiting = self.awaiting.saturating_add(1);
        self.packs_sold += 1;
    }

    /// The bot died with a pack in the back slot: remember the spot, so the trade run can go back
    /// for the doodad after the resurrection.
    pub fn note_death_with_pack(&mut self, item: u32, pos: [f32; 3], inst: u32, now_ms: u64) {
        let lost = LostPack {
            item,
            pos,
            inst,
            at_ms: now_ms,
            until_ms: now_ms.saturating_add(RECOVERY_TTL_MS),
        };
        self.cold().lost = Some(lost);
    }

    /// The pack waiting at a death spot, if any.
    pub fn lost_pack(&self) -> Option<LostPack> {
        self.cold.as_deref().and_then(|c| c.lost)
    }

    /// Puts a dropped pack back after a reconnect (the note outlives the world state, like the
    /// gear memory; the deadline is kept as it was).
    pub fn restore_lost_pack(&mut self, lost: LostPack) {
        self.cold().lost = Some(lost);
    }

    /// Forgets the dropped pack (it was picked up, or written off).
    pub fn clear_lost_pack(&mut self) {
        if let Some(c) = self.cold.as_deref_mut() {
            c.lost = None;
        }
    }

    /// A dropped pack was not recovered: count it (the log line says why).
    pub fn note_pack_lost(&mut self) {
        self.packs_lost = self.packs_lost.saturating_add(1);
    }

    /// Keeps `kind`/`id` out of the plans for `for_ms`.
    pub fn skip(&mut self, kind: SkipKind, id: u32, now_ms: u64, for_ms: u64) {
        let skips = &mut self.cold().skips;
        skips.retain(|s| s.until_ms > now_ms);
        let key = skip_key(kind, id);
        if let Some(s) = skips.iter_mut().find(|s| s.key == key) {
            s.until_ms = now_ms + for_ms;
            return;
        }
        if skips.len() >= MAX_SKIPS {
            skips.remove(0);
        }
        skips.push(Skip {
            key,
            until_ms: now_ms + for_ms,
        });
    }

    /// Whether `kind`/`id` is quarantined at `now_ms`.
    pub fn skipped(&self, kind: SkipKind, id: u32, now_ms: u64) -> bool {
        let key = skip_key(kind, id);
        self.cold
            .as_deref()
            .is_some_and(|c| c.skips.iter().any(|s| s.key == key && s.until_ms > now_ms))
    }

    /// One step of the background duty: lists the mailbox when payments may be waiting, and walks
    /// the first payment mail that needs a request through read and take. Returns a payment that
    /// was just collected (once).
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<Out>) -> Option<Paid> {
        let paid = self.cold.as_deref_mut().and_then(|c| c.paid.take());
        if self.collecting() == 0 {
            if self.should_list(now_ms) {
                self.last_list_ms = now_ms;
                out.push(cs::list_mail());
            }
            return paid;
        }
        let cold = self.cold();
        let mut give_up = None;
        for (i, d) in cold.due.iter_mut().enumerate() {
            let idle = now_ms.saturating_sub(d.at_ms) >= RETRY_MS;
            match d.stage {
                Stage::New => {
                    out.push(cs::read_mail(false, d.mail_id));
                    *d = Due {
                        stage: Stage::ReadSent,
                        tries: 1,
                        at_ms: now_ms,
                        ..*d
                    };
                    break;
                }
                Stage::ReadSent if d.known => {
                    if d.money <= 0 && d.items == 0 {
                        give_up = Some(i); // empty: nothing to take
                    } else {
                        out.push(cs::take_attachment_sequentially(d.mail_id));
                        d.stage = Stage::TakeSent;
                        d.tries = 1;
                        d.at_ms = now_ms;
                    }
                    break;
                }
                Stage::ReadSent | Stage::TakeSent if idle => {
                    if d.tries >= MAX_TRIES {
                        give_up = Some(i);
                    } else {
                        d.tries += 1;
                        d.at_ms = now_ms;
                        out.push(if d.stage == Stage::ReadSent {
                            cs::read_mail(false, d.mail_id)
                        } else {
                            cs::take_attachment_sequentially(d.mail_id)
                        });
                    }
                    break;
                }
                _ => {}
            }
        }
        if let Some(i) = give_up {
            let d = cold.due.remove(i);
            cold.remember(d.mail_id);
            self.awaiting = self.awaiting.saturating_sub(1);
            // an empty payment, or a server that did not answer the requests for the mail
            self.fault = Some(if d.known && d.money <= 0 && d.items == 0 {
                PayFault::Empty
            } else {
                PayFault::Unanswered
            });
        }
        paid
    }

    /// The payment mail that had to be given up on, once (see [`PayFault`]).
    pub fn take_fault(&mut self) -> Option<PayFault> {
        self.fault.take()
    }

    fn should_list(&mut self, now_ms: u64) -> bool {
        if !self.listed_for_unread && self.unread > 0 {
            self.listed_for_unread = true;
            return true;
        }
        self.awaiting > 0 && now_ms.saturating_sub(self.last_list_ms) >= LIST_EVERY_MS
    }
}

fn skip_key(kind: SkipKind, id: u32) -> u64 {
    ((kind as u64) << 32) | u64::from(id)
}

/// Unread mail that was received (any category but sent mail).
fn unread_received(unread: &[i32; 4]) -> i32 {
    unread[1..].iter().map(|n| (*n).max(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_activity::{MailBodyInfo, MailHeader};

    fn header(id: i64, ty: u8) -> Box<MailHeader> {
        Box::new(MailHeader {
            mail_id: id,
            mail_type: ty,
            status: 0,
            title: "".into(),
            sender: "".into(),
            attachments: 1,
            receiver: "".into(),
            open_date: 0,
            returned: false,
            extra: 0,
        })
    }

    fn got(id: i64, ty: u8) -> ServerEvent {
        ServerEvent::Activity(ActivityEvent::MailReceived {
            mail: header(id, ty),
            unread: [0, 1, 0, 0],
            is_cancel: false,
            body: None,
        })
    }

    fn body(id: i64, money: i32) -> ServerEvent {
        ServerEvent::Activity(ActivityEvent::MailBody {
            is_prepare: false,
            is_sent: false,
            body: Box::new(MailBodyInfo {
                mail_id: id,
                mail_type: mail_type::SYS_SELL_BACKPACK,
                receiver: "".into(),
                title: "".into(),
                text: "".into(),
                money,
                billing: 0,
                money2: 0,
                send_date: 0,
                recv_date: 0,
                open_date: 0,
                items: Vec::new(),
            }),
            open_date_modified: true,
            unread: [0, 0, 0, 0],
        })
    }

    fn taken(id: i64) -> ServerEvent {
        ServerEvent::Activity(ActivityEvent::AttachmentTaken {
            mail_id: id,
            money: true,
            aa_point: false,
            take_sequentially: true,
            items: Box::new([]),
        })
    }

    fn ops(out: &[Out]) -> Vec<u16> {
        out.iter().map(|o| o.opcode).collect()
    }

    #[test]
    fn a_payment_mail_that_is_empty_or_unanswered_is_a_fault_once() {
        // an empty one: announced, read, and there is nothing to take
        let mut l = Ledger::default();
        let mut out = Vec::new();
        l.note_sale();
        l.observe(&got(5, mail_type::SYS_SELL_BACKPACK), 1_000, 9);
        l.tick(1_000, &mut out);
        assert_eq!(out.len(), 1, "the mail is read");
        l.observe(&body(5, 0), 1_100, 9);
        l.tick(1_200, &mut out);
        assert_eq!(l.take_fault(), Some(PayFault::Empty));
        assert_eq!(l.take_fault(), None, "taken once");
        assert_eq!(l.awaiting(), 0);

        // one the server never answers: three tries, then the fault
        let mut l = Ledger::default();
        l.note_sale();
        l.observe(&got(6, mail_type::SYS_SELL_BACKPACK), 1_000, 9);
        for t in [1_000, 6_000, 11_000, 16_000, 21_000] {
            l.tick(t, &mut out);
        }
        assert_eq!(l.take_fault(), Some(PayFault::Unanswered));

        // a mail that is paid out is no fault
        let mut l = Ledger::default();
        l.note_sale();
        l.observe(&got(7, mail_type::SYS_SELL_BACKPACK), 1_000, 9);
        l.tick(1_000, &mut out);
        l.observe(&body(7, 5_000), 1_100, 9);
        l.tick(1_200, &mut out);
        assert_eq!(l.take_fault(), None);
    }

    #[test]
    fn a_payment_mail_is_read_then_taken_then_counted() {
        use aa_proto::opcodes::cs as op;
        let mut l = Ledger::default();
        l.note_sale();
        assert_eq!(l.awaiting(), 1);
        l.observe(&got(77, mail_type::SYS_SELL_BACKPACK), 1_000, 5);
        assert_eq!(l.collecting(), 1);
        let mut out = Vec::new();
        assert_eq!(l.tick(1_100, &mut out), None);
        assert_eq!(ops(&out), vec![op::CSReadMailPacket]);
        // no body yet: nothing more, then a retry after the retry time
        out.clear();
        assert_eq!(l.tick(2_000, &mut out), None);
        assert!(out.is_empty());
        assert_eq!(l.tick(5_200, &mut out), None);
        assert_eq!(ops(&out), vec![op::CSReadMailPacket]);
        // the body arrives
        out.clear();
        l.observe(&body(77, 33_000), 5_300, 5);
        l.tick(5_400, &mut out);
        assert_eq!(ops(&out), vec![op::CSTakeAttachmentSequentially]);
        // the attachment is taken
        out.clear();
        l.observe(&taken(77), 5_500, 5);
        assert_eq!(l.collecting(), 0);
        assert_eq!(l.awaiting(), 0);
        let paid = l.tick(5_600, &mut out).unwrap();
        assert_eq!(paid.money, 33_000);
        assert_eq!((l.earned, l.payments), (33_000, 1));
        assert!(out.is_empty());
        assert_eq!(l.tick(5_700, &mut out), None);
        // the same announcement again (a listing) is ignored
        l.observe(&got(77, mail_type::SYS_SELL_BACKPACK), 6_000, 5);
        assert_eq!(l.collecting(), 0);
    }

    #[test]
    fn other_mail_is_ignored_and_cancelled_mail_is_dropped() {
        let mut l = Ledger::default();
        l.observe(&got(1, mail_type::NORMAL), 0, 5);
        assert_eq!(l.collecting(), 0);
        l.observe(&got(2, mail_type::SYS_SELL_BACKPACK), 0, 5);
        assert_eq!(l.collecting(), 1);
        l.observe(
            &ServerEvent::Activity(ActivityEvent::MailReceived {
                mail: header(2, mail_type::SYS_SELL_BACKPACK),
                unread: [0; 4],
                is_cancel: true,
                body: None,
            }),
            1,
            5,
        );
        assert_eq!(l.collecting(), 0);
    }

    #[test]
    fn an_unanswered_request_is_given_up_after_three_tries() {
        let mut l = Ledger::default();
        l.note_sale();
        l.observe(&got(9, mail_type::SYS_SELL_BACKPACK), 0, 5);
        let mut out = Vec::new();
        let mut t = 100;
        for _ in 0..40 {
            l.tick(t, &mut out);
            t += 1_000;
        }
        // three reads, then the mailbox is listed once for the unread mail
        use aa_proto::opcodes::cs as op;
        assert_eq!(
            ops(&out),
            vec![
                op::CSReadMailPacket,
                op::CSReadMailPacket,
                op::CSReadMailPacket,
                op::CSListMailPacket
            ]
        );
        assert_eq!(l.collecting(), 0);
        assert_eq!(l.awaiting(), 0);
    }

    #[test]
    fn an_empty_mail_is_not_taken() {
        let mut l = Ledger::default();
        l.note_sale();
        l.observe(&got(9, mail_type::SYS_SELL_BACKPACK), 0, 5);
        let mut out = Vec::new();
        l.tick(10, &mut out);
        l.observe(&body(9, 0), 20, 5);
        out.clear();
        l.tick(30, &mut out);
        assert!(out.is_empty());
        assert_eq!(l.collecting(), 0);
    }

    #[test]
    fn unread_mail_at_login_lists_the_mailbox_once() {
        use aa_proto::opcodes::cs as op;
        let mut l = Ledger::default();
        let mut out = Vec::new();
        l.tick(10, &mut out);
        assert!(out.is_empty());
        l.observe(
            &ServerEvent::Activity(ActivityEvent::CountUnreadMail {
                unread: [0, 2, 0, 0],
            }),
            20,
            5,
        );
        l.tick(30, &mut out);
        assert_eq!(ops(&out), vec![op::CSListMailPacket]);
        out.clear();
        l.tick(40, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn outstanding_payments_relist_the_mailbox_slowly() {
        use aa_proto::opcodes::cs as op;
        let mut l = Ledger::default();
        l.note_sale();
        let mut out = Vec::new();
        l.tick(LIST_EVERY_MS, &mut out);
        assert_eq!(ops(&out), vec![op::CSListMailPacket]);
        out.clear();
        l.tick(LIST_EVERY_MS + 1_000, &mut out);
        assert!(out.is_empty());
        l.tick(2 * LIST_EVERY_MS, &mut out);
        assert_eq!(ops(&out), vec![op::CSListMailPacket]);
    }

    #[test]
    fn errors_and_casts_are_timestamped() {
        let mut l = Ledger::default();
        assert_eq!(l.error_since(0), None);
        l.observe(
            &ServerEvent::World(WorldEvent::ErrorMsg {
                err: ERR_CRAFT_STOPPED,
                err2: 29,
                kind: 0,
                notify: false,
            }),
            500,
            5,
        );
        assert_eq!(l.error_since(500), Some((ERR_CRAFT_STOPPED, 29)));
        assert_eq!(l.error_since(501), None);
        let started = |caster, result| {
            ServerEvent::World(WorldEvent::SkillStarted {
                caster_obj: caster,
                skill: 16766,
                tl: 1,
                target_obj: 0,
                result,
            })
        };
        l.observe(&started(6, 0), 600, 5); // someone else
        assert!(!l.cast_since(0));
        l.observe(&started(5, 0), 700, 5);
        assert!(l.cast_since(700) && !l.cast_since(701));
        l.observe(&started(5, 3), 800, 5);
        assert_eq!(l.error_since(800), Some((ERR_SKILL_REFUSED, 3)));
    }

    #[test]
    fn quarantine_expires_and_is_bounded() {
        let mut l = Ledger::default();
        l.skip(SkipKind::Craft, 6226, 1_000, 5_000);
        assert!(l.skipped(SkipKind::Craft, 6226, 5_999));
        assert!(!l.skipped(SkipKind::Craft, 6226, 6_000));
        assert!(!l.skipped(SkipKind::Buyer, 6226, 1_001));
        for i in 0..20 {
            l.skip(SkipKind::Buyer, i, 1_000, 5_000);
        }
        assert!(l.cold().skips.len() <= MAX_SKIPS);
        assert!(l.skipped(SkipKind::Buyer, 19, 2_000));
        assert!(!l.skipped(SkipKind::Buyer, 0, 2_000));
    }

    #[test]
    fn spot_keys_separate_neighbouring_cells() {
        assert_ne!(spot_key(100.0, 100.0), spot_key(110.0, 100.0));
        assert_eq!(spot_key(100.0, 100.0), spot_key(101.0, 103.0));
    }
    fn lost_pack() -> LostPack {
        LostPack {
            item: 31855,
            pos: [100.0, 0.0, 100.0],
            inst: 0,
            at_ms: 1_000,
            until_ms: 1_000 + RECOVERY_TTL_MS,
        }
    }

    #[test]
    fn a_dropped_pack_is_waited_for_recovered_or_written_off() {
        let lost = lost_pack();
        let far = [500.0, 0.0, 100.0];
        let near = [110.0, 0.0, 100.0];
        assert_eq!(
            recovery_step(&lost, 2_000, 0, far, None, 0),
            Recovery::Return,
            "far from the death spot: walk back"
        );
        assert_eq!(
            recovery_step(&lost, 2_000, 0, near, None, 0),
            Recovery::Wait,
            "there, but the doodad has not shown up yet"
        );
        assert_eq!(
            recovery_step(&lost, 2_000, 0, near, Some((7, 3.0)), 0),
            Recovery::Pick(7)
        );
        assert_eq!(
            recovery_step(&lost, 2_000, 0, near, None, PACK_APPEAR_MS),
            Recovery::Gone(PackGone::Taken),
            "waited out: somebody took it or it decayed"
        );
        assert_eq!(
            recovery_step(&lost, 2_000, 3, near, None, 0),
            Recovery::Gone(PackGone::OtherWorld)
        );
        assert_eq!(
            recovery_step(&lost, lost.until_ms, 0, near, None, 0),
            Recovery::Gone(PackGone::Expired)
        );
        assert_eq!(PackGone::Taken.word(), "taken or decayed");
    }

    #[test]
    fn a_dropped_pack_note_is_kept_until_it_is_cleared() {
        let mut l = Ledger::default();
        assert!(l.lost_pack().is_none());
        l.note_death_with_pack(31855, [1.0, 2.0, 3.0], 0, 1_000);
        let p = l.lost_pack().unwrap();
        assert_eq!(
            (p.item, p.pos, p.inst, p.at_ms),
            (31855, [1.0, 2.0, 3.0], 0, 1_000)
        );
        assert_eq!(p.until_ms, 1_000 + RECOVERY_TTL_MS);
        l.note_pack_lost();
        assert_eq!(l.packs_lost, 1);
        l.clear_lost_pack();
        assert!(l.lost_pack().is_none());
        // clearing a ledger that never had one does not allocate
        let mut l = Ledger::default();
        l.clear_lost_pack();
        assert!(l.cold.is_none());
    }

    #[test]
    fn an_interrupted_run_goes_only_to_the_goal_that_starts_in_the_same_tick() {
        let mut l = Ledger::default();
        assert!(!l.take_interrupted_run(10), "nothing was interrupted");
        l.note_interrupted_run(10);
        assert!(l.take_interrupted_run(10));
        assert!(!l.take_interrupted_run(10), "used up");
        l.note_interrupted_run(10);
        assert!(!l.take_interrupted_run(11), "a later goal gets nothing");
        assert!(!l.take_interrupted_run(10), "and the note is gone");
    }
}
