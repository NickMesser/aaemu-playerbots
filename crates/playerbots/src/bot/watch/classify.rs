//! The decisions of the detectors, as pure functions over plain numbers.
//!
//! Nothing here touches the network, a clock or the bot: the callers pass in what they saw, and
//! the answer says whether it is worth a report and who is to blame. That keeps the judgement
//! (the part that must not produce false positives) testable without a server.
//!
//! The sections: skill refusals, connection faults, stalls, deaths, quest credit, target
//! immunity and what a finished trade must have changed.

use aa_proto::sc_world::{ItemChange, ItemTaskInfo};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::bot::state::{Item, SLOT_BAG};
use crate::issues::{IssueKind, Severity, Suspect};

// ---- skill refusals ------------------------------------------------------------------------

/// `SkillResult` values (the server's `skill_result`, `systems/skills/types.rs`).
pub mod skill_result {
    pub const FAILURE: u8 = 0x01;
    pub const TARGET_IMMUNE: u8 = 0x1D;
    pub const INVALID_SKILL: u8 = 0x1E;
    pub const INACTIVE_ABILITY: u8 = 0x1F;
    pub const NOT_ENOUGH_ABILITY_LEVEL: u8 = 0x20;
    pub const INVALID_SOURCE: u8 = 0x21;
    pub const INVALID_LOCATION: u8 = 0x23;
    pub const NOT_NOW: u8 = 0x2C;
    pub const NO_PERM: u8 = 0x2D;
    pub const SKILL_REQ_FAIL: u8 = 0x32;
}

/// Whose fault a refused cast is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The bot cast while on cooldown, out of mana, out of range or sight, moving, facing away,
    /// at a target that was gone, or in a state the game forbids (stunned, swimming ...): its own
    /// pre-checks are rough and the server is right.
    Bot,
    /// The server says the target is immune to the skill.
    Immune,
    /// A refusal the bot's pre-checks rule out: the skill is learned, ready and affordable, so the
    /// server's answer (not learned, ability inactive, bad source, not now, no permission ...)
    /// points at the server's data or code.
    Unexpected,
}

/// Classifies a `SkillStarted` result code other than 0.
pub fn refusal(code: u8) -> Refusal {
    use skill_result::*;
    match code {
        TARGET_IMMUNE => Refusal::Immune,
        FAILURE
        | INVALID_SKILL
        | INACTIVE_ABILITY
        | NOT_ENOUGH_ABILITY_LEVEL
        | INVALID_SOURCE
        | INVALID_LOCATION
        | NOT_NOW
        | NO_PERM
        | SKILL_REQ_FAIL => Refusal::Unexpected,
        _ => Refusal::Bot,
    }
}

/// The server's name of a `SkillResult` code, for report texts.
pub fn skill_result_name(code: u8) -> &'static str {
    match code {
        0x01 => "Failure",
        0x02 => "SourceDied",
        0x04 => "TargetDied",
        0x07 => "OnCasting",
        0x08 => "CooldownTime",
        0x09 => "NoTarget",
        0x0B => "LackMana",
        0x0C => "Obstacle",
        0x0F => "TooFarRange",
        0x12 => "CannotCastWhileMoving",
        0x1D => "TargetImmune",
        0x1E => "InvalidSkill",
        0x1F => "InactiveAbility",
        0x20 => "NotEnoughAbilityLevel",
        0x21 => "InvalidSource",
        0x22 => "InvalidTarget",
        0x23 => "InvalidLocation",
        0x2C => "NotNow",
        0x2D => "NoPerm",
        0x32 => "SkillReqFail",
        _ => "SkillResult",
    }
}

// ---- connection faults ---------------------------------------------------------------------

/// Where in the bot's life a connection problem happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The login server exchange.
    Login,
    /// Opening the game connection.
    Connect,
    /// The lobby (enter world, character list, create or select, spawn).
    Lobby,
    /// In the world.
    World,
}

impl Phase {
    /// The word used in keys and texts.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Login => "login",
            Phase::Connect => "connect",
            Phase::Lobby => "lobby",
            Phase::World => "world",
        }
    }
}

/// Why a connection or a login step ended without the bot asking for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// `SCKicked` with its reason code.
    Kicked(u8),
    /// The server closed the connection (end of stream).
    Closed,
    /// The socket failed (reset, aborted, broken pipe).
    Reset,
    /// No packet for the whole read-idle limit (a half-open connection).
    Silent,
    /// The server stopped taking data.
    WriteTimeout,
    /// The server logged the character out although the bot did not ask.
    Logout,
    /// A lobby step or the login exchange got no answer; the text names the step.
    StepTimeout(&'static str),
    /// `ACLoginDenied` with its reason.
    Denied(u8),
    /// `ACJoinResponse` refused with its reason.
    JoinRefused(u16),
    /// `ACEnterWorldDenied`: the game server is down or refused the account.
    EnterDenied(u8),
    /// The login server lists no active world.
    NoWorld,
    /// `X2EnterWorldResponse` with a non-zero reason.
    Refused(i16),
    /// The character could not be created for a reason that is not about the name.
    CreateFailed(u8),
    /// A TCP connection could not be opened.
    ConnectFailed,
    /// A packet or frame that does not follow the protocol.
    Protocol,
}

/// A connection problem: the phase and the cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    /// Where.
    pub phase: Phase,
    /// Why.
    pub cause: Cause,
}

impl Fault {
    /// The part of a report that depends on the fault.
    pub fn verdict(self) -> ConnVerdict {
        use Cause::*;
        let p = self.phase.name();
        let (kind, reason, suspect, severity) = match self.cause {
            Kicked(r) => (
                IssueKind::Kicked,
                format!("reason={r}"),
                Suspect::Unknown,
                Severity::High,
            ),
            Closed => (
                IssueKind::UnexpectedDisconnect,
                "reason=closed".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            Reset => (
                IssueKind::UnexpectedDisconnect,
                "reason=reset".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            Silent => (
                IssueKind::UnexpectedDisconnect,
                "reason=silent".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            WriteTimeout => (
                IssueKind::UnexpectedDisconnect,
                "reason=write_timeout".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            Logout => (
                IssueKind::UnexpectedDisconnect,
                "reason=logout".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            StepTimeout(what) => (
                IssueKind::NoResponse,
                format!("waiting for {what}"),
                Suspect::Server,
                Severity::Medium,
            ),
            Denied(r) => (
                IssueKind::LoginFailure,
                format!("denied={r}"),
                // wrong password or a banned account: the bot's setup, not the server's code
                Suspect::Bot,
                Severity::Low,
            ),
            JoinRefused(r) => (
                IssueKind::LoginFailure,
                format!("join_refused={r}"),
                Suspect::Unknown,
                Severity::Medium,
            ),
            EnterDenied(r) => (
                IssueKind::LoginFailure,
                format!("enter_denied={r}"),
                Suspect::Server,
                Severity::Medium,
            ),
            NoWorld => (
                IssueKind::LoginFailure,
                "no_active_world".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            Refused(r) => (
                IssueKind::LoginFailure,
                format!("refused={r}"),
                Suspect::Server,
                Severity::Medium,
            ),
            CreateFailed(r) => (
                IssueKind::LoginFailure,
                format!("create_failed={r}"),
                Suspect::Server,
                Severity::Medium,
            ),
            ConnectFailed => (
                IssueKind::LoginFailure,
                "reason=connect_failed".into(),
                Suspect::Server,
                Severity::Medium,
            ),
            Protocol => (
                IssueKind::MalformedPacket,
                "reason=protocol".into(),
                Suspect::Unknown,
                Severity::Medium,
            ),
        };
        ConnVerdict {
            kind,
            key: format!("phase={p} {reason}"),
            suspect,
            severity,
        }
    }
}

/// What a [`Fault`] comes to as a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnVerdict {
    /// The kind.
    pub kind: IssueKind,
    /// The stable key: the phase and the reason.
    pub key: String,
    /// Who is to blame.
    pub suspect: Suspect,
    /// How bad.
    pub severity: Severity,
}

/// Whether a connection fault is a new episode worth a report. A bot that lost its connection is
/// told once; the failed reconnect attempts that follow belong to the same episode, which ends
/// when a session lasted `healthy` (the caller resets `reported` then).
pub fn new_episode(reported: &mut bool) -> bool {
    !std::mem::replace(reported, true)
}

// ---- stalls --------------------------------------------------------------------------------

/// No packet for this long while the bot pings every 3 s: the server stopped talking.
pub const STALL_QUIET_MS: u64 = 8_000;
/// A pong that took this long to come back.
pub const STALL_RTT_MS: u64 = 5_000;
/// A pong faster than this ends a stall episode.
pub const HEALTHY_RTT_MS: u64 = 2_000;
/// The housekeeping pass is meant to run every second; this much longer means the bot's own task
/// was not scheduled (the machine is overloaded) and its measurements of the server are worthless.
pub const LAG_GAP_MS: u64 = 3_000;
/// After such a gap the bot distrusts its stall measurements for this long.
pub const LAG_MEMORY_MS: u64 = 15_000;

/// A sustained stall of the server connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stall {
    /// Milliseconds without any packet (0 when only a slow pong was seen).
    pub quiet_ms: u64,
    /// The slow pong's round trip, 0 when none was seen.
    pub rtt_ms: u64,
    /// The bot's own task was starved recently: not the server's fault.
    pub suspect: Suspect,
}

/// Judges the connection: `quiet_ms` since the last packet of any kind, `rtt_ms` of the latest
/// pong (0 = none arrived), `lagging` = the bot's own loop stalled recently.
pub fn stall(quiet_ms: u64, rtt_ms: u64, lagging: bool) -> Option<Stall> {
    if quiet_ms <= STALL_QUIET_MS && rtt_ms <= STALL_RTT_MS {
        return None;
    }
    Some(Stall {
        quiet_ms: if quiet_ms > STALL_QUIET_MS {
            quiet_ms
        } else {
            0
        },
        rtt_ms: if rtt_ms > STALL_RTT_MS { rtt_ms } else { 0 },
        suspect: if lagging {
            Suspect::Bot
        } else {
            Suspect::Server
        },
    })
}

// ---- deaths --------------------------------------------------------------------------------

/// The server sends `SCUnitDeath` within this long after the points reach 0.
pub const DEATH_PACKET_GRACE_MS: u64 = 3_000;
/// A dead bot that asked for the resurrection and is still dead this long after its respawn time
/// (or, when it logged in dead, after it entered the world) gets no help from the server.
pub const RECOVER_LIMIT_MS: u64 = 45_000;

/// What the death watch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    /// The points are 0 but the server never said the bot died.
    NoDeathPacket,
    /// The bot logged in dead and the server never revived it.
    LoggedInDead,
    /// The bot died in the world, asked for the temple resurrection and is still dead.
    NeverResurrected,
}

/// Whether a server log line that names the bot's character is an ordinary event the server logs
/// at `WARN` (it was ported from the C# server that way), not a problem. `message` is the line
/// without time, level and target ([`crate::issues::logtail::split_line`]).
///
/// Seen in a live run: `[Quest] for player: <name>, quest: <id> removed.` is written every time a
/// quest leaves the journal (the bot abandoned it, or it timed out); a dozen bots filed it within
/// minutes as "the server warned about a bot". `Skill line of sight blocked character=...` is the
/// server's log of a refused cast through a wall: the bot has no geometry to check that (its answer
/// is the `skill_obstacle` refusal, which the combat controller already handles by repositioning).
pub fn routine_server_line(message: &str) -> bool {
    let m = message.trim();
    (m.starts_with("[Quest] for player:") && m.ends_with(" removed."))
        || m.starts_with("Skill line of sight blocked")
}

/// What the bot knows about its own death.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeathWatch {
    /// Bot clock when the points reached 0 through `SCUnitPoints` while the bot was alive and no
    /// `SCUnitDeath` has come since; 0 = none.
    pub points_zero_ms: u64,
    /// Bot clock when the character was found dead at login; 0 = it was not.
    pub login_dead_ms: u64,
    /// Which findings were reported already (bit per [`Death`]).
    reported: u8,
}

impl DeathWatch {
    /// The points of the living bot reached 0 at `now_ms`.
    pub fn points_zero(&mut self, now_ms: u64) {
        if self.points_zero_ms == 0 {
            self.points_zero_ms = now_ms.max(1);
        }
    }

    /// `SCUnitDeath` arrived for the bot.
    pub fn death_packet(&mut self) {
        self.points_zero_ms = 0;
    }

    /// The character was found dead on entering the world.
    pub fn found_dead_at_login(&mut self, now_ms: u64) {
        if self.login_dead_ms == 0 {
            self.login_dead_ms = now_ms.max(1);
        }
    }

    /// The bot is alive again.
    pub fn alive(&mut self) {
        self.points_zero_ms = 0;
        self.login_dead_ms = 0;
        self.reported &= !(bit(Death::LoggedInDead) | bit(Death::NeverResurrected));
    }

    /// Judges the situation at `now_ms`; each finding comes once per episode. `dead` is the
    /// bot's state, `respawn_at_ms` the time from which it may ask to be resurrected.
    pub fn check(&mut self, now_ms: u64, dead: bool, respawn_at_ms: u64) -> Option<Death> {
        let found = if self.points_zero_ms != 0
            && now_ms.saturating_sub(self.points_zero_ms) > DEATH_PACKET_GRACE_MS
        {
            self.points_zero_ms = 0;
            Some(Death::NoDeathPacket)
        } else if !dead {
            None
        } else if self.login_dead_ms != 0 {
            (now_ms.saturating_sub(self.login_dead_ms) > RECOVER_LIMIT_MS)
                .then_some(Death::LoggedInDead)
        } else {
            (now_ms.saturating_sub(respawn_at_ms) > RECOVER_LIMIT_MS)
                .then_some(Death::NeverResurrected)
        };
        let f = found?;
        if self.reported & bit(f) != 0 {
            return None;
        }
        self.reported |= bit(f);
        Some(f)
    }
}

fn bit(d: Death) -> u8 {
    1 << (d as u8)
}

// ---- quest credit --------------------------------------------------------------------------

/// The server's quest engine credits an objective within a few seconds (it evaluates in batches).
pub const CREDIT_GRACE_MS: u64 = 10_000;

/// What became of an action that should have advanced an objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credit {
    /// The counter moved (or the objective is done): fine.
    Given,
    /// Still within the grace period.
    Waiting,
    /// The grace period is over and the counter did not move.
    Missing,
}

/// Judges one action: `have_then` = the counter when the server confirmed it, `have_now` = now
/// (`None` when the quest is gone from the journal, which means completed or dropped: not a
/// missing credit), `waited_ms` since the confirmation.
pub fn credit(have_then: i32, have_now: Option<i32>, waited_ms: u64) -> Credit {
    match have_now {
        None => Credit::Given,
        Some(n) if n > have_then => Credit::Given,
        Some(_) if waited_ms < CREDIT_GRACE_MS => Credit::Waiting,
        Some(_) => Credit::Missing,
    }
}

/// An action the server confirmed that should advance an objective, waiting for the counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreditNote {
    /// A skill cast (an item or doodad use), not a kill.
    pub by_skill: bool,
    /// The monster template killed, or the skill cast.
    pub source: u32,
    /// Counter index of the objective.
    pub idx: u8,
    /// The counter when the action was confirmed.
    pub have: i32,
    /// Bot clock (ms) from which the grace period runs (for a cast: when it completes).
    pub since_ms: u64,
}

/// What to do with a [`CreditNote`] at one look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditStep {
    /// Look again later.
    Keep,
    /// Forget it: credited, or no longer comparable.
    Drop,
    /// The credit is missing: report.
    Report,
}

impl CreditNote {
    /// Judges the note: `idx` and `have_now` are the counter being worked now, `damaged_since` =
    /// the bot lost hit points since the action (a cast that was interrupted by an attack is not
    /// the server's doing).
    pub fn step(&self, idx: usize, have_now: i32, now_ms: u64, damaged_since: bool) -> CreditStep {
        if usize::from(self.idx) != idx {
            return CreditStep::Drop;
        }
        match credit(
            self.have,
            Some(have_now),
            now_ms.saturating_sub(self.since_ms),
        ) {
            Credit::Given => CreditStep::Drop,
            Credit::Waiting => CreditStep::Keep,
            Credit::Missing if self.by_skill && damaged_since => CreditStep::Drop,
            Credit::Missing => CreditStep::Report,
        }
    }
}

// ---- target immunity -----------------------------------------------------------------------

/// Accepted casts at one target, none of which cost it a hit point, that make it "immune".
pub const IMMUNE_CASTS: u8 = 8;
/// ... and the fight must have lasted at least this long since the first of them.
pub const IMMUNE_MIN_MS: u64 = 8_000;

/// Whether `accepted` casts that the server confirmed, over `since_first_ms`, leaving the target's
/// points untouched (`dealt` = false), mean it cannot be hurt.
pub fn target_immune(accepted: u8, dealt: bool, since_first_ms: u64) -> bool {
    !dealt && accepted >= IMMUNE_CASTS && since_first_ms >= IMMUNE_MIN_MS
}

// ---- trades --------------------------------------------------------------------------------

/// What the item tasks of a finished trade did to the purse and the bag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Copper added to the purse (negative = paid).
    pub money: i64,
    /// Items that arrived, by template.
    pub gained: SmallVec<[(u32, i64); 4]>,
    /// Items that left (stack counts), all templates together.
    pub lost: i64,
    /// Stacks that left whole but that the bot's bag did not know (its picture is stale): the
    /// number of items that left is then unknown and is not compared.
    pub lost_unknown: u8,
}

/// Adds up the item task of a finished trade. `bag` is the bag *before* the task is applied (the
/// count of a stack that leaves whole is only known from there).
pub fn tally(task: &ItemTaskInfo, bag: &FxHashMap<u16, Item>) -> Tally {
    let mut t = Tally::default();
    let gain = |t: &mut Tally, template: u32, n: i64| match t
        .gained
        .iter_mut()
        .find(|g| g.0 == template)
    {
        Some(g) => g.1 += n,
        None => t.gained.push((template, n)),
    };
    for change in &task.changes {
        match *change {
            ItemChange::Money(d) => t.money += i64::from(d),
            ItemChange::Added {
                slot_type: SLOT_BAG,
                template,
                count,
                ..
            } => gain(&mut t, template, i64::from(count.max(0))),
            ItemChange::CountUpdate {
                slot_type: SLOT_BAG,
                delta,
                template,
                ..
            } => {
                if delta >= 0 {
                    gain(&mut t, template, i64::from(delta));
                } else {
                    t.lost += i64::from(-delta);
                }
            }
            ItemChange::Removed {
                slot_type: SLOT_BAG,
                id,
                slot,
                ..
            }
            | ItemChange::RemovedSlot {
                slot_type: SLOT_BAG,
                id,
                slot,
            } => {
                match bag
                    .values()
                    .find(|i| i.id == id && id != 0)
                    .or_else(|| bag.get(&u16::from(slot)))
                {
                    Some(i) => t.lost += i64::from(i.count.max(0)),
                    None => t.lost_unknown = t.lost_unknown.saturating_add(1),
                }
            }
            _ => {}
        }
    }
    t
}

/// How a finished trade differs from what both sides put up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeFault {
    /// The purse changed by another amount than `other.money - mine.money`.
    Money {
        /// What the offers say.
        expected: i64,
        /// What the server's item task did.
        actual: i64,
    },
    /// The partner's items did not arrive (or fewer arrived).
    ItemsIn {
        /// A template the partner offered and did not fully hand over.
        template: u32,
        /// Units the partner offered.
        expected: i64,
        /// Units that arrived.
        actual: i64,
    },
    /// The bot lost another number of items than it put up.
    ItemsOut {
        /// Units the bot put up.
        expected: i64,
        /// Units that left the bag.
        actual: i64,
    },
}

/// Compares a trade's result with the two offers. `mine_items` = units the bot put up in all,
/// `other_items` = `(template, units)` the partner put up. Returns the first mismatch.
pub fn trade_fault(
    mine_money: i64,
    other_money: i64,
    mine_items: i64,
    other_items: &[(u32, i64)],
    got: &Tally,
) -> Option<TradeFault> {
    let expected = other_money - mine_money;
    if got.money != expected {
        return Some(TradeFault::Money {
            expected,
            actual: got.money,
        });
    }
    for &(template, units) in other_items {
        let arrived = got
            .gained
            .iter()
            .find(|g| g.0 == template)
            .map_or(0, |g| g.1);
        if arrived < units {
            return Some(TradeFault::ItemsIn {
                template,
                expected: units,
                actual: arrived,
            });
        }
    }
    if got.lost_unknown == 0 && got.lost != mine_items {
        return Some(TradeFault::ItemsOut {
            expected: mine_items,
            actual: got.lost,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use skill_result::*;

    #[test]
    fn the_servers_ordinary_warnings_about_a_character_are_not_findings() {
        use crate::issues::logtail::split_line;
        let routine = "17:42:14 [WARN] engine - [Quest] for player: Kleklend, quest: 1222 removed.";
        assert!(routine_server_line(split_line(routine).message));
        let sight = "17:44:39 [WARN] validation - Skill line of sight blocked character=Kleklend skill=11918 target=45654 result_key=\"skill_obstacle\"";
        assert!(routine_server_line(split_line(sight).message));
        // other warnings that name a character still are
        for real in [
            "17:42:14 [WARN] engine - [Quest] for player: Kleklend, quest: 1222 rejected: bad step",
            "17:42:14 [ERROR] engine - [Quest] for player: Kleklend failed to save quests",
            "17:42:14 [WARN] housing - Kleklend tried to build outside the zone",
        ] {
            assert!(!routine_server_line(split_line(real).message), "{real}");
        }
    }

    #[test]
    fn refusals_the_bot_causes_are_told_from_the_ones_it_cannot() {
        // cooldown, mana, range, line of sight, moving, a stale target, dead target, swimming
        for code in [
            0x08, 0x0B, 0x0F, 0x0E, 0x0C, 0x12, 0x09, 0x04, 0x07, 0x15, 0x10, 0x13,
        ] {
            assert_eq!(refusal(code), Refusal::Bot, "{code:#x}");
        }
        assert_eq!(refusal(TARGET_IMMUNE), Refusal::Immune);
        for code in [
            FAILURE,
            INVALID_SKILL,
            INACTIVE_ABILITY,
            NOT_ENOUGH_ABILITY_LEVEL,
            INVALID_SOURCE,
            INVALID_LOCATION,
            NOT_NOW,
            NO_PERM,
            SKILL_REQ_FAIL,
        ] {
            assert_eq!(refusal(code), Refusal::Unexpected, "{code:#x}");
        }
        // an unknown code is never blamed on the server
        assert_eq!(refusal(0xEE), Refusal::Bot);
        assert_eq!(skill_result_name(INVALID_SKILL), "InvalidSkill");
        assert_eq!(skill_result_name(0xEE), "SkillResult");
    }

    #[test]
    fn a_fault_has_a_key_of_phase_and_reason_and_an_honest_suspect() {
        let v = Fault {
            phase: Phase::World,
            cause: Cause::Closed,
        }
        .verdict();
        assert_eq!(v.kind, IssueKind::UnexpectedDisconnect);
        assert_eq!(v.key, "phase=world reason=closed");
        assert_eq!((v.suspect, v.severity), (Suspect::Server, Severity::Medium));

        let kick = Fault {
            phase: Phase::World,
            cause: Cause::Kicked(3),
        }
        .verdict();
        assert_eq!(kick.kind, IssueKind::Kicked);
        assert_eq!(kick.key, "phase=world reason=3");
        assert_eq!(kick.severity, Severity::High);
        assert!(kick.kind.always_report());

        // a refused password is the bot's setup: recorded, never queued
        let denied = Fault {
            phase: Phase::Login,
            cause: Cause::Denied(2),
        }
        .verdict();
        assert_eq!(denied.kind, IssueKind::LoginFailure);
        assert_eq!(denied.suspect, Suspect::Bot);
        assert!(!denied.suspect.is_reportable());

        let step = Fault {
            phase: Phase::Lobby,
            cause: Cause::StepTimeout("the own unit state"),
        }
        .verdict();
        assert_eq!(step.kind, IssueKind::NoResponse);
        assert_eq!(step.key, "phase=lobby waiting for the own unit state");

        // the key never holds anything that changes from one occurrence to the next
        let a = Fault {
            phase: Phase::World,
            cause: Cause::Silent,
        };
        assert_eq!(a.verdict().key, a.verdict().key);
    }

    #[test]
    fn a_disconnect_is_one_episode_until_the_bot_was_healthy_again() {
        let mut reported = false;
        assert!(new_episode(&mut reported), "the first loss is reported");
        assert!(!new_episode(&mut reported), "the failed reconnects are not");
        assert!(!new_episode(&mut reported));
        reported = false; // a healthy session ended the episode
        assert!(new_episode(&mut reported));
    }

    #[test]
    fn only_a_sustained_stall_counts_and_a_starved_bot_blames_itself() {
        assert_eq!(stall(3_000, 120, false), None);
        assert_eq!(stall(STALL_QUIET_MS, STALL_RTT_MS, false), None);
        let s = stall(9_000, 0, false).unwrap();
        assert_eq!(
            (s.quiet_ms, s.rtt_ms, s.suspect),
            (9_000, 0, Suspect::Server)
        );
        let s = stall(100, 6_500, false).unwrap();
        assert_eq!((s.quiet_ms, s.rtt_ms), (0, 6_500));
        let s = stall(12_000, 0, true).unwrap();
        assert_eq!(s.suspect, Suspect::Bot);
    }

    #[test]
    fn a_death_without_the_server_saying_so_is_found_once() {
        let mut d = DeathWatch::default();
        assert_eq!(d.check(10_000, false, 0), None);
        d.points_zero(10_000);
        // the death packet usually follows at once
        assert_eq!(d.check(11_000, true, 0), None);
        d.death_packet();
        assert_eq!(d.check(20_000, true, 12_000), None);
        // the packet never comes
        let mut d = DeathWatch::default();
        d.points_zero(10_000);
        d.points_zero(10_500); // a second update does not move the start
        assert_eq!(d.check(13_000, true, 0), None);
        assert_eq!(d.check(13_001, true, 0), Some(Death::NoDeathPacket));
        assert_eq!(d.check(14_000, true, 0), None, "reported once");
    }

    #[test]
    fn a_bot_that_logged_in_dead_and_never_recovered_is_found_once() {
        let mut d = DeathWatch::default();
        d.found_dead_at_login(1_000);
        assert_eq!(d.check(30_000, true, 2_500), None);
        assert_eq!(d.check(46_001, true, 2_500), Some(Death::LoggedInDead));
        assert_eq!(d.check(60_000, true, 2_500), None);
        // revived: the next death is a fresh case
        d.alive();
        assert_eq!(d.check(100_000, false, 0), None);
        d.found_dead_at_login(100_000);
        assert_eq!(d.check(150_000, true, 0), Some(Death::LoggedInDead));
    }

    #[test]
    fn a_death_in_the_world_that_is_never_resurrected_is_found_once() {
        let mut d = DeathWatch::default();
        // dead, may rise from 20 s on
        assert_eq!(d.check(60_000, true, 20_000), None);
        assert_eq!(d.check(65_001, true, 20_000), Some(Death::NeverResurrected));
        assert_eq!(d.check(70_000, true, 20_000), None);
        // alive bots are never judged
        let mut d = DeathWatch::default();
        assert_eq!(d.check(1_000_000, false, 0), None);
    }

    #[test]
    fn credit_is_missing_only_after_the_grace_period_and_only_for_a_quest_still_open() {
        assert_eq!(credit(2, Some(3), 100), Credit::Given);
        assert_eq!(credit(2, Some(2), 9_999), Credit::Waiting);
        assert_eq!(credit(2, Some(2), CREDIT_GRACE_MS), Credit::Missing);
        // the quest left the journal (completed, dropped): nothing is missing
        assert_eq!(credit(2, None, 60_000), Credit::Given);
        // a counter that went down (the quest reset) is no credit and no reason to wait for ever
        assert_eq!(credit(2, Some(0), CREDIT_GRACE_MS), Credit::Missing);
    }

    #[test]
    fn a_credit_note_is_kept_dropped_or_reported() {
        let kill = CreditNote {
            by_skill: false,
            source: 3_475,
            idx: 1,
            have: 2,
            since_ms: 10_000,
        };
        assert_eq!(kill.step(1, 2, 12_000, false), CreditStep::Keep);
        assert_eq!(kill.step(1, 3, 12_000, false), CreditStep::Drop);
        assert_eq!(kill.step(1, 2, 20_000, false), CreditStep::Report);
        // another objective is being worked now: the note does not apply
        assert_eq!(kill.step(2, 2, 20_000, false), CreditStep::Drop);
        // a kill is the server's to credit whatever happened to the bot since
        assert_eq!(kill.step(1, 2, 20_000, true), CreditStep::Report);
        // a cast that was interrupted by an attack proves nothing
        let cast = CreditNote {
            by_skill: true,
            ..kill
        };
        assert_eq!(cast.step(1, 2, 20_000, true), CreditStep::Drop);
        assert_eq!(cast.step(1, 2, 20_000, false), CreditStep::Report);
    }

    #[test]
    fn a_target_is_immune_after_many_confirmed_casts_without_a_point_lost() {
        assert!(!target_immune(IMMUNE_CASTS - 1, false, 60_000));
        assert!(!target_immune(IMMUNE_CASTS, false, IMMUNE_MIN_MS - 1));
        assert!(target_immune(IMMUNE_CASTS, false, IMMUNE_MIN_MS));
        // the moment it loses a point, it is not immune
        assert!(!target_immune(40, true, 60_000));
    }

    fn task(changes: Vec<ItemChange>) -> ItemTaskInfo {
        ItemTaskInfo {
            changes: SmallVec::from_vec(changes),
            ..ItemTaskInfo::default()
        }
    }

    #[test]
    fn a_finished_trade_is_added_up_from_its_item_task() {
        let mut bag = FxHashMap::default();
        bag.insert(
            3,
            Item {
                id: 77,
                template: 500,
                count: 4,
            },
        );
        let t = tally(
            &task(vec![
                ItemChange::Money(-250),
                ItemChange::Added {
                    action: 5,
                    slot_type: SLOT_BAG,
                    slot: 9,
                    template: 900,
                    id: 5,
                    grade: 0,
                    count: 3,
                    treasure: None,
                },
                ItemChange::CountUpdate {
                    slot_type: SLOT_BAG,
                    slot: 1,
                    id: 6,
                    delta: 2,
                    template: 900,
                },
                ItemChange::CountUpdate {
                    slot_type: SLOT_BAG,
                    slot: 2,
                    id: 8,
                    delta: -5,
                    template: 501,
                },
                ItemChange::Removed {
                    slot_type: SLOT_BAG,
                    slot: 3,
                    id: 77,
                    template: 500,
                },
            ]),
            &bag,
        );
        assert_eq!(t.money, -250);
        assert_eq!(t.gained.as_slice(), &[(900, 5)]);
        assert_eq!(
            t.lost, 9,
            "5 from the count update and the whole stack of 4"
        );
        assert_eq!(t.lost_unknown, 0);
        // a stack the bag never knew left whole: the number of items that left is unknown
        let t = tally(
            &task(vec![ItemChange::Removed {
                slot_type: SLOT_BAG,
                slot: 9,
                id: 12_345,
                template: 1,
            }]),
            &bag,
        );
        assert_eq!((t.lost, t.lost_unknown), (0, 1));
        let got_unknown = Tally {
            lost_unknown: 1,
            ..Tally::default()
        };
        assert_eq!(trade_fault(0, 0, 3, &[], &got_unknown), None);
    }

    #[test]
    fn a_trade_that_matches_its_offers_is_fine_and_each_mismatch_is_named() {
        let got = Tally {
            money: 300,
            gained: smallvec::smallvec![(900, 5)],
            lost: 2,
            lost_unknown: 0,
        };
        // we put up 2 items and 0 copper, they put up 5 of item 900 and 300 copper
        assert_eq!(trade_fault(0, 300, 2, &[(900, 5)], &got), None);
        assert_eq!(
            trade_fault(0, 500, 2, &[(900, 5)], &got),
            Some(TradeFault::Money {
                expected: 500,
                actual: 300
            })
        );
        assert_eq!(
            trade_fault(0, 300, 2, &[(900, 6)], &got),
            Some(TradeFault::ItemsIn {
                template: 900,
                expected: 6,
                actual: 5
            })
        );
        assert_eq!(
            trade_fault(0, 300, 3, &[(900, 5)], &got),
            Some(TradeFault::ItemsOut {
                expected: 3,
                actual: 2
            })
        );
        // a missing template counts as zero arrived
        assert_eq!(
            trade_fault(0, 300, 2, &[(901, 1)], &got),
            Some(TradeFault::ItemsIn {
                template: 901,
                expected: 1,
                actual: 0
            })
        );
        // paying money: the purse falls by what we offered
        let paid = Tally {
            money: -250,
            ..Tally::default()
        };
        assert_eq!(trade_fault(250, 0, 0, &[], &paid), None);
    }
}
