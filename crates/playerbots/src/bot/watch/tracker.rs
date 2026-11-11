//! The expectation tracker: "a request went out, the server must react by a deadline".
//!
//! Some requests are always answered by the server, if only with a refusal: accepting or turning
//! in a quest, opening a loot window, casting a skill (`SCSkillStarted` carries the refusal; an
//! accepted instant or plot skill is answered by `SCSkillFired` / `SCPlotEvent`), a GM
//! `/move` (`SCTeleportUnit`). When the answer does not come, either the server dropped the request
//! silently or the connection is gone; the second case is the stall detector's, the first is a
//! [`Finding::NoResponse`](super::finding::Finding).
//!
//! One [`Tracker`] per bot: a table of [`CAPACITY`] fixed-size slots (no allocation, a
//! housekeeping pass of at most eight comparisons). The bot loop registers a request from the
//! packet it is about to send ([`Tracker::on_sent`]), so every goal is covered without knowing
//! about the tracker; a goal that sends something the opcode does not tell can register it
//! itself through [`Ctx::expect`](crate::bot::ctx::Ctx::expect).
//!
//! A request is **not** reported when
//! * the bot died, or the server teleported it (the world changed under the request),
//! * no packet at all came from the server after the request (the connection is stalled: that is
//!   a [`ServerStall`](crate::issues::IssueKind::ServerStall), not a lost request),
//! * the bot walked away from where it asked (only for [`Req::moved_away_matters`]).

use aa_proto::cs::Out;
use aa_proto::game::LEVEL_GAME;
use aa_proto::opcodes::{cs, sc};
use aa_proto::sc::ServerEvent;
use aa_proto::sc_activity::ActivityEvent;
use aa_proto::sc_world::WorldEvent;
use smallvec::SmallVec;

/// Requests tracked at once; a ninth is dropped.
pub const CAPACITY: usize = 8;
/// Skills below this id are the default attacks (Melee Attack 2, Ranged Attack 4, ...). The server
/// starts them as an automatic attack that repeats by itself, and a request for the one that is
/// already running is accepted with no packet at all (`start_skill`: `is_auto && auto.skill ==
/// skill_id`), so silence is its answer and nothing to track.
pub const AUTO_ATTACK_BELOW: u32 = 5_000;
/// The server has this long to react. Longer than the keep-alive interval, so a healthy
/// connection delivers at least one packet (a pong) in the window.
pub const WAIT_MS: u32 = 6_000;
/// The bot moved this far (metres) from where it asked: the request is forgotten.
pub const MOVED_AWAY_M: f32 = 12.0;

/// What kind of request is waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Req {
    /// `CSStartQuestContext`; `id` = quest. Answer: started or refused.
    QuestAccept = 1,
    /// `CSCompleteQuestContext`; `id` = quest. Answer: completed, rewarded by mail or refused.
    QuestComplete = 2,
    /// `CSLootOpenBag`; `id` = the corpse's object id, `aux` its template. Answer: the loot
    /// window, a refusal or "nothing to loot".
    LootOpen = 3,
    /// `CSStartSkill`; `id` = skill. Answer: `SCSkillStarted` (its result is the refusal), or for
    /// an instant skill `SCSkillFired`, or for a plot skill `SCPlotEvent` (see [`cast_proof`]).
    SkillCast = 4,
    /// A GM `/move`. Answer: `SCTeleportUnit` or `SCLoadInstance` (or a chat line saying no).
    Teleport = 5,
}

impl Req {
    /// A short name for keys and titles.
    pub fn name(self) -> &'static str {
        match self {
            Req::QuestAccept => "quest_accept",
            Req::QuestComplete => "quest_complete",
            Req::LootOpen => "loot_open",
            Req::SkillCast => "skill_cast",
            Req::Teleport => "teleport",
        }
    }

    /// The server judges the request by where the bot stands, so a bot that moved away
    /// (knocked back, teleported by something else) may have had it ignored.
    pub fn moved_away_matters(self) -> bool {
        matches!(self, Req::QuestAccept | Req::QuestComplete | Req::LootOpen)
    }

    /// Whether `ev` answers a request of this kind for `id`. `me_obj` is the bot's object id.
    pub fn answered_by(self, id: u32, ev: &ServerEvent, me_obj: u32) -> bool {
        match (self, ev) {
            (Req::QuestAccept, ServerEvent::Activity(a)) => match a {
                ActivityEvent::QuestStarted { quest, .. } => quest.template == id,
                ActivityEvent::QuestFailed { quest_id, .. }
                | ActivityEvent::QuestUnitReqFailed { quest_id, .. } => *quest_id == id,
                ActivityEvent::Quests { list } => list.iter().any(|q| q.template == id),
                _ => false,
            },
            (Req::QuestComplete, ServerEvent::Activity(a)) => match a {
                ActivityEvent::QuestCompleted { quest_id, .. }
                | ActivityEvent::QuestFailed { quest_id, .. }
                | ActivityEvent::QuestReset { quest_id, .. } => *quest_id == id,
                ActivityEvent::QuestRewardedByMail { quests } => quests.contains(&id),
                _ => false,
            },
            (Req::LootOpen, ServerEvent::World(w)) => match w {
                WorldEvent::LootBag { owner_obj, .. }
                | WorldEvent::LootFailed { owner_obj, .. }
                | WorldEvent::LootTook { owner_obj, .. } => *owner_obj == id,
                // the corpse has nothing (more) to loot: that is an answer too
                WorldEvent::LootableState {
                    owner_obj,
                    has_loot: false,
                    ..
                } => *owner_obj == id,
                _ => false,
            },
            (
                Req::SkillCast,
                ServerEvent::World(WorldEvent::SkillStarted {
                    caster_obj, skill, ..
                }),
            ) => me_obj != 0 && *caster_obj == me_obj && *skill == id,
            (Req::Teleport, ServerEvent::World(w)) => {
                matches!(
                    w,
                    WorldEvent::TeleportUnit { .. }
                    | WorldEvent::LoadInstance { .. }
                    // a GM command without the right gets a chat line or an error message
                    | WorldEvent::ErrorMsg { .. }
                ) || matches!(w, WorldEvent::Chat { kind, .. } if (-2..=-1).contains(kind))
            }
            _ => false,
        }
    }
}

/// The skill and the caster's object id of a server packet that proves a cast was accepted although
/// no `SCSkillStarted` came: the server sends that one only for a cast with a casting time (and for
/// refusals). An *instant* skill is announced by `SCSkillFired` alone, and a *plot* skill by its
/// `SCPlotEvent`s. The world parser has no use for either, so the bot loop reads them here, from
/// the raw body: no allocation, three fixed offsets.
///
/// `SCSkillFired`: `u32 skill, u16 tl, u8 caster kind, bc caster object, ...`.
/// `SCPlotEvent`: `u16 tl, u32 event, u32 skill, u8 caster kind, bc caster object, ...`
/// (G/systems/skills/packets.rs:135 and :538).
pub fn cast_proof(level: u8, opcode: u16, body: &[u8]) -> Option<(u32, u32)> {
    if level != LEVEL_GAME {
        return None;
    }
    let (skill_at, caster_at) = match opcode {
        sc::SCSkillFiredPacket => (0, 7),
        sc::SCPlotEventPacket => (6, 11),
        _ => return None,
    };
    let skill = u32::from_le_bytes(*body.get(skill_at..skill_at + 4)?.first_chunk::<4>()?);
    let b = body.get(caster_at..caster_at + 3)?;
    let caster = u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
    Some((skill, caster))
}

/// One waiting request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pending {
    /// What was asked.
    pub req: Req,
    /// The quest, skill or object it is about.
    pub id: u32,
    /// A second id for the report (the corpse's template for [`Req::LootOpen`]).
    pub aux: u32,
    /// Bot clock (ms) of the request.
    pub sent_ms: u64,
    /// How long the server has.
    pub wait_ms: u32,
    /// Where the bot stood.
    pub pos: [f32; 2],
}

impl Pending {
    /// The bot clock at which the request is overdue.
    pub fn due_ms(&self) -> u64 {
        self.sent_ms + u64::from(self.wait_ms)
    }
}

/// The table of waiting requests. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    slots: [Option<Pending>; CAPACITY],
    used: u8,
}

impl Tracker {
    /// Whether nothing waits (the common case: the bot loop skips all matching then).
    pub fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Requests waiting.
    pub fn len(&self) -> usize {
        usize::from(self.used)
    }

    /// The waiting requests.
    pub fn pending(&self) -> impl Iterator<Item = &Pending> {
        self.slots.iter().flatten()
    }

    /// Registers a request. One already waiting for the same `(req, id)` keeps its deadline (a
    /// retry does not buy the server more time). Returns whether it was stored.
    pub fn expect(&mut self, p: Pending) -> bool {
        if self
            .slots
            .iter()
            .flatten()
            .any(|q| q.req == p.req && q.id == p.id)
        {
            return true;
        }
        match self.slots.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some(p);
                self.used += 1;
                true
            }
            None => false,
        }
    }

    /// Registers the request a packet about to be sent makes, if it is one that is tracked.
    /// `units_template` maps a corpse's object id to its NPC template (0 when unknown).
    /// Returns whether a request was stored.
    pub fn on_sent(
        &mut self,
        out: &Out,
        now_ms: u64,
        pos: [f32; 3],
        units_template: impl FnOnce(u32) -> u32,
    ) -> bool {
        if out.level != LEVEL_GAME {
            return false;
        }
        let (req, id, aux) = match out.opcode {
            cs::CSStartQuestContextPacket => (Req::QuestAccept, le_u32(&out.body), 0),
            cs::CSCompleteQuestContextPacket => (Req::QuestComplete, le_u32(&out.body), 0),
            cs::CSStartSkillPacket => {
                let skill = le_u32(&out.body);
                if skill < AUTO_ATTACK_BELOW {
                    // (the server accepts the auto attack that already runs without a word)
                    return false;
                }
                (Req::SkillCast, skill, 0)
            }
            cs::CSLootOpenBagPacket => {
                let obj = le_u24(&out.body);
                (Req::LootOpen, obj, units_template(obj))
            }
            _ => return false,
        };
        if id == 0 {
            return false;
        }
        self.expect(Pending {
            req,
            id,
            aux,
            sent_ms: now_ms,
            wait_ms: WAIT_MS,
            pos: [pos[0], pos[1]],
        })
    }

    /// A cast of the bot's own started without an `SCSkillStarted` (see [`cast_proof`]): forgets
    /// the cast request of that skill. Returns how many.
    pub fn on_cast_proof(&mut self, skill: u32, caster_obj: u32, me_obj: u32) -> usize {
        if self.used == 0 || me_obj == 0 || caster_obj != me_obj {
            return 0;
        }
        let mut cleared = 0;
        for slot in &mut self.slots {
            if slot.is_some_and(|p| p.req == Req::SkillCast && p.id == skill) {
                *slot = None;
                cleared += 1;
            }
        }
        self.used -= cleared as u8;
        cleared
    }

    /// An event arrived: forgets the requests it answers. Returns how many.
    pub fn on_event(&mut self, ev: &ServerEvent, me_obj: u32) -> usize {
        if self.used == 0 {
            return 0;
        }
        let mut cleared = 0;
        for slot in &mut self.slots {
            if slot.is_some_and(|p| p.req.answered_by(p.id, ev, me_obj)) {
                *slot = None;
                cleared += 1;
            }
        }
        self.used -= cleared as u8;
        cleared
    }

    /// Forgets everything (death, a new connection).
    pub fn clear(&mut self) {
        self.slots = [None; CAPACITY];
        self.used = 0;
    }

    /// Forgets every request that is not a teleport (the server moved the bot).
    pub fn forget_all_but_teleports(&mut self) {
        for slot in &mut self.slots {
            if slot.is_some_and(|p| p.req != Req::Teleport) {
                *slot = None;
                self.used -= 1;
            }
        }
    }

    /// Takes the overdue requests out of the table at `now_ms`. A request is dropped without a
    /// report when the bot moved away from where it asked (for the kinds it matters for) or when
    /// the server sent nothing at all since `last_rx_ms` was before the request (a stall). The rest
    /// are returned for a report.
    pub fn take_due(
        &mut self,
        now_ms: u64,
        pos: [f32; 3],
        last_rx_ms: u64,
    ) -> SmallVec<[Pending; 2]> {
        let mut due = SmallVec::new();
        if self.used == 0 {
            return due;
        }
        for slot in &mut self.slots {
            let Some(p) = *slot else { continue };
            if now_ms < p.due_ms() {
                continue;
            }
            *slot = None;
            self.used -= 1;
            let moved = p.req.moved_away_matters()
                && dist2(p.pos, [pos[0], pos[1]]) > MOVED_AWAY_M * MOVED_AWAY_M;
            let heard_since = last_rx_ms > p.sent_ms;
            if !moved && heard_since {
                due.push(p);
            }
        }
        due
    }
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)
}

fn le_u32(body: &[u8]) -> u32 {
    body.get(..4)
        .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// A `bc` (three bytes, little endian) at the start of `body`.
fn le_u24(body: &[u8]) -> u32 {
    body.get(..3).map_or(0, |b| {
        u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::cs as csb;
    use aa_proto::sc_activity::QuestRow;

    fn row(template: u32) -> Box<QuestRow> {
        Box::new(QuestRow {
            id: 1,
            template,
            status: 1,
            objectives: [0; 5],
            left_time_ms: -1,
            component: 0,
            acceptor_type: 0,
            acceptor_id: 0,
        })
    }

    fn pending(req: Req, id: u32, sent_ms: u64) -> Pending {
        Pending {
            req,
            id,
            aux: 0,
            sent_ms,
            wait_ms: WAIT_MS,
            pos: [10.0, 10.0],
        }
    }

    #[test]
    fn the_default_attacks_are_not_tracked_because_a_repeat_is_accepted_in_silence() {
        let mut t = Tracker::default();
        let pos = [1.0, 2.0, 3.0];
        for auto in [2, 4, 4_999] {
            assert!(!t.on_sent(&csb::start_skill_unit(auto, 9, 12), 100, pos, |_| 0));
        }
        assert!(t.is_empty());
        assert!(t.on_sent(
            &csb::start_skill_unit(AUTO_ATTACK_BELOW, 9, 12),
            100,
            pos,
            |_| 0
        ));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn requests_are_read_from_the_packets_that_make_them() {
        let mut t = Tracker::default();
        let pos = [1.0, 2.0, 3.0];
        assert!(t.on_sent(&csb::start_quest_context(251, 77, 0, 0), 100, pos, |_| 0));
        assert!(t.on_sent(&csb::complete_quest_context(300, 0, 5, 0), 100, pos, |_| 0));
        assert!(t.on_sent(&csb::start_skill_unit(14_001, 9, 12), 100, pos, |_| 0));
        assert!(
            t.on_sent(&csb::loot_open_bag(4_321, true), 100, pos, |obj| {
                assert_eq!(obj, 4_321);
                3_475
            })
        );
        let got: Vec<_> = t.pending().map(|p| (p.req, p.id, p.aux)).collect();
        assert_eq!(
            got,
            vec![
                (Req::QuestAccept, 251, 0),
                (Req::QuestComplete, 300, 0),
                (Req::SkillCast, 14_001, 0),
                (Req::LootOpen, 4_321, 3_475),
            ]
        );
        // packets nobody waits for are not tracked
        assert!(!t.on_sent(&csb::ping(5), 100, pos, |_| 0));
        assert!(!t.on_sent(&csb::change_target(9), 100, pos, |_| 0));
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn a_retry_keeps_the_first_deadline_and_a_full_table_drops_new_requests() {
        let mut t = Tracker::default();
        assert!(t.expect(pending(Req::QuestAccept, 5, 1_000)));
        assert!(t.expect(pending(Req::QuestAccept, 5, 4_000)));
        assert_eq!(t.len(), 1);
        assert_eq!(t.pending().next().unwrap().sent_ms, 1_000);
        for q in 0..CAPACITY as u32 + 3 {
            t.expect(pending(Req::SkillCast, 100 + q, 1_000));
        }
        assert_eq!(t.len(), CAPACITY);
        assert!(!t.expect(pending(Req::Teleport, 0, 1_000)));
    }

    #[test]
    fn the_matching_answer_clears_the_request() {
        let mut t = Tracker::default();
        t.expect(pending(Req::QuestAccept, 251, 0));
        t.expect(pending(Req::QuestComplete, 300, 0));
        t.expect(pending(Req::SkillCast, 14_001, 0));
        t.expect(pending(Req::LootOpen, 4_321, 0));
        let started = ServerEvent::Activity(ActivityEvent::QuestStarted {
            quest: row(251),
            component: 1,
        });
        assert_eq!(t.on_event(&started, 9), 1);
        // another quest's answer does not
        let other = ServerEvent::Activity(ActivityEvent::QuestFailed {
            quest_id: 999,
            err: 2,
        });
        assert_eq!(t.on_event(&other, 9), 0);
        // a refusal is an answer
        let refused = ServerEvent::Activity(ActivityEvent::QuestFailed {
            quest_id: 300,
            err: 12,
        });
        assert_eq!(t.on_event(&refused, 9), 1);
        // a skill start of another unit does not answer, the bot's own does (even a refusal)
        let foreign = ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj: 77,
            skill: 14_001,
            tl: 1,
            target_obj: 0,
            result: 0,
        });
        assert_eq!(t.on_event(&foreign, 9), 0);
        let refusal = ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj: 9,
            skill: 14_001,
            tl: 0,
            target_obj: 0,
            result: 8,
        });
        assert_eq!(t.on_event(&refusal, 9), 1);
        // an empty corpse answers a loot request
        let empty = ServerEvent::World(WorldEvent::LootableState {
            owner_obj: 4_321,
            owner_type: 1,
            has_loot: false,
        });
        assert_eq!(t.on_event(&empty, 9), 1);
        assert!(t.is_empty());
        // nothing waits: matching is skipped altogether
        assert_eq!(t.on_event(&started, 9), 0);
    }

    /// A server body: `skill` first as `SCSkillFired` has it.
    fn fired_body(skill: u32, caster: u32) -> Vec<u8> {
        let mut b = skill.to_le_bytes().to_vec();
        b.extend(7u16.to_le_bytes()); // tl
        b.push(0); // caster kind: unit
        b.extend(&caster.to_le_bytes()[..3]);
        b.extend([0; 20]);
        b
    }

    fn plot_body(skill: u32, caster: u32) -> Vec<u8> {
        let mut b = 7u16.to_le_bytes().to_vec(); // tl
        b.extend(1u32.to_le_bytes()); // event
        b.extend(skill.to_le_bytes());
        b.push(0);
        b.extend(&caster.to_le_bytes()[..3]);
        b.extend([0; 20]);
        b
    }

    #[test]
    fn an_instant_or_plot_cast_is_answered_without_a_skill_started() {
        // (the server sends SCSkillStarted only for casts with a casting time and for refusals)
        assert_eq!(
            cast_proof(
                LEVEL_GAME,
                sc::SCSkillFiredPacket,
                &fired_body(18_132, 0x1234)
            ),
            Some((18_132, 0x1234))
        );
        assert_eq!(
            cast_proof(
                LEVEL_GAME,
                sc::SCPlotEventPacket,
                &plot_body(10_752, 0x0a0b0c)
            ),
            Some((10_752, 0x0a0b0c))
        );
        // another packet, a proxy-level packet, a short body: nothing
        assert_eq!(
            cast_proof(LEVEL_GAME, sc::SCSkillEndedPacket, &fired_body(1, 2)),
            None
        );
        assert_eq!(
            cast_proof(2, sc::SCSkillFiredPacket, &fired_body(1, 2)),
            None
        );
        assert_eq!(
            cast_proof(LEVEL_GAME, sc::SCSkillFiredPacket, &[1, 2, 3, 4, 5]),
            None
        );
        assert_eq!(
            cast_proof(LEVEL_GAME, sc::SCPlotEventPacket, &plot_body(1, 2)[..12]),
            None
        );

        let mut t = Tracker::default();
        let cast = |skill| Pending {
            req: Req::SkillCast,
            id: skill,
            aux: 0,
            sent_ms: 0,
            wait_ms: 6_000,
            pos: [0.0; 2],
        };
        assert!(t.expect(cast(18_132)));
        assert!(t.expect(cast(10_752)));
        // somebody else's cast, or another skill, answers nothing
        assert_eq!(t.on_cast_proof(18_132, 0x77, 9), 0);
        assert_eq!(t.on_cast_proof(5, 9, 9), 0);
        assert_eq!(t.on_cast_proof(18_132, 9, 9), 1);
        assert_eq!(t.on_cast_proof(10_752, 9, 9), 1);
        assert!(t.is_empty());
        // a bot without an object id of its own is nobody
        assert!(t.expect(cast(1)));
        assert_eq!(t.on_cast_proof(1, 0, 0), 0);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn a_gm_move_is_answered_by_a_teleport_or_by_a_refusing_chat_line() {
        for ev in [
            WorldEvent::TeleportUnit {
                reason: 0,
                error: 0,
                pos: (1.0, 2.0, 3.0),
            },
            WorldEvent::ErrorMsg {
                err: 1,
                err2: 1,
                kind: 0,
                notify: false,
            },
            WorldEvent::Chat {
                kind: -2,
                sender_obj: 0,
                sender_id: 0,
                name: "".into(),
                msg: "no permission".into(),
            },
        ] {
            let mut t = Tracker::default();
            t.expect(pending(Req::Teleport, 0, 0));
            assert_eq!(t.on_event(&ServerEvent::World(ev), 9), 1);
        }
        let mut t = Tracker::default();
        t.expect(pending(Req::Teleport, 0, 0));
        let say = ServerEvent::World(WorldEvent::Chat {
            kind: 0,
            sender_obj: 4,
            sender_id: 4,
            name: "x".into(),
            msg: "hello".into(),
        });
        assert_eq!(t.on_event(&say, 9), 0, "local chat is not an answer");
    }

    #[test]
    fn overdue_requests_are_reported_unless_the_bot_or_the_connection_explains_them() {
        let mut t = Tracker::default();
        t.expect(pending(Req::QuestAccept, 1, 1_000));
        t.expect(pending(Req::SkillCast, 2, 1_000));
        t.expect(pending(Req::LootOpen, 3, 1_000));
        t.expect(pending(Req::QuestComplete, 4, 5_000));
        // before the deadline nothing is due
        assert!(t.take_due(6_999, [10.0, 10.0, 0.0], 6_000).is_empty());
        // at the deadline: the server was talking since the requests (6_500 > 1_000), the bot
        // stands where it asked
        let due = t.take_due(7_000, [10.0, 10.0, 0.0], 6_500);
        let ids: Vec<_> = due.iter().map(|p| (p.req, p.id)).collect();
        assert_eq!(
            ids,
            vec![
                (Req::QuestAccept, 1),
                (Req::SkillCast, 2),
                (Req::LootOpen, 3)
            ]
        );
        assert_eq!(t.len(), 1, "the later request still waits");

        // the bot was carried 50 m away: a quest or loot request does not count, a cast does
        let mut t = Tracker::default();
        t.expect(pending(Req::QuestAccept, 1, 0));
        t.expect(pending(Req::SkillCast, 2, 0));
        let due = t.take_due(7_000, [60.0, 10.0, 0.0], 3_000);
        let ids: Vec<_> = due.iter().map(|p| p.req).collect();
        assert_eq!(ids, vec![Req::SkillCast]);

        // nothing at all came from the server since the request: a stall, not a lost request
        let mut t = Tracker::default();
        t.expect(pending(Req::SkillCast, 2, 1_000));
        assert!(t.take_due(7_000, [10.0, 10.0, 0.0], 900).is_empty());
        assert!(t.is_empty(), "the stall detector owns it now");
    }

    #[test]
    fn a_teleport_by_the_server_forgets_the_requests_but_not_the_teleport_itself() {
        let mut t = Tracker::default();
        t.expect(pending(Req::QuestAccept, 1, 0));
        t.expect(pending(Req::Teleport, 0, 0));
        t.forget_all_but_teleports();
        assert_eq!(
            t.pending().map(|p| p.req).collect::<Vec<_>>(),
            [Req::Teleport]
        );
        assert_eq!(t.len(), 1);
        t.clear();
        assert!(t.is_empty() && t.pending().next().is_none());
    }

    #[test]
    fn a_tracker_is_small_and_needs_no_heap() {
        assert!(std::mem::size_of::<Tracker>() <= 8 * 40);
    }
}
