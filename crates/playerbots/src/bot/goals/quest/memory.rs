//! What a bot remembers about quests between two runs of the quest goal: which quests failed and
//! how often, which are quarantined, how many it finished.
//!
//! The memory lives in [`BotState::quest_mem`](crate::bot::state::BotState::quest_mem) (a goal is
//! created afresh every time the brain picks it) and is dropped with the state on a reconnect.
//! The failure policy follows the original bots' lifecycle controller:
//!
//! * every failure backs the quest off longer: [`BACKOFF_STEPS_MS`] is 30 s, 2 min, 8 min,
//! * [`FAILS_TO_QUARANTINE`] failures in a row quarantine it for the rest of the session
//!   ([`QUARANTINE_MS`]; a story quest cools down for [`STORY_COOLDOWN_MS`] instead and is never
//!   dropped from the journal),
//! * a quest the bot cannot do at all ([`Failure::Unsupported`]) is quarantined for
//!   [`UNSUPPORTED_MS`],
//! * any success clears the record of that quest.
//!
//! Reasons are typed ([`Failure`]), never strings.

use std::fmt;

use smallvec::SmallVec;

use super::progress::Progress;

/// The wait after the 1st, 2nd, ... failure in a row: 30 s, 2 min, 8 min.
pub const BACKOFF_STEPS_MS: [u64; 3] = [30_000, 120_000, 480_000];
/// Failures in a row after which a side quest is quarantined (after the three waits above).
pub const FAILS_TO_QUARANTINE: u8 = BACKOFF_STEPS_MS.len() as u8 + 1;
/// How long a side quest stays quarantined: for the rest of the session (the memory lives in the
/// bot state and is dropped on a reconnect).
pub const QUARANTINE_MS: u64 = u64::MAX;
/// How long a story quest rests after [`FAILS_TO_QUARANTINE`] failures.
pub const STORY_COOLDOWN_MS: u64 = 300_000;
/// How long a quest stays out that the bot cannot do (an objective kind without support).
pub const UNSUPPORTED_MS: u64 = 3_600_000;
/// A quest whose targets other characters crowd out rests this long. It does not count towards a
/// quarantine: the quest is fine, the spawn is just busy.
pub const CROWD_BACKOFF_MS: u64 = 45_000;
/// Entries kept; the one that expires first goes first.
const MAX_ENTRIES: usize = 24;

/// Why a quest (or one step of it) failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The bot cannot do it: the text names the objective or data that is missing.
    Unsupported(&'static str),
    /// The monsters are far above the bot's level.
    TooStrong,
    /// The quest giver (NPC or doodad) was not found at any of its spawns.
    GiverMissing,
    /// The server did not start the quest after the request.
    AcceptRefused,
    /// The server answered with `SCQuestContextFailed` and this reason byte.
    Server(u8),
    /// A start component's unit requirement refused the start (`SCQuestUnitReqFailed`): `kind`
    /// and its value (kind 0x25 names the quest that must be in progress).
    UnitReq {
        /// `unit_reqs.kind_id`.
        kind: u8,
        /// Requirement value.
        value: u32,
    },
    /// The turn-in did not complete the quest.
    TurnInRefused,
    /// No monster, NPC or doodad the objective needs could be found.
    TargetMissing,
    /// The objective did not advance within its time budget.
    NoProgress,
    /// The way to the target is blocked (stuck or trip timeout).
    Unreachable,
    /// A quest item is not in the bag.
    MissingItem,
    /// The server refused the skill the objective needs.
    CastRefused,
    /// The targets are there but other characters are working on them: wait for the respawns
    /// took longer than the bot is willing to.
    Crowded,
}

impl Failure {
    /// Whether the quest can never be done by this bot (as opposed to a bad moment).
    pub fn permanent(self) -> bool {
        matches!(self, Failure::Unsupported(_))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Unsupported(what) => write!(f, "unsupported ({what})"),
            Failure::TooStrong => f.write_str("monsters too strong"),
            Failure::GiverMissing => f.write_str("giver not found"),
            Failure::AcceptRefused => f.write_str("accept refused"),
            Failure::Server(err) => write!(f, "server refused (code {err})"),
            Failure::UnitReq { kind, value } => {
                write!(
                    f,
                    "server unit requirement refused (kind {kind}, value {value})"
                )
            }
            Failure::TurnInRefused => f.write_str("turn-in refused"),
            Failure::TargetMissing => f.write_str("no target found"),
            Failure::NoProgress => f.write_str("no progress"),
            Failure::Unreachable => f.write_str("unreachable"),
            Failure::MissingItem => f.write_str("quest item missing"),
            Failure::CastRefused => f.write_str("skill refused"),
            Failure::Crowded => f.write_str("targets taken by other characters"),
        }
    }
}

/// What a failure led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The quest rests until this bot-clock time.
    Backoff(u64),
    /// The quest is out until this bot-clock time.
    Quarantine(u64),
}

impl Verdict {
    /// Bot-clock time at which the quest may be tried again.
    pub fn until(self) -> u64 {
        match self {
            Verdict::Backoff(t) | Verdict::Quarantine(t) => t,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    quest: u32,
    fails: u8,
    until_ms: u64,
    last: Failure,
    quarantined: bool,
}

/// The per-bot quest memory. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Memory {
    entries: SmallVec<[Entry; 4]>,
    /// Quests this bot turned in since it connected.
    pub completed: u32,
    /// Quests this bot accepted since it connected.
    pub accepted: u32,
    /// Failures since it connected.
    pub failed: u32,
    /// Bot-clock time before which the goal does not scan for a new quest again.
    pub next_scan_ms: u64,
    /// The last `SCQuestUnitReqFailed`: quest, `kind`, value and bot clock.
    pub unit_req: Option<(u32, u8, u32, u64)>,
    /// Trips to places with quests when nothing is near, see [`super::progress`]. Allocated by the
    /// first empty scan.
    pub progress: Option<Box<Progress>>,
}

impl Memory {
    /// Records a failure of `quest` at `now_ms`. `story` quests rest instead of being quarantined.
    pub fn fail(&mut self, quest: u32, why: Failure, story: bool, now_ms: u64) -> Verdict {
        self.failed += 1;
        let i = self.slot(quest, why);
        let e = &mut self.entries[i];
        e.last = why;
        if why == Failure::Crowded {
            // not the quest's fault: a rest, no strike
            e.quarantined = false;
            e.until_ms = now_ms + CROWD_BACKOFF_MS;
            return Verdict::Backoff(e.until_ms);
        }
        e.fails = e.fails.saturating_add(1);
        if why.permanent() {
            e.quarantined = true;
            e.until_ms = now_ms.saturating_add(UNSUPPORTED_MS);
            return Verdict::Quarantine(e.until_ms);
        }
        if e.fails >= FAILS_TO_QUARANTINE {
            e.fails = 0;
            e.quarantined = !story;
            e.until_ms = now_ms.saturating_add(if story {
                STORY_COOLDOWN_MS
            } else {
                QUARANTINE_MS
            });
            return if story {
                Verdict::Backoff(e.until_ms)
            } else {
                Verdict::Quarantine(e.until_ms)
            };
        }
        e.quarantined = false;
        let step = BACKOFF_STEPS_MS[usize::from(e.fails - 1).min(BACKOFF_STEPS_MS.len() - 1)];
        e.until_ms = now_ms.saturating_add(step);
        Verdict::Backoff(e.until_ms)
    }

    /// The quest was turned in: forget its failures.
    pub fn success(&mut self, quest: u32) {
        self.entries.retain(|e| e.quest != quest);
        self.completed += 1;
    }

    /// The server started the quest: its accept failures are over.
    pub fn accepted(&mut self, quest: u32) {
        self.accepted += 1;
        self.entries.retain(|e| e.quest != quest);
    }

    /// Whether the quest may be tried at `now_ms`.
    pub fn usable(&self, quest: u32, now_ms: u64) -> bool {
        self.entries
            .iter()
            .find(|e| e.quest == quest)
            .is_none_or(|e| now_ms >= e.until_ms)
    }

    /// How long until a quest that is merely backing off (not quarantined) may be tried again,
    /// `None` when it may be tried now or is quarantined.
    pub fn retry_in(&self, quest: u32, now_ms: u64) -> Option<u64> {
        self.entries
            .iter()
            .find(|e| e.quest == quest && !e.quarantined && now_ms < e.until_ms)
            .map(|e| e.until_ms - now_ms)
    }

    /// Whether the quest is quarantined (not merely backing off) at `now_ms`.
    pub fn quarantined(&self, quest: u32, now_ms: u64) -> bool {
        self.entries
            .iter()
            .any(|e| e.quest == quest && e.quarantined && now_ms < e.until_ms)
    }

    /// The last failure recorded for `quest`.
    pub fn last_failure(&self, quest: u32) -> Option<Failure> {
        self.entries
            .iter()
            .find(|e| e.quest == quest)
            .map(|e| e.last)
    }

    /// Number of remembered quests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is remembered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn slot(&mut self, quest: u32, why: Failure) -> usize {
        if let Some(i) = self.entries.iter().position(|e| e.quest == quest) {
            return i;
        }
        if self.entries.len() >= MAX_ENTRIES {
            // Drop the entry that expires first.
            if let Some((i, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.until_ms)
            {
                self.entries.remove(i);
            }
        }
        self.entries.push(Entry {
            quest,
            fails: 0,
            until_ms: 0,
            last: why,
            quarantined: false,
        });
        self.entries.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_backs_the_quest_off() {
        let mut m = Memory::default();
        assert!(m.usable(7, 0));
        let v = m.fail(7, Failure::NoProgress, false, 1_000);
        assert_eq!(v, Verdict::Backoff(1_000 + BACKOFF_STEPS_MS[0]));
        assert!(!m.usable(7, 1_000 + BACKOFF_STEPS_MS[0] - 1));
        assert!(m.usable(7, 1_000 + BACKOFF_STEPS_MS[0]));
        assert!(!m.quarantined(7, 2_000));
        assert!(m.usable(8, 2_000));
    }

    #[test]
    fn repeated_failures_back_off_exponentially_then_quarantine() {
        let mut m = Memory::default();
        let mut now = 0;
        for (i, &wait) in BACKOFF_STEPS_MS.iter().enumerate() {
            let v = m.fail(7, Failure::AcceptRefused, false, now);
            assert_eq!(v, Verdict::Backoff(now + wait), "failure {}", i + 1);
            assert!(!m.usable(7, now + wait - 1));
            assert!(m.usable(7, now + wait));
            now += wait;
        }
        assert_eq!(BACKOFF_STEPS_MS, [30_000, 120_000, 480_000]);
        let v = m.fail(7, Failure::AcceptRefused, false, now);
        assert_eq!(v, Verdict::Quarantine(u64::MAX), "the rest of the session");
        assert!(m.quarantined(7, now + 1));
        assert!(!m.usable(7, now + 1));
        assert!(!m.usable(7, u64::MAX - 1));
        assert!(!m.quarantined(7, u64::MAX));
    }

    #[test]
    fn a_story_quest_rests_instead_of_being_quarantined() {
        let mut m = Memory::default();
        let mut v = Verdict::Backoff(0);
        for i in 0..FAILS_TO_QUARANTINE {
            v = m.fail(9, Failure::TargetMissing, true, u64::from(i) * 40_000);
        }
        assert!(matches!(v, Verdict::Backoff(t) if t >= 80_000 + STORY_COOLDOWN_MS));
        assert!(!m.quarantined(9, 90_000));
    }

    #[test]
    fn an_unsupported_quest_is_out_for_good() {
        let mut m = Memory::default();
        let v = m.fail(5, Failure::Unsupported("craft"), false, 100);
        assert_eq!(v, Verdict::Quarantine(100 + UNSUPPORTED_MS));
        assert!(m.quarantined(5, 200));
        assert_eq!(m.last_failure(5), Some(Failure::Unsupported("craft")));
        assert!(Failure::Unsupported("x").permanent());
        assert!(!Failure::NoProgress.permanent());
    }

    #[test]
    fn a_backing_off_quest_says_when_it_may_be_tried_again() {
        let mut m = Memory::default();
        assert_eq!(m.retry_in(7, 0), None);
        m.fail(7, Failure::NoProgress, false, 1_000);
        assert_eq!(m.retry_in(7, 11_000), Some(BACKOFF_STEPS_MS[0] - 10_000));
        assert_eq!(m.retry_in(7, 1_000 + BACKOFF_STEPS_MS[0]), None);
        // a quarantined quest is out for the session, not about to come back
        for _ in 0..FAILS_TO_QUARANTINE {
            m.fail(8, Failure::NoProgress, false, 0);
        }
        assert!(m.quarantined(8, 1));
        assert_eq!(FAILS_TO_QUARANTINE, 4, "three waits and then the drop");
        assert_eq!(m.retry_in(8, 1), None);
        // a story quest rests instead
        for _ in 0..FAILS_TO_QUARANTINE {
            m.fail(9, Failure::NoProgress, true, 0);
        }
        assert_eq!(m.retry_in(9, 1), Some(STORY_COOLDOWN_MS - 1));
    }

    #[test]
    fn success_clears_the_record() {
        let mut m = Memory::default();
        m.fail(7, Failure::Unreachable, false, 0);
        assert!(!m.is_empty());
        m.success(7);
        assert!(m.is_empty());
        assert!(m.usable(7, 0));
        assert_eq!(m.completed, 1);
    }

    #[test]
    fn the_memory_is_bounded() {
        let mut m = Memory::default();
        for q in 0..100 {
            m.fail(q, Failure::NoProgress, false, u64::from(q) * 10);
        }
        assert!(m.len() <= MAX_ENTRIES);
        // the newest failure is still remembered
        assert!(!m.usable(99, 995));
    }

    #[test]
    fn a_crowded_spawn_rests_the_quest_without_a_strike() {
        let mut m = Memory::default();
        let mut now = 0;
        // far more crowded failures than it takes to quarantine anything
        for _ in 0..FAILS_TO_QUARANTINE * 3 {
            let v = m.fail(251, Failure::Crowded, false, now);
            assert_eq!(v, Verdict::Backoff(now + CROWD_BACKOFF_MS));
            assert!(!m.usable(251, now + CROWD_BACKOFF_MS - 1));
            assert!(m.usable(251, now + CROWD_BACKOFF_MS));
            assert!(!m.quarantined(251, now + 1));
            now += CROWD_BACKOFF_MS;
        }
        assert_eq!(m.last_failure(251), Some(Failure::Crowded));
        assert!(!Failure::Crowded.permanent());
        // a real failure after them is still only the first strike
        assert!(matches!(
            m.fail(251, Failure::NoProgress, false, now),
            Verdict::Backoff(t) if t == now + BACKOFF_STEPS_MS[0]
        ));
        assert!(!m.quarantined(251, now + 1));
        assert_eq!(
            Failure::Crowded.to_string(),
            "targets taken by other characters"
        );
    }

    #[test]
    fn reasons_read_well() {
        assert_eq!(Failure::Server(7).to_string(), "server refused (code 7)");
        assert_eq!(
            Failure::Unsupported("level objective").to_string(),
            "unsupported (level objective)"
        );
    }
}
