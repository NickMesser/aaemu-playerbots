//! The phases of a dungeon run as a pure state machine.
//!
//! The goal ([`super::Dungeon`]) builds a [`Facts`] from the tick context every tick, asks
//! [`Machine::step`] what to do ([`Intent`]) and carries it out; everything that decides *when*
//! (timers, retries, what a refusal means, when the party is complete, when to leave) is here and
//! is tested with scripted facts.
//!
//! ```text
//!  Gather ─ order ─► Prep ─► Team ─► Travel ─► Muster ─► Enter ─► Load ─► Clear ─► Leave ─► Out ─► done
//!     │                                                     ▲                          │
//!     └─ no party in time: fail                             └─ a refusal that may pass  └─ fails ⇒ leave the team
//! ```
//!
//! * **Gather**: the bot asked the coordinator for a party and waits for its order.
//! * **Prep**: the dungeon is known; a level outside its range is raised (GM) or the run fails.
//! * **Team**: the leader invites the others one at a time, the others accept the leader's
//!   invitation; a team that is not the leader's is left first.
//! * **Travel**: walk (or `/move`) to the entrance doodad.
//! * **Muster**: wait at the entrance until everybody is within [`MUSTER_M`], then go in one by
//!   one ([`STAGGER_MS`] apart).
//! * **Enter**: cast the entrance skill on the doodad; a refusal is classified
//!   ([`super::entry`]).
//! * **Load**: between `SCProcessingInstance` / `SCLoadInstance` and the bot's answer.
//! * **Clear**: inside; the goal works through the route until it is done, the time is up, the
//!   party is gone, too many deaths, or two party wipes.
//! * **Leave**: walk to the exit doodad and cast its skill; when that does nothing, leave the team
//!   (the server then moves the bot out after about 15 s).
//! * **Out**: back in the main world: members leave the team, the leader dismisses it last.

use super::entry::{self, EntryError, Reaction};
use crate::bot::party::MAX_PARTY;

/// Waiting for a party order: longer than the relax wait, so a small party can still form after
/// it (the coordinator drops a wish before this and the bot gives up here).
pub const GATHER_MS: u64 = 660_000;
/// Waiting for the level raise to show.
pub const PREP_MS: u64 = 20_000;
/// Building the team.
pub const TEAM_MS: u64 = 75_000;
/// Gap between two invitations.
pub const INVITE_GAP_MS: u64 = 4_500;
/// Invitations per member.
pub const INVITE_TRIES: u8 = 3;
/// The whole trip to the entrance, at least (see [`travel_budget_ms`]).
pub const TRAVEL_MS: u64 = 10 * 60_000;
/// The pace (m/s) a trip to the entrance is budgeted at: the run speed minus detours around cliffs
/// and water, the fights on the way and the rests after them. (The first live run walked 2.9 km
/// in more than ten minutes.)
pub const TRAVEL_BUDGET_MPS: f32 = 3.0;
/// Extra time on top of the walk, for the last bots of the party to catch up.
pub const TRAVEL_SLACK_MS: u64 = 2 * 60_000;
/// The longest budget of a trip to the entrance.
pub const TRAVEL_MAX_MS: u64 = 45 * 60_000;

/// Failed trips (stuck, timeout) before the run is given up.
pub const MOTION_RETRIES: u8 = 5;
/// Everybody within this many metres of the entrance counts as arrived.
pub const MUSTER_M: f32 = 20.0;
/// Waiting for the others at the entrance (plus the walk of the farthest, see [`muster_patience_ms`]).
pub const MUSTER_MS: u64 = 90_000;
/// After a member died on the way, the party waits this long for it to come back before it goes
/// in without it. (The live run's leader waited eight minutes at the entrance.)
pub const MUSTER_DEAD_MS: u64 = 120_000;
/// Party wipes after which the run is given up (the party leaves through the exit).
pub const WIPE_LIMIT: u8 = 2;
/// Delay between two members entering.
pub const STAGGER_MS: u64 = 1_800;
/// Gap between two casts of the entrance skill.
pub const CAST_GAP_MS: u64 = 7_000;
/// Casts of the entrance skill.
pub const CAST_TRIES: u8 = 4;
/// Waiting for the entrance doodad to show.
pub const DOOR_WAIT_MS: u64 = 20_000;
/// A cast without any reaction for this long is a finding.
pub const NO_LOAD_MS: u64 = 30_000;
/// The whole load.
pub const LOAD_MS: u64 = 60_000;
/// The run inside.
pub const RUN_LIMIT_MS: u64 = 25 * 60_000;
/// Deaths after which a bot leaves.
pub const MAX_DEATHS: u8 = 3;
/// Gap between two casts of the exit skill.
pub const EXIT_GAP_MS: u64 = 6_000;
/// Casts of the exit skill.
pub const EXIT_TRIES: u8 = 4;
/// Reach of the exit doodad.
pub const EXIT_RANGE: f32 = 3.5;
/// After the exit failed and the bot left the team, how long it waits for the server to move it.
pub const EXIT_STUCK_MS: u64 = 60_000;
/// The way back out, team included.
pub const OUT_MS: u64 = 40_000;
/// The leader waits this long for the others to leave the team before it dismisses it.
pub const LEADER_OUT_WAIT_MS: u64 = 20_000;
/// Waiting for the exit doodad to show.
pub const EXIT_DOOR_WAIT_MS: u64 = 20_000;
/// Re-sends of a team packet.
pub const TEAM_RESEND_MS: u64 = 5_000;

/// How long a bot at the entrance waits for the others: [`MUSTER_MS`], and the walk of the
/// farthest of them (`far_m` metres from the entrance when the trip began). A party spreads over
/// hundreds of metres, and a bot that was moved to the door by a GM `/move` stands there long
/// before the others have walked.
pub fn muster_patience_ms(far_m: f32) -> u64 {
    let walk = if far_m.is_finite() && far_m > 0.0 {
        (far_m / TRAVEL_BUDGET_MPS * 1000.0) as u64
    } else {
        0
    };
    (MUSTER_MS + walk).min(TRAVEL_MAX_MS)
}

/// How long the trip to the entrance may take when the bot started `dist` metres (straight line)
/// from it: the party planner sends parties farther than the 10 minutes of a short walk cover.
pub fn travel_budget_ms(dist: f32) -> u64 {
    let walk = if dist.is_finite() && dist > 0.0 {
        (dist / TRAVEL_BUDGET_MPS * 1000.0) as u64 + TRAVEL_SLACK_MS
    } else {
        0
    };
    walk.clamp(TRAVEL_MS, TRAVEL_MAX_MS)
}

/// The phase a run is in. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the order.
    Gather,
    /// Checking level and dungeon.
    Prep,
    /// Building the team.
    Team,
    /// Walking to the entrance.
    Travel,
    /// Waiting for the party at the entrance.
    Muster,
    /// Casting the entrance skill.
    Enter,
    /// Between the server's load and the bot's answer.
    Load,
    /// Working through the dungeon.
    Clear,
    /// Walking to the exit and using it.
    Leave,
    /// Back in the main world, leaving the team.
    Out,
}

impl Phase {
    /// A short word for the web UI and the log.
    pub fn word(self) -> &'static str {
        match self {
            Phase::Gather => "looking for a party",
            Phase::Prep => "preparing",
            Phase::Team => "forming the team",
            Phase::Travel => "on the way to the dungeon",
            Phase::Muster => "waiting for the party at the entrance",
            Phase::Enter => "entering",
            Phase::Load => "loading",
            Phase::Clear => "clearing",
            Phase::Leave => "leaving",
            Phase::Out => "leaving the team",
        }
    }

    /// Whether a run goes on after the bot dies in this phase: inside the dungeon (the load and
    /// the exit included), and on the way there, so a member that dies before the entrance
    /// resurrects and walks back to the party. The leader waits for it a bounded time.
    pub fn survives_death(self) -> bool {
        matches!(
            self,
            Phase::Travel
                | Phase::Muster
                | Phase::Enter
                | Phase::Load
                | Phase::Clear
                | Phase::Leave
        )
    }
}

/// What the movement controller is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trip {
    /// Standing.
    #[default]
    Idle,
    /// On the way.
    Moving,
    /// The last trip was abandoned.
    Failed,
}

/// What the bot knows about its team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TeamFacts {
    /// The bot is in a team.
    pub in_team: bool,
    /// The team belongs to the leader (its owner is the leader, or the owner is not known).
    pub led_by_leader: bool,
    /// Members including the bot.
    pub size: u8,
    /// Members of the order, the bot not counted, that are not in the team: bit `i` for order
    /// index `i`.
    pub missing: u8,
    /// An invitation of the leader waits for an answer.
    pub invite: bool,
}

/// Everything the machine looks at in one tick.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Facts {
    /// The bot clock.
    pub now_ms: u64,
    /// The bot leads the party.
    pub leader: bool,
    /// The bot's index in the order (the leader is 0).
    pub rank: u8,
    /// Members of the order, the bot included.
    pub expected: u8,
    /// The bot's level.
    pub level: u8,
    /// The dungeon's lowest level.
    pub level_min: u8,
    /// The dungeon's highest level.
    pub level_max: u8,
    /// The bot may raise its level (GM).
    pub raise_ok: bool,
    /// Deaths of the bot in this run.
    pub deaths: u8,
    /// The bot is dead (waiting for a resurrection does not count against a trip or a muster).
    pub dead: bool,
    /// Online members of the party that are dead right now.
    pub muster_dead: u8,
    /// Party wipes in this run (everybody dead at once).
    pub wipes: u8,
    /// The bot is in an instance world.
    pub in_instance: bool,
    /// A load is under way (between the server's notice and the bot's answer).
    pub loading: bool,
    /// The instance world is the one the dungeon loads (or the name is not known).
    pub world_ok: bool,
    /// The team.
    pub team: TeamFacts,
    /// The bot stands at the entrance.
    pub at_door: bool,
    /// Horizontal distance to the entrance (0 = not known).
    pub door_dist: f32,
    /// The movement controller.
    pub trip: Trip,
    /// The entrance doodad is in view.
    pub door_in_view: bool,
    /// Every online member of the party is near the entrance.
    pub muster_ready: bool,
    /// Members (the bot included) near the entrance.
    pub muster_near: u8,
    /// Distance of the farthest online member from the entrance (0 = not known).
    pub muster_far: f32,
    /// A refusal of the entry the server sent since [`Machine::error_watermark`].
    pub error: Option<EntryError>,
    /// The server refused the entrance cast itself (range, cooldown, state) since
    /// [`Machine::error_watermark`].
    pub cast_refused: bool,
    /// The route is done (the leader's call).
    pub route_done: bool,
    /// The leader reported that the clear is over.
    pub party_leaving: bool,
    /// The leader left the game or is gone.
    pub leader_gone: bool,
    /// Distance to the exit doodad (or its known place).
    pub exit_dist: f32,
    /// The exit doodad is in view.
    pub exit_in_view: bool,
}

/// Why a run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fail {
    /// No party formed in time.
    NoParty,
    /// The level is outside the dungeon's range and cannot be raised.
    Level,
    /// The level raise did not show.
    LevelUp,
    /// The team did not form.
    Team,
    /// The team fell apart.
    TeamBroke,
    /// The bot could not reach the entrance.
    Stuck,
    /// Something took too long.
    Timeout,
    /// The entrance doodad never showed.
    NoDoor,
    /// The server refused the entry.
    Refused(EntryError),
    /// The server refused every cast of the entrance skill.
    CastRefused,
    /// The entry was cast but the server did nothing.
    NoLoad,
    /// The server loaded another world.
    WrongWorld,
    /// The exit does nothing.
    ExitStuck,
    /// The party was wiped twice.
    Wiped,
}

impl Fail {
    /// A short phrase for the log.
    pub fn word(self) -> &'static str {
        match self {
            Fail::NoParty => "no party formed",
            Fail::Level => "level outside the dungeon's range",
            Fail::LevelUp => "the level raise did not show",
            Fail::Team => "the team did not form",
            Fail::TeamBroke => "the team fell apart",
            Fail::Stuck => "could not reach the entrance",
            Fail::Timeout => "took too long",
            Fail::NoDoor => "the entrance doodad never showed",
            Fail::Refused(e) => e.word(),
            Fail::CastRefused => "the server refuses the entrance cast",
            Fail::NoLoad => "the server did not load the dungeon",
            Fail::WrongWorld => "the server loaded another world",
            Fail::ExitStuck => "the exit does nothing",
            Fail::Wiped => "the party was wiped twice",
        }
    }
}

/// What the goal is to do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Nothing.
    Wait,
    /// Ask the coordinator for a party.
    PostWish,
    /// Raise the level to the dungeon's minimum (GM).
    RaiseLevel,
    /// Invite the member with this order index.
    Invite(u8),
    /// Answer the pending invitation with yes.
    Accept,
    /// Leave the team.
    LeaveTeam,
    /// Dismiss the team (the leader).
    DismissTeam,
    /// Walk (or `/move`) to the entrance.
    GoToDoor,
    /// Stand still.
    Stand,
    /// Cast the entrance skill on the doodad.
    CastEntry,
    /// One clearing step.
    Clear,
    /// Walk to the exit.
    GoToExit,
    /// Cast the exit skill on the doodad.
    CastExit,
    /// The run is over.
    Done,
    /// The run failed.
    Fail(Fail),
}

/// The state machine of one run. See the module docs.
#[derive(Debug, Clone)]
pub struct Machine {
    phase: Phase,
    since_ms: u64,
    last_ms: u64,
    tries: u8,
    /// Invitations sent per order index.
    invites: [u8; MAX_PARTY],
    trips_failed: u8,
    /// The farthest the bot was from the entrance since the trip began (sets the time allowed).
    travel_dist: f32,
    /// The farthest a member was from the entrance since the muster began.
    muster_far: f32,
    /// When a dead member was first seen at the muster (0 = none); caps the wait for it.
    death_wait_ms: u64,
    posted: bool,
    raised_ms: u64,
    left_stale: bool,
    joined: bool,
    ready_ms: u64,
    cast_first_ms: u64,
    retries: u8,
    error_ms: u64,
    reentered: u8,
    entered_ms: u64,
    exit_fallback: bool,
    sent_out_ms: u64,
    /// The run ended inside because the bot was moved out without asking.
    expelled: bool,
}

impl Machine {
    /// A run that starts in [`Phase::Gather`] (`has_order` false) or [`Phase::Prep`].
    pub fn new(has_order: bool, now_ms: u64) -> Machine {
        Machine {
            phase: if has_order {
                Phase::Prep
            } else {
                Phase::Gather
            },
            since_ms: now_ms,
            last_ms: 0,
            tries: 0,
            invites: [0; MAX_PARTY],
            trips_failed: 0,
            travel_dist: 0.0,
            muster_far: 0.0,
            death_wait_ms: 0,
            posted: false,
            raised_ms: 0,
            left_stale: false,
            joined: false,
            ready_ms: 0,
            cast_first_ms: 0,
            retries: 0,
            error_ms: 0,
            reentered: 0,
            entered_ms: 0,
            exit_fallback: false,
            sent_out_ms: 0,
            expelled: false,
        }
    }

    /// The phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// When the bot arrived inside (0 = not yet).
    pub fn entered_ms(&self) -> u64 {
        self.entered_ms
    }

    /// The bot was moved out of the dungeon without asking (kicked, expelled).
    pub fn expelled(&self) -> bool {
        self.expelled
    }

    /// The entrance skill was cast at least once (and how long ago the first cast was).
    pub fn cast_age_ms(&self, now_ms: u64) -> Option<u64> {
        (self.cast_first_ms != 0).then(|| now_ms.saturating_sub(self.cast_first_ms))
    }

    /// Refusals older than this bot clock are not news: the time of the last cast or of the last
    /// refusal that was acted on.
    pub fn error_watermark(&self) -> u64 {
        self.error_ms
    }

    /// The `Phase::Gather` wish was posted.
    pub fn wish_posted(&self) -> bool {
        self.posted
    }

    /// Moves to phase `p`.
    fn go(&mut self, p: Phase, now_ms: u64) {
        self.phase = p;
        self.since_ms = now_ms;
        self.last_ms = 0;
        self.tries = 0;
        self.travel_dist = 0.0;
        self.muster_far = 0.0;
        self.death_wait_ms = 0;
    }

    /// The team the bot belonged to is gone (it was kicked or dismissed).
    fn lost(&self, f: &Facts) -> bool {
        self.joined && !f.team.in_team
    }

    /// One tick.
    pub fn step(&mut self, f: &Facts) -> Intent {
        let now = f.now_ms;
        if f.team.in_team && f.team.led_by_leader && f.team.size >= 2 {
            self.joined = true;
        }
        match self.phase {
            Phase::Gather => self.gather(now),
            Phase::Prep => self.prep(f, now),
            Phase::Team => self.team(f, now),
            Phase::Travel => self.travel(f, now),
            Phase::Muster => self.muster(f, now),
            Phase::Enter => self.enter(f, now),
            Phase::Load => self.load(f, now),
            Phase::Clear => self.clear(f, now),
            Phase::Leave => self.leave(f, now),
            Phase::Out => self.out(f, now),
        }
    }

    fn gather(&mut self, now: u64) -> Intent {
        if now.saturating_sub(self.since_ms) > GATHER_MS {
            return Intent::Fail(Fail::NoParty);
        }
        if !self.posted {
            self.posted = true;
            return Intent::PostWish;
        }
        Intent::Wait
    }

    fn prep(&mut self, f: &Facts, now: u64) -> Intent {
        if f.level > f.level_max {
            return Intent::Fail(Fail::Level);
        }
        if f.level >= f.level_min {
            self.go(Phase::Team, now);
            return Intent::Wait;
        }
        if !f.raise_ok {
            return Intent::Fail(Fail::Level);
        }
        if self.raised_ms == 0 {
            self.raised_ms = now.max(1);
            return Intent::RaiseLevel;
        }
        if now.saturating_sub(self.raised_ms) > PREP_MS {
            return Intent::Fail(Fail::LevelUp);
        }
        Intent::Wait
    }

    fn team(&mut self, f: &Facts, now: u64) -> Intent {
        let t = f.team;
        // a team that is not the leader's is left first
        if t.in_team && !t.led_by_leader {
            if !self.left_stale {
                self.left_stale = true;
                return Intent::LeaveTeam;
            }
            return Intent::Wait;
        }
        let complete = t.in_team && t.led_by_leader && t.missing == 0 && t.size >= f.expected;
        if complete {
            self.go(Phase::Travel, now);
            return Intent::GoToDoor;
        }
        let waited = now.saturating_sub(self.since_ms);
        if f.leader {
            if waited > TEAM_MS {
                // the others did not come: go with whoever joined, if anybody did
                if t.in_team && t.size >= 2 {
                    self.go(Phase::Travel, now);
                    return Intent::GoToDoor;
                }
                return Intent::Fail(Fail::Team);
            }
            if now.saturating_sub(self.last_ms) >= INVITE_GAP_MS || self.last_ms == 0 {
                for i in 0..MAX_PARTY.min(8) {
                    // (bit i is order index i; the leader is index 0 and never missing)
                    if t.missing & (1 << i) != 0 && self.invites[i] < INVITE_TRIES {
                        self.invites[i] += 1;
                        self.last_ms = now.max(1);
                        return Intent::Invite(i as u8);
                    }
                }
            }
            Intent::Wait
        } else {
            if t.invite {
                return Intent::Accept;
            }
            if waited > TEAM_MS {
                return Intent::Fail(Fail::Team);
            }
            Intent::Wait
        }
    }

    fn travel(&mut self, f: &Facts, now: u64) -> Intent {
        if self.lost(f) {
            return Intent::Fail(Fail::TeamBroke);
        }
        if f.dead {
            // a death on the way: the walk back starts when the bot is resurrected
            self.since_ms = now;
            return Intent::Stand;
        }
        self.travel_dist = self.travel_dist.max(f.door_dist);
        if now.saturating_sub(self.since_ms) > travel_budget_ms(self.travel_dist) {
            return Intent::Fail(Fail::Timeout);
        }
        if f.at_door {
            self.go(Phase::Muster, now);
            return Intent::Stand;
        }
        match f.trip {
            Trip::Moving => Intent::Wait,
            Trip::Failed => {
                self.trips_failed += 1;
                if self.trips_failed >= MOTION_RETRIES {
                    Intent::Fail(Fail::Stuck)
                } else {
                    Intent::GoToDoor
                }
            }
            Trip::Idle => Intent::GoToDoor,
        }
    }

    fn muster(&mut self, f: &Facts, now: u64) -> Intent {
        if self.lost(f) {
            return Intent::Fail(Fail::TeamBroke);
        }
        if f.dead {
            // the corpse lies at the door; the bot walks back after the resurrection
            self.since_ms = now;
            return Intent::Stand;
        }
        if !f.at_door {
            // pushed away from the entrance (knocked back, a late teleport)
            return Intent::GoToDoor;
        }
        let waited = now.saturating_sub(self.since_ms);
        self.muster_far = self.muster_far.max(f.muster_far);
        if self.ready_ms == 0 {
            // (once the party was complete the others walking in one by one do not undo it)
            if !f.muster_ready {
                if f.muster_dead > 0 && self.death_wait_ms == 0 {
                    self.death_wait_ms = now.max(1);
                }
                let over = if self.death_wait_ms != 0 {
                    // a member died on the way: wait a bounded time for it, not the whole walk
                    now.saturating_sub(self.death_wait_ms) > MUSTER_DEAD_MS
                } else {
                    waited > muster_patience_ms(self.muster_far)
                };
                if !over {
                    return Intent::Stand;
                }
                // the others did not come in time: go with those who did, if any
                if f.muster_near < 2 && f.expected >= 2 {
                    return Intent::Fail(Fail::Team);
                }
            }
            self.ready_ms = now.max(1);
        }
        if now < self.ready_ms + u64::from(f.rank) * STAGGER_MS {
            return Intent::Stand;
        }
        self.go(Phase::Enter, now);
        self.error_ms = now;
        Intent::Wait
    }

    fn enter(&mut self, f: &Facts, now: u64) -> Intent {
        if f.loading || f.in_instance {
            self.go(Phase::Load, now);
            return Intent::Wait;
        }
        if self.lost(f) {
            return Intent::Fail(Fail::TeamBroke);
        }
        if let Some(e) = f.error {
            self.error_ms = now;
            match entry::reaction(e) {
                Reaction::Retry => {
                    if self.retries >= 6 {
                        return Intent::Fail(Fail::Refused(e));
                    }
                    self.retries += 1;
                    self.last_ms = now.max(1);
                    return Intent::Wait;
                }
                Reaction::RaiseLevel => {
                    if f.raise_ok && f.level < f.level_min && self.raised_ms != u64::MAX {
                        // (once more: the first raise may not have shown yet)
                        self.raised_ms = u64::MAX;
                        self.last_ms = now.max(1);
                        return Intent::RaiseLevel;
                    }
                    return Intent::Fail(Fail::Refused(e));
                }
                Reaction::GiveUp { .. } => return Intent::Fail(Fail::Refused(e)),
            }
        }
        if f.cast_refused {
            // a refused cast is no cast: the clock for the server's reaction starts afresh
            self.error_ms = now;
            self.cast_first_ms = 0;
        }
        if !f.door_in_view {
            if now.saturating_sub(self.since_ms) > DOOR_WAIT_MS {
                return Intent::Fail(Fail::NoDoor);
            }
            return Intent::Wait;
        }
        if self.cast_first_ms != 0 && now.saturating_sub(self.cast_first_ms) > NO_LOAD_MS {
            return Intent::Fail(Fail::NoLoad);
        }
        if self.tries == 0 || now.saturating_sub(self.last_ms) >= CAST_GAP_MS {
            if self.tries >= CAST_TRIES {
                // (no accepted cast among them: the server refused every one)
                return Intent::Fail(if self.cast_first_ms == 0 {
                    Fail::CastRefused
                } else {
                    Fail::NoLoad
                });
            }
            self.tries += 1;
            self.last_ms = now.max(1);
            self.error_ms = now;
            if self.cast_first_ms == 0 {
                self.cast_first_ms = now.max(1);
            }
            return Intent::CastEntry;
        }
        Intent::Wait
    }

    fn load(&mut self, f: &Facts, now: u64) -> Intent {
        if f.in_instance && !f.loading {
            if !f.world_ok {
                return Intent::Fail(Fail::WrongWorld);
            }
            self.entered_ms = now.max(1);
            self.go(Phase::Clear, now);
            return Intent::Wait;
        }
        let waited = now.saturating_sub(self.since_ms);
        if !f.in_instance && !f.loading && waited > 15_000 {
            // the load went nowhere: cast again (twice at most)
            if self.reentered >= 2 {
                return Intent::Fail(Fail::NoLoad);
            }
            self.reentered += 1;
            self.cast_first_ms = 0;
            self.go(Phase::Enter, now);
            self.error_ms = now;
            return Intent::Wait;
        }
        if waited > LOAD_MS {
            return Intent::Fail(Fail::NoLoad);
        }
        Intent::Wait
    }

    fn clear(&mut self, f: &Facts, now: u64) -> Intent {
        if !f.in_instance && !f.loading {
            // moved out without asking: kicked, expelled
            self.expelled = true;
            self.go(Phase::Out, now);
            return Intent::Wait;
        }
        let over = f.route_done
            || f.party_leaving
            || f.leader_gone
            || f.deaths >= MAX_DEATHS
            || f.wipes >= WIPE_LIMIT
            || self.lost(f)
            || now.saturating_sub(self.entered_ms) > RUN_LIMIT_MS;
        if over {
            self.go(Phase::Leave, now);
            return Intent::Wait;
        }
        Intent::Clear
    }

    fn leave(&mut self, f: &Facts, now: u64) -> Intent {
        if !f.in_instance && !f.loading {
            self.go(Phase::Out, now);
            return Intent::Wait;
        }
        if self.exit_fallback {
            if now.saturating_sub(self.since_ms) > EXIT_STUCK_MS {
                return Intent::Fail(Fail::ExitStuck);
            }
            return Intent::Wait;
        }
        if f.exit_dist > EXIT_RANGE {
            if now.saturating_sub(self.since_ms) > 4 * EXIT_STUCK_MS {
                return Intent::Fail(Fail::ExitStuck);
            }
            return Intent::GoToExit;
        }
        if !f.exit_in_view {
            if now.saturating_sub(self.since_ms) > EXIT_DOOR_WAIT_MS * 2 {
                return self.fall_back_to_the_team(now);
            }
            return Intent::Wait;
        }
        if self.tries == 0 || now.saturating_sub(self.last_ms) >= EXIT_GAP_MS {
            if self.tries >= EXIT_TRIES {
                return self.fall_back_to_the_team(now);
            }
            self.tries += 1;
            self.last_ms = now.max(1);
            return Intent::CastExit;
        }
        Intent::Wait
    }

    /// The exit does nothing: leave the team, which makes the server move the bot out.
    fn fall_back_to_the_team(&mut self, now: u64) -> Intent {
        self.exit_fallback = true;
        self.since_ms = now;
        Intent::LeaveTeam
    }

    /// Whether the exit failed and the bot gave up on it.
    pub fn exit_failed(&self) -> bool {
        self.exit_fallback
    }

    fn out(&mut self, f: &Facts, now: u64) -> Intent {
        if f.in_instance || f.loading {
            // a late load is still under way
            return Intent::Wait;
        }
        let waited = now.saturating_sub(self.since_ms);
        if !f.team.in_team {
            return Intent::Done;
        }
        if waited > OUT_MS {
            return Intent::Done;
        }
        let resend =
            self.sent_out_ms == 0 || now.saturating_sub(self.sent_out_ms) >= TEAM_RESEND_MS;
        if f.leader {
            let others = f.team.size.saturating_sub(1);
            if (others == 0 || waited > LEADER_OUT_WAIT_MS) && resend {
                self.sent_out_ms = now.max(1);
                return Intent::DismissTeam;
            }
            return Intent::Wait;
        }
        if resend {
            self.sent_out_ms = now.max(1);
            return Intent::LeaveTeam;
        }
        Intent::Wait
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_run_survives_a_death_inside_or_on_the_way_to_the_dungeon() {
        for p in [
            Phase::Travel,
            Phase::Muster,
            Phase::Enter,
            Phase::Load,
            Phase::Clear,
            Phase::Leave,
        ] {
            assert!(p.survives_death(), "{p:?}");
        }
        for p in [Phase::Gather, Phase::Prep, Phase::Team, Phase::Out] {
            assert!(!p.survives_death(), "{p:?}");
        }
    }

    use super::*;

    fn facts(now_ms: u64) -> Facts {
        Facts {
            now_ms,
            leader: false,
            rank: 1,
            expected: 3,
            level: 20,
            level_min: 18,
            level_max: 55,
            raise_ok: false,
            world_ok: true,
            exit_dist: 100.0,
            ..Facts::default()
        }
    }

    fn in_team(size: u8, missing: u8) -> TeamFacts {
        TeamFacts {
            in_team: true,
            led_by_leader: true,
            size,
            missing,
            invite: false,
        }
    }

    #[test]
    fn a_bot_without_an_order_posts_one_wish_and_gives_up_after_the_wait() {
        let mut m = Machine::new(false, 1_000);
        assert_eq!(m.phase(), Phase::Gather);
        assert_eq!(m.step(&facts(1_000)), Intent::PostWish);
        assert_eq!(m.step(&facts(5_000)), Intent::Wait);
        assert!(m.wish_posted());
        assert_eq!(
            m.step(&facts(1_000 + GATHER_MS + 1)),
            Intent::Fail(Fail::NoParty)
        );
        // with an order the run starts at prep
        assert_eq!(Machine::new(true, 0).phase(), Phase::Prep);
    }

    #[test]
    fn prep_checks_the_level_and_raises_it_when_it_may() {
        // fine: straight to the team
        let mut m = Machine::new(true, 0);
        assert_eq!(m.step(&facts(10)), Intent::Wait);
        assert_eq!(m.phase(), Phase::Team);
        // too low and no GM: fail
        let mut m = Machine::new(true, 0);
        let low = Facts {
            level: 5,
            ..facts(10)
        };
        assert_eq!(m.step(&low), Intent::Fail(Fail::Level));
        // too high
        let mut m = Machine::new(true, 0);
        let high = Facts {
            level: 56,
            ..facts(10)
        };
        assert_eq!(m.step(&high), Intent::Fail(Fail::Level));
        // GM: raise once, wait for it, then go on
        let mut m = Machine::new(true, 0);
        let gm = Facts {
            level: 5,
            raise_ok: true,
            ..facts(10)
        };
        assert_eq!(m.step(&gm), Intent::RaiseLevel);
        assert_eq!(
            m.step(&Facts {
                now_ms: 2_000,
                ..gm
            }),
            Intent::Wait
        );
        assert_eq!(m.phase(), Phase::Prep);
        let raised = Facts {
            level: 18,
            raise_ok: true,
            ..facts(3_000)
        };
        assert_eq!(m.step(&raised), Intent::Wait);
        assert_eq!(m.phase(), Phase::Team);
        // a raise that never shows fails the run
        let mut m = Machine::new(true, 0);
        m.step(&gm);
        assert_eq!(
            m.step(&Facts {
                now_ms: 10 + PREP_MS + 1,
                ..gm
            }),
            Intent::Fail(Fail::LevelUp)
        );
    }

    fn at_team() -> Machine {
        let mut m = Machine::new(true, 0);
        m.step(&facts(0));
        assert_eq!(m.phase(), Phase::Team);
        m
    }

    #[test]
    fn the_leader_invites_the_missing_members_one_at_a_time() {
        let mut m = at_team();
        let lead = |now, missing, size, in_team| Facts {
            leader: true,
            rank: 0,
            team: TeamFacts {
                in_team,
                led_by_leader: in_team,
                size,
                missing,
                invite: false,
            },
            ..facts(now)
        };
        // members 1 and 2 are missing (bits 1 and 2)
        assert_eq!(m.step(&lead(1_000, 0b110, 0, false)), Intent::Invite(1));
        // not again before the gap
        assert_eq!(m.step(&lead(2_000, 0b110, 0, false)), Intent::Wait);
        // the first joined: the second is next
        assert_eq!(
            m.step(&lead(1_000 + INVITE_GAP_MS, 0b100, 2, true)),
            Intent::Invite(2)
        );
        // everybody is in: on to the entrance
        assert_eq!(m.step(&lead(20_000, 0, 3, true)), Intent::GoToDoor);
        assert_eq!(m.phase(), Phase::Travel);
    }

    #[test]
    fn invitations_are_limited_and_a_partial_team_goes_at_the_timeout() {
        let mut m = at_team();
        let lead = |now, size| Facts {
            leader: true,
            rank: 0,
            team: TeamFacts {
                in_team: size >= 2,
                led_by_leader: size >= 2,
                size,
                missing: 0b100,
                invite: false,
            },
            ..facts(now)
        };
        let mut sent = 0;
        let mut t = 100;
        while t < TEAM_MS {
            if matches!(m.step(&lead(t, 2)), Intent::Invite(2)) {
                sent += 1;
            }
            t += 1_000;
        }
        assert_eq!(sent, usize::from(INVITE_TRIES), "three tries for member 2");
        // at the timeout the leader goes with one member
        assert_eq!(m.step(&lead(TEAM_MS + 1, 2)), Intent::GoToDoor);
        assert_eq!(m.phase(), Phase::Travel);
        // a leader that stands alone fails
        let mut m = at_team();
        assert_eq!(
            m.step(&Facts {
                leader: true,
                rank: 0,
                team: TeamFacts {
                    missing: 0b110,
                    ..TeamFacts::default()
                },
                ..facts(TEAM_MS + 1)
            }),
            Intent::Fail(Fail::Team)
        );
    }

    #[test]
    fn a_member_accepts_the_leaders_invitation_and_leaves_a_stale_team() {
        let mut m = at_team();
        let with_invite = Facts {
            team: TeamFacts {
                invite: true,
                ..TeamFacts::default()
            },
            ..facts(500)
        };
        assert_eq!(m.step(&with_invite), Intent::Accept);
        assert_eq!(m.step(&facts(600)), Intent::Wait);
        // joined: a complete team goes on
        let full = Facts {
            team: in_team(3, 0),
            ..facts(900)
        };
        assert_eq!(m.step(&full), Intent::GoToDoor);
        // a team of somebody else is left first, once
        let mut m = at_team();
        let stale = Facts {
            team: TeamFacts {
                in_team: true,
                led_by_leader: false,
                size: 4,
                ..TeamFacts::default()
            },
            ..facts(100)
        };
        assert_eq!(m.step(&stale), Intent::LeaveTeam);
        assert_eq!(
            m.step(&Facts {
                now_ms: 200,
                ..stale
            }),
            Intent::Wait
        );
        // a member that never gets in fails
        let mut m = at_team();
        assert_eq!(m.step(&facts(TEAM_MS + 1)), Intent::Fail(Fail::Team));
    }

    fn at_travel() -> Machine {
        let mut m = at_team();
        let full = Facts {
            team: in_team(3, 0),
            ..facts(100)
        };
        assert_eq!(m.step(&full), Intent::GoToDoor);
        m
    }

    #[test]
    fn the_muster_waits_for_the_walk_of_the_farthest_member() {
        assert_eq!(muster_patience_ms(0.0), MUSTER_MS);
        assert_eq!(muster_patience_ms(f32::NAN), MUSTER_MS);
        assert_eq!(muster_patience_ms(3_000.0), MUSTER_MS + 1_000_000);
        assert_eq!(muster_patience_ms(1.0e9), TRAVEL_MAX_MS);
        // at the entrance first: the other walks 975 m (324 s), the old wait of 90 s was over
        let mut m = at_travel();
        let team = in_team(3, 0);
        let at = |now, near, far| Facts {
            team,
            at_door: true,
            muster_ready: false,
            muster_near: near,
            muster_far: far,
            ..facts(now)
        };
        assert_eq!(m.step(&at(200, 1, 975.0)), Intent::Stand);
        assert_eq!(m.phase(), Phase::Muster);
        assert_eq!(m.step(&at(300, 1, 975.0)), Intent::Stand);
        assert_eq!(m.step(&at(200 + MUSTER_MS + 1, 1, 400.0)), Intent::Stand);
        assert_eq!(m.step(&at(200 + 330_000, 1, 10.0)), Intent::Stand);
        // the others did come: the party goes in
        assert_ne!(m.step(&at(200 + 340_000, 2, 0.0)), Intent::Fail(Fail::Team));
        // and one that never comes is given up after the walk it needed
        let mut m = at_travel();
        assert_eq!(m.step(&at(200, 1, 975.0)), Intent::Stand);
        assert_eq!(m.step(&at(300, 1, 975.0)), Intent::Stand);
        assert_eq!(
            m.step(&at(200 + muster_patience_ms(975.0) + 1, 1, 975.0)),
            Intent::Fail(Fail::Team)
        );
    }

    #[test]
    fn a_far_entrance_gets_a_longer_budget() {
        assert_eq!(travel_budget_ms(0.0), TRAVEL_MS);
        assert_eq!(travel_budget_ms(1_000.0), TRAVEL_MS);
        assert_eq!(travel_budget_ms(f32::NAN), TRAVEL_MS);
        assert_eq!(travel_budget_ms(1.0e9), TRAVEL_MAX_MS);
        // 2.9 km at 3 m/s and two minutes of slack: the 10 minutes of the first live run were short
        let ms = travel_budget_ms(2_900.0);
        assert!((1_000_000..1_200_000).contains(&ms), "{ms}");
        let mut m = at_travel();
        let team = in_team(3, 0);
        let f = |now, door_dist| Facts {
            team,
            trip: Trip::Moving,
            door_dist,
            ..facts(now)
        };
        assert_eq!(m.step(&f(200, 2_900.0)), Intent::Wait);
        // still walking after the old limit
        assert_eq!(m.step(&f(100 + TRAVEL_MS + 60_000, 900.0)), Intent::Wait);
        // the budget is the one of the start distance, not of the distance now
        assert_eq!(m.step(&f(100 + ms + 1, 300.0)), Intent::Fail(Fail::Timeout));
    }

    #[test]
    fn a_death_on_the_way_does_not_use_up_the_trip_or_the_muster_wait() {
        let mut m = at_travel();
        let team = in_team(3, 0);
        let dead = Facts {
            team,
            dead: true,
            door_dist: 500.0,
            ..facts(100 + TRAVEL_MS + 1)
        };
        assert_eq!(m.step(&dead), Intent::Stand);
        assert_eq!(m.phase(), Phase::Travel);
        // resurrected: the walk (and its budget) starts again from here
        let alive = Facts {
            dead: false,
            trip: Trip::Moving,
            ..dead
        };
        assert_eq!(m.step(&alive), Intent::Wait);
        assert_eq!(
            m.step(&Facts {
                now_ms: alive.now_ms + TRAVEL_MS + 1,
                ..alive
            }),
            Intent::Fail(Fail::Timeout)
        );
    }

    #[test]
    fn a_party_waits_a_bounded_time_for_a_member_that_died_on_the_way() {
        let mut m = at_muster(0);
        let team = in_team(3, 0);
        let waiting = |now, dead| Facts {
            team,
            at_door: true,
            muster_ready: false,
            muster_near: 2,
            muster_far: 3_000.0,
            muster_dead: dead,
            rank: 0,
            ..facts(now)
        };
        assert_eq!(m.step(&waiting(1_000, 0)), Intent::Stand);
        assert_eq!(m.step(&waiting(1_500, 1)), Intent::Stand);
        // the walk of the farthest would allow much longer; the death caps it
        assert_eq!(
            m.step(&waiting(1_500 + MUSTER_DEAD_MS - 1, 1)),
            Intent::Stand
        );
        assert_eq!(
            m.step(&waiting(1_500 + MUSTER_DEAD_MS + 1, 1)),
            Intent::Wait,
            "the party goes in without the dead member"
        );
        assert_eq!(m.phase(), Phase::Enter);
    }

    #[test]
    fn travel_walks_until_the_bot_is_at_the_entrance() {
        let mut m = at_travel();
        let team = in_team(3, 0);
        let f = |now, trip, at_door| Facts {
            team,
            trip,
            at_door,
            ..facts(now)
        };
        assert_eq!(m.step(&f(200, Trip::Idle, false)), Intent::GoToDoor);
        assert_eq!(m.step(&f(300, Trip::Moving, false)), Intent::Wait);
        // a failed trip is tried again, five fail the run
        for _ in 0..MOTION_RETRIES - 1 {
            assert_eq!(m.step(&f(400, Trip::Failed, false)), Intent::GoToDoor);
        }
        assert_eq!(
            m.step(&f(500, Trip::Failed, false)),
            Intent::Fail(Fail::Stuck)
        );
        // arriving ends the trip
        let mut m = at_travel();
        assert_eq!(m.step(&f(600, Trip::Idle, true)), Intent::Stand);
        assert_eq!(m.phase(), Phase::Muster);
        // the trip has a limit
        let mut m = at_travel();
        assert_eq!(
            m.step(&f(100 + TRAVEL_MS + 1, Trip::Moving, false)),
            Intent::Fail(Fail::Timeout)
        );
        // and the team falling apart ends it
        let mut m = at_travel();
        let gone = Facts {
            team: TeamFacts::default(),
            ..facts(900)
        };
        assert_eq!(m.step(&gone), Intent::Fail(Fail::TeamBroke));
    }

    fn at_muster(rank: u8) -> Machine {
        let mut m = at_travel();
        let f = Facts {
            team: in_team(3, 0),
            at_door: true,
            rank,
            ..facts(200)
        };
        assert_eq!(m.step(&f), Intent::Stand);
        m
    }

    #[test]
    fn the_party_gathers_at_the_entrance_and_goes_in_one_by_one() {
        let team = in_team(3, 0);
        let f = |now, ready: bool, near, rank| Facts {
            team,
            at_door: true,
            muster_ready: ready,
            muster_near: near,
            rank,
            ..facts(now)
        };
        // waiting for the others
        let mut m = at_muster(0);
        assert_eq!(m.step(&f(1_000, false, 1, 0)), Intent::Stand);
        // all there: the leader (rank 0) goes at once, rank 2 waits two staggers
        assert_eq!(m.step(&f(5_000, true, 3, 0)), Intent::Wait);
        assert_eq!(m.phase(), Phase::Enter);
        let mut m2 = at_muster(2);
        assert_eq!(m2.step(&f(5_000, true, 3, 2)), Intent::Stand);
        assert_eq!(m2.step(&f(5_000 + STAGGER_MS, true, 3, 2)), Intent::Stand);
        assert_eq!(
            m2.step(&f(5_000 + 2 * STAGGER_MS, true, 3, 2)),
            Intent::Wait
        );
        assert_eq!(m2.phase(), Phase::Enter);
        // the others do not come: after the wait the bot goes if somebody else is there
        let mut m = at_muster(0);
        assert_eq!(m.step(&f(200 + MUSTER_MS + 1, false, 2, 0)), Intent::Wait);
        assert_eq!(m.phase(), Phase::Enter);
        // ... and fails when it is alone
        let mut m = at_muster(0);
        assert_eq!(
            m.step(&f(200 + MUSTER_MS + 1, false, 1, 0)),
            Intent::Fail(Fail::Team)
        );
        // pushed away from the door: walk back
        let mut m = at_muster(0);
        assert_eq!(
            m.step(&Facts {
                at_door: false,
                ..f(900, false, 1, 0)
            }),
            Intent::GoToDoor
        );
    }

    fn at_enter() -> Machine {
        let mut m = at_muster(0);
        let f = Facts {
            team: in_team(3, 0),
            at_door: true,
            muster_ready: true,
            muster_near: 3,
            rank: 0,
            ..facts(1_000)
        };
        assert_eq!(m.step(&f), Intent::Wait);
        assert_eq!(m.phase(), Phase::Enter);
        m
    }

    fn enter_facts(now: u64) -> Facts {
        Facts {
            team: in_team(3, 0),
            at_door: true,
            door_in_view: true,
            rank: 0,
            ..facts(now)
        }
    }

    #[test]
    fn enter_casts_the_entrance_skill_and_waits_for_the_load() {
        let mut m = at_enter();
        assert_eq!(m.step(&enter_facts(1_100)), Intent::CastEntry);
        assert!(m.cast_age_ms(1_100).is_some());
        // no second cast before the gap
        assert_eq!(m.step(&enter_facts(2_000)), Intent::Wait);
        // the server says "processing": loading
        let processing = Facts {
            loading: true,
            ..enter_facts(3_000)
        };
        assert_eq!(m.step(&processing), Intent::Wait);
        assert_eq!(m.phase(), Phase::Load);
        // the load finishes in the right world: clearing starts
        let loaded = Facts {
            in_instance: true,
            loading: false,
            ..enter_facts(5_000)
        };
        assert_eq!(m.step(&loaded), Intent::Wait);
        assert_eq!(m.phase(), Phase::Clear);
        assert_eq!(m.entered_ms(), 5_000);
    }

    #[test]
    fn a_cast_that_does_nothing_is_cast_again_and_then_reported() {
        let mut m = at_enter();
        assert_eq!(m.step(&enter_facts(1_100)), Intent::CastEntry);
        assert_eq!(m.step(&enter_facts(1_100 + CAST_GAP_MS)), Intent::CastEntry);
        assert_eq!(
            m.step(&enter_facts(1_100 + NO_LOAD_MS + 1)),
            Intent::Fail(Fail::NoLoad),
            "30 s without a reaction"
        );
        // a missing doodad
        let mut m = at_enter();
        let no_door = Facts {
            door_in_view: false,
            ..enter_facts(1_100)
        };
        assert_eq!(m.step(&no_door), Intent::Wait);
        assert_eq!(
            m.step(&Facts {
                now_ms: 1_000 + DOOR_WAIT_MS + 1,
                ..no_door
            }),
            Intent::Fail(Fail::NoDoor)
        );
    }

    #[test]
    fn casts_the_server_refuses_are_not_casts_that_did_nothing() {
        let mut m = at_enter();
        let mut now = 1_100;
        for _ in 0..CAST_TRIES {
            assert_eq!(m.step(&enter_facts(now)), Intent::CastEntry);
            now += 1_000;
            let refused = Facts {
                cast_refused: true,
                ..enter_facts(now)
            };
            assert_eq!(m.step(&refused), Intent::Wait);
            now += CAST_GAP_MS;
        }
        assert_eq!(
            m.step(&enter_facts(now)),
            Intent::Fail(Fail::CastRefused),
            "every cast was refused: the server's answer is the finding, not silence"
        );
        // a refusal after an accepted cast does not hide that the server then did nothing
        let mut m = at_enter();
        assert_eq!(m.step(&enter_facts(1_100)), Intent::CastEntry);
        assert_eq!(m.step(&enter_facts(1_100 + CAST_GAP_MS)), Intent::CastEntry);
        assert_eq!(
            m.step(&enter_facts(1_100 + 2 * CAST_GAP_MS + 100)),
            Intent::CastEntry
        );
        assert_eq!(
            m.step(&enter_facts(1_100 + 3 * CAST_GAP_MS + 200)),
            Intent::CastEntry
        );
        assert_eq!(
            m.step(&enter_facts(1_100 + 4 * CAST_GAP_MS + 300)),
            Intent::Fail(Fail::NoLoad)
        );
    }

    #[test]
    fn the_muster_stays_complete_when_the_first_members_go_in() {
        let f = |now, ready: bool, near, rank| Facts {
            team: in_team(3, 0),
            at_door: true,
            muster_ready: ready,
            muster_near: near,
            rank,
            ..facts(now)
        };
        let mut m = at_muster(2);
        assert_eq!(m.step(&f(5_000, true, 3, 2)), Intent::Stand);
        // rank 0 and 1 went in: only this bot is left at the door, but the party was complete
        assert_eq!(m.step(&f(5_000 + STAGGER_MS, false, 1, 2)), Intent::Stand);
        assert_eq!(
            m.step(&f(5_000 + 2 * STAGGER_MS, false, 1, 2)),
            Intent::Wait
        );
        assert_eq!(m.phase(), Phase::Enter);
    }

    #[test]
    fn refusals_are_answered_by_their_kind() {
        // the dungeon is busy: wait and cast again
        let mut m = at_enter();
        assert_eq!(m.step(&enter_facts(1_100)), Intent::CastEntry);
        let busy = Facts {
            error: Some(EntryError::TryLater),
            ..enter_facts(2_000)
        };
        assert_eq!(m.step(&busy), Intent::Wait);
        assert_eq!(m.error_watermark(), 2_000);
        // the entry limit is final
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        let limit = Facts {
            error: Some(EntryError::VisitLimit),
            ..enter_facts(2_000)
        };
        assert_eq!(
            m.step(&limit),
            Intent::Fail(Fail::Refused(EntryError::VisitLimit))
        );
        // a level problem is raised away when GM allows it, once
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        let level = Facts {
            error: Some(EntryError::Level),
            raise_ok: true,
            level: 12,
            ..enter_facts(2_000)
        };
        assert_eq!(m.step(&level), Intent::RaiseLevel);
        assert_eq!(
            m.step(&Facts {
                now_ms: 3_000,
                ..level
            }),
            Intent::Fail(Fail::Refused(EntryError::Level))
        );
        // ... and without GM it is final at once
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        let no_gm = Facts {
            error: Some(EntryError::Level),
            level: 12,
            ..enter_facts(2_000)
        };
        assert_eq!(
            m.step(&no_gm),
            Intent::Fail(Fail::Refused(EntryError::Level))
        );
        // too many "full" answers end the run
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        for _ in 0..6 {
            let full = Facts {
                error: Some(EntryError::Full),
                ..enter_facts(2_000)
            };
            assert_eq!(m.step(&full), Intent::Wait);
        }
        assert_eq!(
            m.step(&Facts {
                error: Some(EntryError::Full),
                ..enter_facts(2_000)
            }),
            Intent::Fail(Fail::Refused(EntryError::Full))
        );
    }

    #[test]
    fn a_load_into_another_world_or_none_fails() {
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        m.step(&Facts {
            loading: true,
            ..enter_facts(2_000)
        });
        let wrong = Facts {
            in_instance: true,
            world_ok: false,
            ..enter_facts(3_000)
        };
        assert_eq!(m.step(&wrong), Intent::Fail(Fail::WrongWorld));
        // a load that never ends
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        let loading = Facts {
            loading: true,
            ..enter_facts(2_000)
        };
        m.step(&loading);
        assert_eq!(
            m.step(&Facts {
                now_ms: 2_000 + LOAD_MS + 1,
                ..loading
            }),
            Intent::Fail(Fail::NoLoad)
        );
        // a load that vanished goes back to casting (twice), then fails
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        m.step(&loading);
        assert_eq!(m.phase(), Phase::Load);
        let nothing = enter_facts(2_000 + 16_000);
        assert_eq!(m.step(&nothing), Intent::Wait);
        assert_eq!(m.phase(), Phase::Enter);
    }

    fn at_clear() -> Machine {
        let mut m = at_enter();
        m.step(&enter_facts(1_100));
        m.step(&Facts {
            loading: true,
            ..enter_facts(2_000)
        });
        m.step(&Facts {
            in_instance: true,
            ..enter_facts(3_000)
        });
        assert_eq!(m.phase(), Phase::Clear);
        m
    }

    fn inside(now: u64) -> Facts {
        Facts {
            in_instance: true,
            team: in_team(3, 0),
            rank: 0,
            leader: true,
            ..facts(now)
        }
    }

    #[test]
    fn the_clear_ends_with_the_route_the_clock_deaths_or_the_party() {
        let mut m = at_clear();
        assert_eq!(m.step(&inside(10_000)), Intent::Clear);
        assert_eq!(
            m.step(&Facts {
                route_done: true,
                ..inside(11_000)
            }),
            Intent::Wait
        );
        assert_eq!(m.phase(), Phase::Leave);
        // the leader's word
        let mut m = at_clear();
        m.step(&Facts {
            party_leaving: true,
            ..inside(10_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
        // too many deaths
        let mut m = at_clear();
        m.step(&Facts {
            deaths: MAX_DEATHS,
            ..inside(10_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
        // two deaths are fine
        let mut m = at_clear();
        assert_eq!(
            m.step(&Facts {
                deaths: MAX_DEATHS - 1,
                ..inside(10_000)
            }),
            Intent::Clear
        );
        // the time limit
        let mut m = at_clear();
        m.step(&inside(3_000 + RUN_LIMIT_MS + 1));
        assert_eq!(m.phase(), Phase::Leave);
        // kicked out of the team
        let mut m = at_clear();
        m.step(&inside(5_000));
        m.step(&Facts {
            team: TeamFacts::default(),
            ..inside(6_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
        // the leader is gone
        let mut m = at_clear();
        m.step(&Facts {
            leader_gone: true,
            ..inside(6_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
        // the party was wiped twice: leave through the exit
        let mut m = at_clear();
        assert_eq!(
            m.step(&Facts {
                wipes: WIPE_LIMIT - 1,
                ..inside(10_000)
            }),
            Intent::Clear
        );
        m.step(&Facts {
            wipes: WIPE_LIMIT,
            ..inside(11_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
    }

    #[test]
    fn being_moved_out_without_asking_is_noticed() {
        let mut m = at_clear();
        let out = Facts {
            in_instance: false,
            ..inside(10_000)
        };
        assert_eq!(m.step(&out), Intent::Wait);
        assert_eq!(m.phase(), Phase::Out);
        assert!(m.expelled());
    }

    fn at_leave() -> Machine {
        let mut m = at_clear();
        m.step(&Facts {
            route_done: true,
            ..inside(10_000)
        });
        assert_eq!(m.phase(), Phase::Leave);
        m
    }

    #[test]
    fn leave_walks_to_the_exit_and_casts_its_skill() {
        let mut m = at_leave();
        let far = Facts {
            exit_dist: 40.0,
            ..inside(11_000)
        };
        assert_eq!(m.step(&far), Intent::GoToExit);
        let near = Facts {
            exit_dist: 2.0,
            exit_in_view: true,
            ..inside(12_000)
        };
        assert_eq!(m.step(&near), Intent::CastExit);
        assert_eq!(
            m.step(&Facts {
                now_ms: 13_000,
                ..near
            }),
            Intent::Wait
        );
        assert_eq!(
            m.step(&Facts {
                now_ms: 12_000 + EXIT_GAP_MS,
                ..near
            }),
            Intent::CastExit
        );
        // the load back to the main world ends the stay
        let back = Facts {
            in_instance: false,
            team: in_team(3, 0),
            ..facts(20_000)
        };
        assert_eq!(m.step(&back), Intent::Wait);
        assert_eq!(m.phase(), Phase::Out);
        assert!(!m.expelled(), "the bot left on purpose");
    }

    #[test]
    fn an_exit_that_does_nothing_makes_the_bot_leave_the_team() {
        let mut m = at_leave();
        let near = Facts {
            exit_dist: 2.0,
            exit_in_view: true,
            ..inside(11_000)
        };
        let mut casts = 0;
        let mut t = 11_000;
        let mut intent = Intent::Wait;
        while t < 11_000 + 10 * EXIT_GAP_MS {
            intent = m.step(&Facts { now_ms: t, ..near });
            if intent == Intent::CastExit {
                casts += 1;
            }
            if intent == Intent::LeaveTeam {
                break;
            }
            t += 1_000;
        }
        assert_eq!(casts, usize::from(EXIT_TRIES));
        assert_eq!(intent, Intent::LeaveTeam);
        assert!(m.exit_failed());
        // then it waits for the server to move it, and fails when it does not
        assert_eq!(
            m.step(&Facts {
                now_ms: t + 1_000,
                ..near
            }),
            Intent::Wait
        );
        assert_eq!(
            m.step(&Facts {
                now_ms: t + EXIT_STUCK_MS + 1,
                ..near
            }),
            Intent::Fail(Fail::ExitStuck)
        );
    }

    #[test]
    fn out_members_leave_and_the_leader_dismisses_last() {
        // a member leaves the team, then is done
        let mut m = at_leave();
        m.step(&Facts {
            in_instance: false,
            leader: false,
            team: in_team(3, 0),
            ..facts(20_000)
        });
        assert_eq!(m.phase(), Phase::Out);
        let member = |now, in_team_| Facts {
            leader: false,
            team: TeamFacts {
                in_team: in_team_,
                led_by_leader: in_team_,
                size: if in_team_ { 3 } else { 0 },
                ..TeamFacts::default()
            },
            ..facts(now)
        };
        assert_eq!(m.step(&member(20_100, true)), Intent::LeaveTeam);
        assert_eq!(m.step(&member(20_200, true)), Intent::Wait);
        assert_eq!(m.step(&member(20_300, false)), Intent::Done);

        // the leader waits for the others, then dismisses
        let mut m = at_leave();
        m.step(&Facts {
            in_instance: false,
            team: in_team(3, 0),
            ..facts(20_000)
        });
        let lead = |now, size| Facts {
            leader: true,
            rank: 0,
            team: TeamFacts {
                in_team: true,
                led_by_leader: true,
                size,
                ..TeamFacts::default()
            },
            ..facts(now)
        };
        assert_eq!(m.step(&lead(20_100, 3)), Intent::Wait, "two are still in");
        assert_eq!(m.step(&lead(20_200, 1)), Intent::DismissTeam);
        assert_eq!(
            m.step(&Facts {
                team: TeamFacts::default(),
                ..lead(20_300, 0)
            }),
            Intent::Done
        );
        // a leader that waited long enough dismisses anyway
        let mut m = at_leave();
        m.step(&Facts {
            in_instance: false,
            team: in_team(3, 0),
            ..facts(20_000)
        });
        assert_eq!(
            m.step(&lead(20_000 + LEADER_OUT_WAIT_MS + 1, 3)),
            Intent::DismissTeam
        );
        // a team that cannot be left does not hold the bot for ever
        let mut m = at_leave();
        m.step(&Facts {
            in_instance: false,
            team: in_team(3, 0),
            ..facts(20_000)
        });
        assert_eq!(m.step(&member(20_000 + OUT_MS + 1, true)), Intent::Done);
    }
}
