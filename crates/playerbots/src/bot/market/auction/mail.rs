//! The mail the auction house sends: what a sale, a win, an expired lot or an outbid brings, and
//! how a bot collects it.
//!
//! Results are delivered by mail only (`E/mail_kinds.rs:157-340`, `E/` = `aaemu-game/src/systems/
//! economy/`); no mailbox needs to be near:
//!
//! | Type | Receiver | Content |
//! | --- | --- | --- |
//! | 14 `OFF_SUCCESS` | seller | money: 90% of the price |
//! | 16 `BID_WIN` | buyer | the item |
//! | 15 `OFF_FAIL` | seller | the item of a lot that ended unsold; or, to an outbid bidder, the bid as money |
//! | 13 `OFF_CANCEL` | seller | the item of a cancelled lot |
//!
//! The server pushes the mail (`SCGotMail`) while the bot is online and lists it at the bot's
//! request (`CSListMail`); [`MailTracker`] walks every auction mail through *read* and *take the
//! attachments* (the same way `trade_run::ledger` does for trade pack payments) from the brain's
//! background duty, whatever the bot is doing, and keeps the counters the issue detectors compare.

use aa_proto::auction::mail_type;
use aa_proto::cs::{self, Out};
use aa_proto::sc_activity::ActivityEvent;
use smallvec::SmallVec;

/// A request that got no answer is repeated after this long.
const RETRY_MS: u64 = 4_000;
/// A request is sent at most this many times.
const MAX_TRIES: u8 = 3;
/// While results are outstanding the mailbox is listed this often.
const LIST_EVERY_MS: u64 = 300_000;
/// Mails that are collected at once.
const MAX_DUE: usize = 6;
/// Finished mail ids that are remembered.
const MAX_DONE: usize = 16;

/// What an auction mail brought.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The seller's lot sold (money).
    Sold,
    /// The seller's cancelled lot came back (item).
    Cancelled,
    /// The seller's lot ended unsold (item).
    Expired,
    /// A bid was outbid (money back).
    Refund,
    /// The bidder won (item).
    Won,
}

impl Kind {
    /// The kind of a mail with this type and number of attached items, `None` for other mail.
    pub fn of(mail_type: u8, items: u8) -> Option<Kind> {
        match mail_type {
            mail_type::OFF_SUCCESS => Some(Kind::Sold),
            mail_type::OFF_CANCEL => Some(Kind::Cancelled),
            mail_type::OFF_FAIL if items > 0 => Some(Kind::Expired),
            mail_type::OFF_FAIL => Some(Kind::Refund),
            mail_type::BID_WIN => Some(Kind::Won),
            _ => None,
        }
    }

    /// Whether the mail tells the seller what became of one of its lots.
    pub fn is_seller(self) -> bool {
        matches!(self, Kind::Sold | Kind::Cancelled | Kind::Expired)
    }
}

/// A mail that was collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// Mail id.
    pub mail_id: i64,
    /// What it was.
    pub kind: Kind,
    /// Copper in it.
    pub money: i32,
    /// Items in it.
    pub items: u8,
    pub sale: Option<Box<SaleDetails>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleDetails {
    pub item: String,
    pub quantity: u32,
    pub paid: i32,
    pub buyout: i32,
    pub fee: i32,
}

impl SaleDetails {
    fn parse(text: &str) -> Option<Self> {
        // Sale mail is authored by the auction service as body('name', count, share, buyout,
        // tax, fee). Split off the numeric fields first since item names can contain commas.
        let text = text.strip_prefix("body(")?.strip_suffix(')')?;
        let mut fields = text.rsplitn(6, ',').map(str::trim);
        let fee = fields.next()?.parse().ok()?;
        let _tax: i32 = fields.next()?.parse().ok()?;
        let buyout = fields.next()?.parse().ok()?;
        let paid = fields.next()?.parse().ok()?;
        let quantity = fields.next()?.parse().ok()?;
        let item = fields.next()?.strip_prefix('\'')?.strip_suffix('\'')?;
        let mut chars = item.chars();
        let mut name = String::new();
        while let Some(c) = chars.next() {
            name.push(if c != '\\' {
                c
            } else {
                match chars.next()? {
                    c @ ('\\' | '\'') => c,
                    '0' if chars.next()? == '4' => match chars.next()? {
                        '0' => '(',
                        '1' => ')',
                        _ => return None,
                    },
                    _ => return None,
                }
            });
        }
        (fee >= 0 && buyout > 0 && paid > 0 && quantity > 0).then_some(Self {
            item: name,
            quantity,
            paid,
            buyout,
            fee,
        })
    }
}

#[test]
fn auction_receipt_preserves_names_with_commas_and_lua_escapes() {
    let sale = SaleDetails::parse(
        r"body('Miner\'s Ore, Refined \040Grade 1\041', 10, 1620, 1800, 180, 72)",
    )
    .unwrap();
    assert_eq!(sale.item, "Miner's Ore, Refined (Grade 1)");
    assert_eq!(
        (sale.quantity, sale.paid, sale.buyout, sale.fee),
        (10, 1620, 1800, 72)
    );
    assert!(SaleDetails::parse("body('Ore', 10, 1620, 1800, 180)").is_none());
}

/// Why a mail had to be given up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The mail has neither money nor items: nothing to take (a mail already emptied is not a
    /// fault; this is one the server announced with an attachment).
    Empty(Kind),
    /// The server did not answer the requests for the mail or its attachments.
    Unanswered(Kind),
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

#[derive(Debug)]
struct Due {
    mail_id: i64,
    kind: Kind,
    /// The header said it carries items.
    announced_items: u8,
    stage: Stage,
    /// The body arrived.
    known: bool,
    tries: u8,
    items: u8,
    money: i32,
    sale: Option<Box<SaleDetails>>,
    /// Attachments still to take (items and the money count one each).
    left: u8,
    /// Bot clock of the last request.
    at_ms: u64,
}

/// Counters of what the mail brought.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Sale mails collected.
    pub sold: u32,
    /// Copper they paid.
    pub earned: i64,
    /// Cancelled lots that came back.
    pub cancelled: u32,
    /// Lots that ended unsold and came back.
    pub expired: u32,
    /// Outbid refunds.
    pub refunds: u32,
    /// Won items collected.
    pub won: u32,
}

/// See the module docs.
#[derive(Debug, Default)]
pub struct MailTracker {
    due: SmallVec<[Due; 2]>,
    /// Mail ids that were seen (and collected, or given up on): a repeated announcement is ignored.
    done: SmallVec<[i64; 8]>,
    last_list_ms: u64,
    /// List the mailbox on the next tick.
    list_wanted: bool,
    /// Unread received mail according to the server's counters.
    unread: i32,
    listed_for_unread: bool,
    /// What was collected.
    pub counts: Counts,
    /// Seller-side result mails seen since the connection started (header level, collected or not).
    pub seller_mails: u32,
    /// Win mails seen since the connection started.
    pub win_mails: u32,
    /// The last collected mail, until [`MailTracker::take_settled`].
    settled: Option<Settled>,
    fault: Option<Fault>,
}

impl MailTracker {
    /// Whether the mail tracker is working on something.
    pub fn collecting(&self) -> usize {
        self.due.len()
    }

    /// Lists the mailbox on the next tick (a visit to the auction house starts with it, so mail
    /// that came while the bot was away is collected).
    pub fn want_list(&mut self) {
        self.list_wanted = true;
    }

    /// Whether the mailbox was listed at or after bot clock `since_ms`.
    pub fn listed_since(&self, since_ms: u64) -> bool {
        self.last_list_ms != 0 && self.last_list_ms >= since_ms
    }

    fn knows(&self, mail_id: i64) -> bool {
        self.due.iter().any(|d| d.mail_id == mail_id) || self.done.contains(&mail_id)
    }

    fn remember(&mut self, mail_id: i64) {
        if self.done.len() >= MAX_DONE {
            self.done.remove(0);
        }
        self.done.push(mail_id);
    }

    /// Whether this event can concern the tracker at all (so a bot without market memory makes
    /// it for an auction mail).
    pub fn concerns(ev: &ActivityEvent) -> bool {
        match ev {
            ActivityEvent::MailReceived { mail, .. } => mail_type::is_auction(mail.mail_type),
            ActivityEvent::MailList { mails, is_sent } => {
                !is_sent && mails.iter().any(|m| mail_type::is_auction(m.mail_type))
            }
            _ => false,
        }
    }

    /// Feeds the tracker a mail event.
    pub fn observe(&mut self, ev: &ActivityEvent, now_ms: u64) {
        match ev {
            ActivityEvent::CountUnreadMail { unread } => self.unread = unread_received(unread),
            ActivityEvent::MailReceived {
                mail,
                unread,
                is_cancel,
                ..
            } => {
                self.unread = unread_received(unread);
                if *is_cancel {
                    self.due.retain(|d| d.mail_id != mail.mail_id);
                } else {
                    self.announce(mail.mail_id, mail.mail_type, mail.attachments, now_ms);
                }
            }
            ActivityEvent::MailList { mails, is_sent } if !is_sent => {
                for m in mails {
                    self.announce(m.mail_id, m.mail_type, m.attachments, now_ms);
                }
            }
            ActivityEvent::MailBody { body, unread, .. } => {
                self.unread = unread_received(unread);
                if let Some(d) = self.due.iter_mut().find(|d| d.mail_id == body.mail_id) {
                    d.known = true;
                    d.money = body.money;
                    d.items = u8::try_from(body.items.len()).unwrap_or(u8::MAX);
                    d.left = d.items.saturating_add(u8::from(body.money > 0));
                    if d.kind == Kind::Sold && body.mail_type == mail_type::OFF_SUCCESS {
                        d.sale = SaleDetails::parse(&body.text).map(Box::new);
                    }
                }
            }
            ActivityEvent::AttachmentTaken {
                mail_id,
                money,
                items,
                ..
            } => {
                let got = u8::try_from(items.len())
                    .unwrap_or(u8::MAX)
                    .saturating_add(u8::from(*money));
                if let Some(i) = self.due.iter().position(|d| d.mail_id == *mail_id) {
                    let d = &mut self.due[i];
                    d.left = d.left.saturating_sub(got.max(1));
                    d.tries = 1;
                    d.at_ms = now_ms;
                    if d.left == 0 {
                        let d = self.due.remove(i);
                        self.finish(d);
                    } else {
                        // more to take
                        d.stage = Stage::ReadSent;
                    }
                }
            }
            ActivityEvent::MailDeleted {
                mail_id, unread, ..
            } => {
                self.unread = unread_received(unread);
                self.due.retain(|d| d.mail_id != *mail_id);
            }
            _ => {}
        }
    }

    fn announce(&mut self, mail_id: i64, mail_type: u8, attachments: u8, now_ms: u64) {
        let Some(kind) = Kind::of(mail_type, attachments) else {
            return;
        };
        if self.knows(mail_id) {
            return;
        }
        if kind.is_seller() {
            self.seller_mails += 1;
        }
        if kind == Kind::Won {
            self.win_mails += 1;
        }
        if self.due.len() >= MAX_DUE {
            let d = self.due.remove(0);
            self.remember(d.mail_id);
        }
        self.due.push(Due {
            mail_id,
            kind,
            announced_items: attachments,
            stage: Stage::New,
            known: false,
            tries: 0,
            items: 0,
            money: 0,
            sale: None,
            left: 0,
            at_ms: now_ms,
        });
    }

    fn finish(&mut self, d: Due) {
        self.remember(d.mail_id);
        let c = &mut self.counts;
        match d.kind {
            Kind::Sold => {
                c.sold += 1;
                c.earned += i64::from(d.money.max(0));
            }
            Kind::Cancelled => c.cancelled += 1,
            Kind::Expired => c.expired += 1,
            Kind::Refund => c.refunds += 1,
            Kind::Won => c.won += 1,
        }
        self.settled = Some(Settled {
            mail_id: d.mail_id,
            kind: d.kind,
            money: d.money,
            items: d.items,
            sale: d.sale,
        });
    }

    /// The mail that was collected since the last call.
    pub fn take_settled(&mut self) -> Option<Settled> {
        self.settled.take()
    }

    /// The mail that had to be given up on, once.
    pub fn take_fault(&mut self) -> Option<Fault> {
        self.fault.take()
    }

    /// One step of the background duty: lists the mailbox when asked (or when results are
    /// outstanding: `awaiting`) and walks the first mail that needs a request through read and
    /// take. At most one request per call.
    pub fn tick(&mut self, now_ms: u64, awaiting: bool, out: &mut Vec<Out>) {
        let mut give_up = None;
        for (i, d) in self.due.iter_mut().enumerate() {
            let idle = now_ms.saturating_sub(d.at_ms) >= RETRY_MS;
            match d.stage {
                Stage::New => {
                    out.push(cs::read_mail(false, d.mail_id));
                    d.stage = Stage::ReadSent;
                    d.tries = 1;
                    d.at_ms = now_ms;
                    return;
                }
                Stage::ReadSent if d.known => {
                    if d.left == 0 {
                        // nothing (left) to take: a mail that was already emptied
                        give_up = Some((i, d.announced_items > 0));
                    } else {
                        out.push(cs::take_attachment_sequentially(d.mail_id));
                        d.stage = Stage::TakeSent;
                        d.tries = 1;
                        d.at_ms = now_ms;
                        return;
                    }
                    break;
                }
                Stage::ReadSent | Stage::TakeSent if idle => {
                    if d.tries >= MAX_TRIES {
                        give_up = Some((i, false));
                    } else {
                        d.tries += 1;
                        d.at_ms = now_ms;
                        out.push(if d.stage == Stage::ReadSent {
                            cs::read_mail(false, d.mail_id)
                        } else {
                            cs::take_attachment_sequentially(d.mail_id)
                        });
                        return;
                    }
                    break;
                }
                _ => {}
            }
        }
        if let Some((i, announced)) = give_up {
            let d = self.due.remove(i);
            self.remember(d.mail_id);
            // (an emptied mail the header still announced an item for is the server's)
            self.fault = if d.known && d.left == 0 {
                announced.then_some(Fault::Empty(d.kind))
            } else {
                Some(Fault::Unanswered(d.kind))
            };
            return;
        }
        let first_listing = !self.listed_for_unread && self.unread > 0;
        let periodic = awaiting && now_ms.saturating_sub(self.last_list_ms) >= LIST_EVERY_MS;
        if (self.list_wanted || first_listing || periodic) && self.due.is_empty() {
            self.list_wanted = false;
            self.listed_for_unread |= first_listing;
            self.last_list_ms = now_ms;
            out.push(cs::list_mail());
        }
    }
}

/// Unread mail that was received (any category but sent mail).
fn unread_received(unread: &[i32; 4]) -> i32 {
    unread[1..].iter().map(|n| (*n).max(0)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::sc_activity::{MailBodyInfo, MailHeader, TakenItem};

    fn header(id: i64, ty: u8, attachments: u8) -> Box<MailHeader> {
        Box::new(MailHeader {
            mail_id: id,
            mail_type: ty,
            status: 0,
            title: "".into(),
            sender: "".into(),
            attachments,
            receiver: "".into(),
            open_date: 0,
            returned: false,
            extra: 0,
        })
    }

    fn got(id: i64, ty: u8, attachments: u8) -> ActivityEvent {
        ActivityEvent::MailReceived {
            mail: header(id, ty, attachments),
            unread: [0, 1, 0, 0],
            is_cancel: false,
            body: None,
        }
    }

    fn body(id: i64, ty: u8, money: i32, items: usize) -> ActivityEvent {
        ActivityEvent::MailBody {
            is_prepare: false,
            is_sent: false,
            body: Box::new(MailBodyInfo {
                mail_id: id,
                mail_type: ty,
                receiver: "".into(),
                title: "".into(),
                text: "".into(),
                money,
                billing: 0,
                money2: 0,
                send_date: 0,
                recv_date: 0,
                open_date: 0,
                items: vec![Default::default(); items],
            }),
            open_date_modified: true,
            unread: [0, 0, 0, 0],
        }
    }

    fn taken(id: i64, money: bool, items: usize) -> ActivityEvent {
        ActivityEvent::AttachmentTaken {
            mail_id: id,
            money,
            aa_point: false,
            take_sequentially: true,
            items: (0..items)
                .map(|i| TakenItem {
                    id: 100 + i as u64,
                    slot_type: 1,
                    slot: i as u8,
                })
                .collect(),
        }
    }

    fn packets(t: &mut MailTracker, now: u64, awaiting: bool) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        t.tick(now, awaiting, &mut out);
        out.into_iter().map(|o| (o.opcode, o.body)).collect()
    }

    const READ: u16 = aa_proto::opcodes::cs::CSReadMailPacket;
    const TAKE: u16 = aa_proto::opcodes::cs::CSTakeAttachmentSequentially;
    const LIST: u16 = aa_proto::opcodes::cs::CSListMailPacket;

    #[test]
    fn the_kind_follows_the_type_and_the_items() {
        assert_eq!(Kind::of(14, 0), Some(Kind::Sold));
        assert_eq!(Kind::of(13, 1), Some(Kind::Cancelled));
        assert_eq!(Kind::of(15, 1), Some(Kind::Expired));
        assert_eq!(Kind::of(15, 0), Some(Kind::Refund));
        assert_eq!(Kind::of(16, 1), Some(Kind::Won));
        assert_eq!(Kind::of(19, 0), None);
        assert_eq!(Kind::of(1, 3), None);
        assert!(Kind::Sold.is_seller() && Kind::Expired.is_seller() && !Kind::Won.is_seller());
    }

    #[test]
    fn a_sale_mail_is_read_and_taken() {
        let mut t = MailTracker::default();
        t.observe(&got(5, 14, 0), 1_000);
        assert_eq!((t.collecting(), t.seller_mails), (1, 1));
        // read
        let p = packets(&mut t, 1_000, false);
        assert_eq!(p.len(), 1);
        assert_eq!(
            (p[0].0, p[0].1.clone()),
            (READ, [vec![0], 5i64.to_le_bytes().to_vec()].concat())
        );
        // nothing while the body is pending
        assert!(packets(&mut t, 2_000, false).is_empty());
        // the body: 36 copper -> take
        t.observe(&body(5, 14, 36, 0), 2_100);
        let p = packets(&mut t, 2_100, false);
        assert_eq!(
            (p[0].0, p[0].1.clone()),
            (TAKE, 5i64.to_le_bytes().to_vec())
        );
        // taken
        t.observe(&taken(5, true, 0), 2_500);
        assert_eq!(t.collecting(), 0);
        let s = t.take_settled().unwrap();
        assert_eq!((s.kind, s.money, s.items), (Kind::Sold, 36, 0));
        assert_eq!((t.counts.sold, t.counts.earned), (1, 36));
        assert!(t.take_settled().is_none());
        // a repeated announcement of a finished mail is ignored
        t.observe(&got(5, 14, 0), 3_000);
        assert_eq!((t.collecting(), t.seller_mails), (0, 1));
    }

    #[test]
    fn items_won_and_returned_are_collected() {
        let mut t = MailTracker::default();
        t.observe(&got(7, 16, 1), 0);
        t.observe(&got(8, 15, 1), 0);
        t.observe(&got(9, 15, 0), 0);
        assert_eq!((t.seller_mails, t.win_mails), (1, 1));
        for id in [7, 8, 9] {
            let p = packets(&mut t, 0, false);
            assert_eq!(p[0].0, READ);
            let (money, items) = if id == 9 { (50, 0) } else { (0, 1) };
            let ty = if id == 7 { 16 } else { 15 };
            t.observe(&body(id, ty, money, items), 0);
            let p = packets(&mut t, 0, false);
            assert_eq!(p[0].0, TAKE);
            t.observe(&taken(id, money > 0, items), 0);
        }
        let c = t.counts;
        assert_eq!((c.won, c.expired, c.refunds), (1, 1, 1));
        assert_eq!(c.sold, 0);
        assert_eq!(t.collecting(), 0);
    }

    #[test]
    fn requests_are_repeated_and_then_given_up_on() {
        let mut t = MailTracker::default();
        t.observe(&got(5, 14, 0), 0);
        assert_eq!(packets(&mut t, 0, false)[0].0, READ);
        // no body: read again after the retry time, up to three requests in all
        assert!(packets(&mut t, 3_000, false).is_empty());
        assert_eq!(packets(&mut t, 4_000, false)[0].0, READ);
        assert_eq!(packets(&mut t, 8_000, false)[0].0, READ);
        assert!(packets(&mut t, 12_000, false).is_empty());
        assert_eq!(t.collecting(), 0, "given up");
        assert_eq!(t.take_fault(), Some(Fault::Unanswered(Kind::Sold)));
        assert_eq!(t.take_fault(), None);
        assert_eq!(t.counts.sold, 0);
    }

    #[test]
    fn an_emptied_mail_is_dropped_quietly_unless_the_header_promised_items() {
        let mut t = MailTracker::default();
        // listed again after it was taken in an earlier session
        t.observe(&got(5, 14, 0), 0);
        packets(&mut t, 0, false);
        t.observe(&body(5, 14, 0, 0), 0);
        packets(&mut t, 0, false);
        assert_eq!(t.collecting(), 0);
        assert_eq!(t.take_fault(), None);
        // the header announced an item but the body has none
        t.observe(&got(6, 13, 1), 0);
        packets(&mut t, 0, false);
        t.observe(&body(6, 13, 0, 0), 0);
        packets(&mut t, 0, false);
        assert_eq!(t.take_fault(), Some(Fault::Empty(Kind::Cancelled)));
    }

    #[test]
    fn a_mail_with_money_and_an_item_takes_two_requests() {
        let mut t = MailTracker::default();
        t.observe(&got(5, 14, 1), 0);
        packets(&mut t, 0, false);
        t.observe(&body(5, 14, 10, 1), 0);
        assert_eq!(packets(&mut t, 0, false)[0].0, TAKE);
        t.observe(&taken(5, false, 1), 100);
        assert_eq!(t.collecting(), 1, "the money is still in it");
        // (the next request goes out at once: the body is known)
        let p = packets(&mut t, 100, false);
        assert_eq!(p[0].0, TAKE);
        t.observe(&taken(5, true, 0), 200);
        assert_eq!(t.collecting(), 0);
        assert_eq!(t.counts.earned, 10);
    }

    #[test]
    fn the_mailbox_is_listed_when_asked_for_unread_mail_and_while_waiting() {
        let mut t = MailTracker::default();
        assert!(packets(&mut t, 0, false).is_empty());
        t.want_list();
        let p = packets(&mut t, 10, false);
        assert_eq!(p[0].0, LIST);
        assert!(t.listed_since(10) && !t.listed_since(11));
        assert!(packets(&mut t, 20, false).is_empty());
        // unread mail at login: once
        t.observe(
            &ActivityEvent::CountUnreadMail {
                unread: [0, 2, 0, 0],
            },
            30,
        );
        assert_eq!(packets(&mut t, 30, false)[0].0, LIST);
        assert!(packets(&mut t, 40, false).is_empty());
        // results outstanding: every five minutes
        assert!(packets(&mut t, 200_000, true).is_empty());
        assert_eq!(packets(&mut t, 330_000, true)[0].0, LIST);
        // not while a mail is being collected
        t.observe(&got(1, 14, 0), 400_000);
        t.want_list();
        assert_eq!(packets(&mut t, 400_000, false)[0].0, READ);
    }

    #[test]
    fn a_listing_announces_old_mail_and_only_auction_mail() {
        let mut t = MailTracker::default();
        let list = ActivityEvent::MailList {
            is_sent: false,
            mails: vec![
                *header(1, 14, 0),
                *header(2, 19, 0),
                *header(3, 1, 0),
                *header(4, 15, 1),
            ],
        };
        assert!(MailTracker::concerns(&list));
        t.observe(&list, 0);
        assert_eq!((t.collecting(), t.seller_mails), (2, 2));
        let sent = ActivityEvent::MailList {
            is_sent: true,
            mails: vec![*header(5, 14, 0)],
        };
        assert!(!MailTracker::concerns(&sent));
        t.observe(&sent, 0);
        assert_eq!(t.collecting(), 2);
        assert!(!MailTracker::concerns(&got(1, 19, 0)));
        assert!(MailTracker::concerns(&got(1, 16, 1)));
    }

    #[test]
    fn a_withdrawn_or_deleted_mail_is_forgotten() {
        let mut t = MailTracker::default();
        t.observe(&got(1, 14, 0), 0);
        t.observe(&got(2, 14, 0), 0);
        t.observe(
            &ActivityEvent::MailReceived {
                mail: header(1, 14, 0),
                unread: [0, 0, 0, 0],
                is_cancel: true,
                body: None,
            },
            0,
        );
        assert_eq!(t.collecting(), 1);
        t.observe(
            &ActivityEvent::MailDeleted {
                is_sent: false,
                mail_id: 2,
                unread_modified: true,
                unread: [0, 0, 0, 0],
            },
            0,
        );
        assert_eq!(t.collecting(), 0);
    }

    #[test]
    fn the_queue_and_the_memory_are_bounded() {
        let mut t = MailTracker::default();
        for id in 0..40 {
            t.observe(&got(id, 14, 0), 0);
        }
        assert_eq!(t.collecting(), MAX_DUE);
        for id in 0..40 {
            t.observe(&got(1000 + id, 14, 0), 0);
            let mut out = Vec::new();
            t.tick(0, false, &mut out);
        }
        assert!(t.done.len() <= MAX_DONE);
    }
}
