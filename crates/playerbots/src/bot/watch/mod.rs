//! The detectors: how a bot notices that the *server* misbehaves.
//!
//! A bot plays like a player, so it can tell when a request is never answered, a quest gives no
//! credit, a monster takes no damage, a trade pays out wrong, the connection stalls or drops. The
//! process-wide plumbing for such reports is [`crate::issues`]; this module is the bot's side:
//!
//! * [`Watch`] is the per-bot state of the detectors (in [`BotState::watch`], one allocation per
//!   bot): the packet trail, the expectation [`tracker`], the connection and death watches and the
//!   list of findings waiting to be filed.
//! * [`classify`] holds the *judgement* as pure functions: which refusals are the bot's own fault,
//!   when a stall is sustained, when a counter that did not move is a missing credit, what a
//!   finished trade must have changed. Everything that could produce a false positive is decided
//!   there and tested there.
//! * [`finding`] turns a [`Finding`] (a `Copy` note a detector leaves) into the
//!   [`IssueReport`](crate::issues::IssueReport): key, title, expected/actual, suspect, severity.
//!
//! The flow: a detector in the state, a goal or the combat controller pushes a finding
//! ([`Watch::push`], once per episode); the bot loop drains them after every packet, tick and
//! housekeeping pass ([`Watch::take_findings`]) and files them with the packet trail attached. The
//! detectors in the bot loop itself (malformed packets, connection faults, stalls, the tracker's
//! overdue requests) file through the same path. Nothing here allocates per tick: the work per
//! packet is one ring write, per second a few comparisons.
//!
//! The rules every detector follows: report an *episode* once, not per tick or per packet; blame
//! the bot ([`Suspect::Bot`](crate::issues::Suspect)) when its own behaviour is the likelier cause
//! (such reports are kept in the web UI and never queued); give a key that holds ids only.

pub mod classify;
pub mod finding;
pub mod tracker;

use aa_proto::sc::ServerEvent;
use aa_proto::sc_world::WorldEvent;
use smallvec::SmallVec;

use classify::{DeathWatch, Stall};
pub use finding::{Finding, Names, PayFault, Role};
pub use tracker::{Pending, Req, Tracker};

use crate::issues::{Dir, IssueReport, TrailRing};

/// Findings kept between two drains; more are dropped (the same episode would repeat anyway).
const MAX_FINDINGS: usize = 6;
/// Episodes remembered (each is reported once per session).
const MAX_SEEN: usize = 24;
/// Distinct malformed packets remembered per session.
const MAX_MALFORMED: usize = 6;

/// What the bot knows about the health of its connection.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnWatch {
    /// Bot clock (ms) of the last packet of any kind; 0 before the first.
    last_rx_ms: u64,
    /// A pong that took too long, until the next check looks at it (0 = none).
    slow_rtt_ms: u64,
    /// A stall is on and was reported; it ends with a quick pong.
    stalled: bool,
    /// Bot clock of the last housekeeping pass.
    last_check_ms: u64,
    /// The bot's own task was starved until this bot clock.
    lag_until_ms: u64,
}

impl ConnWatch {
    /// A packet arrived.
    pub fn on_rx(&mut self, now_ms: u64) {
        self.last_rx_ms = now_ms;
    }

    /// Bot clock of the last packet.
    pub fn last_rx_ms(&self) -> u64 {
        self.last_rx_ms
    }

    /// A pong arrived `rtt_ms` after its ping left. A slow one is remembered for the next check; a
    /// quick one ends a stall episode.
    pub fn on_pong(&mut self, rtt_ms: u64) {
        if rtt_ms > classify::STALL_RTT_MS {
            self.slow_rtt_ms = rtt_ms;
        } else if rtt_ms < classify::HEALTHY_RTT_MS {
            self.stalled = false;
        }
    }

    /// The once-a-second pass: the stall of the connection, if one just became sustained.
    pub fn check(&mut self, now_ms: u64) -> Option<Stall> {
        if self.last_check_ms != 0
            && now_ms.saturating_sub(self.last_check_ms) > classify::LAG_GAP_MS
        {
            self.lag_until_ms = now_ms + classify::LAG_MEMORY_MS;
        }
        self.last_check_ms = now_ms;
        if self.last_rx_ms == 0 {
            self.last_rx_ms = now_ms;
        }
        let rtt = std::mem::take(&mut self.slow_rtt_ms);
        if self.stalled {
            return None;
        }
        let s = classify::stall(
            now_ms.saturating_sub(self.last_rx_ms),
            rtt,
            now_ms < self.lag_until_ms,
        )?;
        self.stalled = true;
        Some(s)
    }
}

/// The detectors' state of one bot. See the module docs.
#[derive(Debug, Default)]
pub struct Watch {
    /// The last packets sent and received.
    pub trail: TrailRing,
    /// Requests that wait for an answer.
    pub tracker: Tracker,
    /// The connection's health.
    pub conn: ConnWatch,
    /// The bot's own death.
    pub death: DeathWatch,
    /// The bot's last accepted skill start `(skill, bot clock)`.
    pub cast_ok: (u32, u64),
    findings: SmallVec<[Finding; MAX_FINDINGS]>,
    seen: SmallVec<[u64; MAX_SEEN]>,
    malformed: SmallVec<[u32; MAX_MALFORMED]>,
}

impl Watch {
    /// Notes a packet in the trail.
    pub fn packet(&mut self, now_ms: u64, dir: Dir, level: u8, opcode: u16) {
        self.trail.push(now_ms, dir, level, opcode);
    }

    /// Looks at a raw server packet the world parser skips: an instant or plot skill of the bot is
    /// accepted without an `SCSkillStarted` (see [`tracker::cast_proof`]).
    pub fn on_raw(&mut self, level: u8, opcode: u16, body: &[u8], me_obj: u32, now_ms: u64) {
        if let Some((skill, caster)) = tracker::cast_proof(level, opcode, body) {
            if me_obj != 0 && caster == me_obj {
                self.cast_ok = (skill, now_ms);
                self.tracker.on_cast_proof(skill, caster, me_obj);
            }
        }
    }

    /// Feeds a decoded server event to the detectors that follow the event stream.
    pub fn observe(&mut self, ev: &ServerEvent, me_obj: u32, now_ms: u64) {
        if let ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj,
            skill,
            result: 0,
            ..
        }) = ev
        {
            if *caster_obj == me_obj && me_obj != 0 {
                self.cast_ok = (*skill, now_ms);
            }
        }
        self.tracker.on_event(ev, me_obj);
        if let ServerEvent::World(
            WorldEvent::TeleportUnit { .. } | WorldEvent::LoadInstance { .. },
        ) = ev
        {
            // the server moved the bot: requests made from the old place are moot
            self.tracker.forget_all_but_teleports();
        }
    }

    /// Leaves a finding for the bot loop to file. Each episode ([`Finding::episode_key`]) is
    /// kept once per session; the rest, and any beyond the queue's size, are dropped.
    pub fn push(&mut self, finding: Finding) {
        let key = finding.episode_key();
        if self.seen.contains(&key) || self.findings.len() >= MAX_FINDINGS {
            return;
        }
        if self.seen.len() >= MAX_SEEN {
            self.seen.remove(0);
        }
        self.seen.push(key);
        self.findings.push(finding);
    }

    /// Whether findings wait to be filed.
    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }

    /// Takes the waiting findings.
    pub fn take_findings(&mut self) -> SmallVec<[Finding; MAX_FINDINGS]> {
        std::mem::take(&mut self.findings)
    }

    /// Whether this malformed packet (`level`, `opcode`) is a new one for this session.
    pub fn new_malformed(&mut self, level: u8, opcode: u16) -> bool {
        let key = u32::from(level) << 16 | u32::from(opcode);
        if self.malformed.contains(&key) {
            return false;
        }
        if self.malformed.len() >= MAX_MALFORMED {
            self.malformed.remove(0);
        }
        self.malformed.push(key);
        true
    }

    /// Adds the packet trail to `report` unless the detector set one.
    pub fn attach_trail(&self, mut report: IssueReport, now_ms: u64) -> IssueReport {
        if report.trail.is_empty() {
            report.trail = self.trail.snapshot(now_ms);
        }
        report
    }
}

impl Finding {
    /// A number that is the same for every occurrence of the same problem and differs between
    /// problems: what makes a session report an episode once.
    pub fn episode_key(&self) -> u64 {
        let (tag, a, b): (u64, u64, u64) = match *self {
            Finding::NoResponse { req, id, aux, .. } => {
                (1, req as u64, u64::from(id) << 32 | u64::from(aux))
            }
            Finding::QuestCreditMissing {
                quest, component, ..
            } => (2, u64::from(quest), u64::from(component)),
            Finding::QuestRefused {
                quest,
                code,
                turn_in,
            } => (
                3,
                u64::from(quest),
                u64::from(code) | u64::from(turn_in) << 8,
            ),
            Finding::RoleMissing {
                template,
                doodad,
                role,
                ..
            } => (
                4,
                u64::from(template),
                u64::from(doodad) | (role as u64) << 1,
            ),
            Finding::TargetImmune { npc, .. } => (5, u64::from(npc), 0),
            Finding::SkillRefused { skill, code } => (6, u64::from(skill), u64::from(code)),
            Finding::Death(d) => (7, d as u64, 0),
            Finding::Trade(f) => match f {
                classify::TradeFault::Money { .. } => (8, 0, 0),
                classify::TradeFault::ItemsIn { template, .. } => (8, 1, u64::from(template)),
                classify::TradeFault::ItemsOut { .. } => (8, 2, 0),
            },
            Finding::Payment { fault } => (9, fault as u64, 0),
            Finding::RoleUnreachable {
                template,
                doodad,
                role,
                ..
            } => (
                10,
                u64::from(template),
                u64::from(doodad) | (role as u64) << 1,
            ),
            Finding::QuestUnitReq { quest, kind, value } => (
                11,
                u64::from(quest),
                u64::from(kind) << 32 | u64::from(value),
            ),
        };
        tag << 56 ^ a.rotate_left(24) ^ b.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use classify::Death;

    #[test]
    fn an_episode_is_kept_once_per_session_and_the_queue_is_bounded() {
        let mut w = Watch::default();
        let f = Finding::SkillRefused {
            skill: 14_001,
            code: 0x1E,
        };
        w.push(f);
        w.push(f);
        assert!(w.has_findings());
        assert_eq!(w.take_findings().len(), 1);
        assert!(!w.has_findings());
        // the same problem is not reported again in this session ...
        w.push(f);
        assert!(!w.has_findings());
        // ... another code of the same skill is another problem
        w.push(Finding::SkillRefused {
            skill: 14_001,
            code: 0x2C,
        });
        assert_eq!(w.take_findings().len(), 1);
        // a burst of different problems fills the queue and drops the rest
        for skill in 0..40 {
            w.push(Finding::SkillRefused { skill, code: 1 });
        }
        assert_eq!(w.take_findings().len(), MAX_FINDINGS);
    }

    #[test]
    fn episode_keys_differ_between_problems_and_not_between_occurrences() {
        let a = Finding::NoResponse {
            req: Req::SkillCast,
            id: 7,
            aux: 0,
            waited_ms: 6_000,
        };
        let b = Finding::NoResponse {
            req: Req::SkillCast,
            id: 7,
            aux: 0,
            waited_ms: 9_000,
        };
        let c = Finding::NoResponse {
            req: Req::QuestAccept,
            id: 7,
            aux: 0,
            waited_ms: 6_000,
        };
        assert_eq!(a.episode_key(), b.episode_key());
        assert_ne!(a.episode_key(), c.episode_key());
        assert_ne!(
            Finding::Death(Death::NoDeathPacket).episode_key(),
            Finding::Death(Death::LoggedInDead).episode_key()
        );
    }

    #[test]
    fn the_session_forgets_nothing_it_should_remember_and_remembers_malformed_packets_once() {
        let mut w = Watch::default();
        assert!(w.new_malformed(1, 0x123));
        assert!(!w.new_malformed(1, 0x123));
        assert!(w.new_malformed(2, 0x123), "another level is another packet");
        for op in 0..20 {
            w.new_malformed(1, 0x200 + op);
        }
        assert!(w.malformed.len() <= MAX_MALFORMED);
    }

    #[test]
    fn events_feed_the_tracker_and_the_last_accepted_cast() {
        let mut w = Watch::default();
        w.tracker.expect(Pending {
            req: Req::SkillCast,
            id: 14_001,
            aux: 0,
            sent_ms: 100,
            wait_ms: 6_000,
            pos: [0.0; 2],
        });
        w.tracker.expect(Pending {
            req: Req::QuestAccept,
            id: 5,
            aux: 0,
            sent_ms: 100,
            wait_ms: 6_000,
            pos: [0.0; 2],
        });
        let started = ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj: 9,
            skill: 14_001,
            tl: 1,
            target_obj: 12,
            result: 0,
        });
        w.observe(&started, 9, 500);
        assert_eq!(w.cast_ok, (14_001, 500));
        // an instant skill: SCSkillFired alone, from the raw packet
        w.tracker.expect(Pending {
            req: Req::SkillCast,
            id: 18_132,
            aux: 0,
            sent_ms: 600,
            wait_ms: 6_000,
            pos: [0.0; 2],
        });
        let mut fired = 18_132u32.to_le_bytes().to_vec();
        fired.extend(3u16.to_le_bytes());
        fired.push(0);
        fired.extend(&9u32.to_le_bytes()[..3]);
        fired.extend([0; 16]);
        w.on_raw(1, aa_proto::opcodes::sc::SCSkillFiredPacket, &fired, 8, 610);
        assert_eq!(w.tracker.len(), 2, "somebody else's cast answers nothing");
        w.on_raw(1, aa_proto::opcodes::sc::SCSkillFiredPacket, &fired, 9, 620);
        assert_eq!(w.cast_ok, (18_132, 620));
        assert_eq!(w.tracker.len(), 1);
        w.cast_ok = (14_001, 500);
        assert_eq!(w.tracker.len(), 1);
        // a refusal answers the request but is not an accepted cast
        let refused = ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj: 9,
            skill: 14_002,
            tl: 0,
            target_obj: 12,
            result: 8,
        });
        w.observe(&refused, 9, 600);
        assert_eq!(w.cast_ok, (14_001, 500));
        // another unit's cast is nobody's business
        let other = ServerEvent::World(WorldEvent::SkillStarted {
            caster_obj: 77,
            skill: 14_003,
            tl: 1,
            target_obj: 0,
            result: 0,
        });
        w.observe(&other, 9, 700);
        assert_eq!(w.cast_ok, (14_001, 500));
        // the server moving the bot forgets the quest request
        let tp = ServerEvent::World(WorldEvent::TeleportUnit {
            reason: 0,
            error: 0,
            pos: (1.0, 2.0, 3.0),
        });
        w.observe(&tp, 9, 800);
        assert!(w.tracker.is_empty());
    }

    #[test]
    fn a_connection_that_goes_quiet_is_a_stall_once_and_ends_with_a_quick_pong() {
        let mut c = ConnWatch::default();
        c.on_rx(1_000);
        // the housekeeping pass runs every second
        for t in (2_000..=8_000).step_by(1_000) {
            assert_eq!(c.check(t), None, "7 s of silence at {t} is not sustained");
        }
        let s = c.check(9_500).expect("8.5 s of silence");
        assert_eq!(s.suspect, crate::issues::Suspect::Server);
        assert!(s.quiet_ms >= 8_000);
        // the same episode is not reported again
        for t in (10_500..=30_500).step_by(1_000) {
            assert_eq!(c.check(t), None);
        }
        // packets come back, a quick pong ends the episode, the next stall is a new one
        c.on_rx(31_000);
        c.on_pong(150);
        assert_eq!(c.check(32_000), None);
        for t in (33_000..=39_000).step_by(1_000) {
            assert_eq!(c.check(t), None);
        }
        assert!(c.check(41_000).is_some());
    }

    #[test]
    fn a_slow_pong_is_a_stall_even_with_traffic_and_a_starved_bot_blames_itself() {
        let mut c = ConnWatch::default();
        c.on_rx(1_000);
        assert_eq!(c.check(1_500), None);
        c.on_rx(2_000);
        c.on_pong(6_200);
        let s = c.check(2_500).expect("a ping took 6 s");
        assert_eq!((s.quiet_ms, s.rtt_ms), (0, 6_200));
        // the bot's own housekeeping did not run for 6 s: it was starved, the server is not guilty
        let mut c = ConnWatch::default();
        c.on_rx(1_000);
        assert_eq!(c.check(1_500), None);
        let s = c.check(12_000).expect("silent for 11 s");
        assert_eq!(s.suspect, crate::issues::Suspect::Bot);
    }

    #[test]
    fn the_trail_is_attached_unless_the_detector_brought_its_own() {
        let mut w = Watch::default();
        w.packet(100, Dir::C2S, 1, aa_proto::opcodes::cs::CSStartSkillPacket);
        w.packet(
            150,
            Dir::S2C,
            1,
            aa_proto::opcodes::sc::SCSkillStartedPacket,
        );
        let r = IssueReport::new(crate::issues::IssueKind::Other, "k", "t");
        let r = w.attach_trail(r, 200);
        assert_eq!(r.trail.len(), 2);
        assert_eq!(&*r.trail[1].name, "SCSkillStartedPacket");
        assert_eq!(r.trail[1].t_ms_before, 50);
        let own = IssueReport::new(crate::issues::IssueKind::Other, "k", "t")
            .trail(vec![crate::issues::TrailEntry::new(1, Dir::S2C, 1, 1, "X")]);
        assert_eq!(w.attach_trail(own, 200).trail.len(), 1);
    }
}
