//! The sail goal's state machine: get a scroll, pick a trip, walk to the launch beach, summon the
//! boat, board it, sail the route, land, despawn the boat. A pure function of its state and an
//! [`Input`] (what the bot knows this tick), returning [`Act`]s (what the bot should do), so the
//! whole trip is tested against a scripted server without a network or a [`Ctx`](crate::bot::ctx::Ctx).
//!
//! Phases (see [`Phase`]): `Gear` (the scroll), `Plan` (launch, landing), `Shore` (walk to the
//! launch beach), `Summon` (cast, wait for the slave), `Board` (swim to it, bind), `Sail` (probe the
//! helm, follow the route), `Land` (stop, leave, despawn the boat, come ashore) and `Abort` (the
//! way out of every failure: leave the boat, despawn it, go to the nearest beach). Nothing here
//! leaves a boat behind: every end of the goal passes through [`Machine::stop`].

use aa_proto::sc_travel::error_message as emsg;
use aa_world::travel::{HullSize, ShipClass};
use aa_world::water::{yaw_of, Disc, ShoreSpot};

use super::trip::{Plan, PlanError};
use crate::bot::motion::MotionStatus;
use crate::bot::state::StateError;
use crate::bot::vehicle::follow::{guide, Params};
use crate::bot::vehicle::helm::{self, throttle, wanted_speed, Cmd, Helm, Hull};
use crate::bot::vehicle::{wrap_pi, Sailor, Track};
use crate::bot::watch::classify::{refusal, skill_result, Refusal};
use crate::bot::watch::Finding;
use crate::issues::{IssueKind, IssueReport, Severity, Suspect};

use super::super::{FailReason, GoalStatus};

/// The server code the issue reports point at.
const AREA: &str = "rust/crates/aaemu-game/src/systems/vehicles/";
/// A boat must be at least this fast (m/s) to be worth the trip (the rowboat does 3.2).
pub const MIN_SPEED: f32 = 2.5;
/// A trip (all phases) ends after this long, or after eight minutes and two and a half times the
/// time the planned route takes at top speed, if that is longer.
const TRIP_LIMIT_MS: u64 = 40 * 60_000;
/// A boat that was already there at the start must be gone within this long.
const STALE_BOAT_MS: u64 = 15_000;
/// How long to wait for a GM-added scroll to show up in the bag.
const GEAR_WAIT_MS: u64 = 10_000;
/// The bot stands at most this far from the launch point when it summons.
const STAND_M: f32 = 3.0;
/// Pause after turning to the water, so that the server knows the facing.
const FACE_WAIT_MS: u64 = 800;
/// The summon skill's cooldown on the server.
const SUMMON_COOLDOWN_MS: u64 = 10_000;
/// Wait for the slave after the cast time.
const SUMMON_EXTRA_MS: u64 = 10_000;
/// Another beach is tried this many times when the server finds no room for the boat.
const MAX_RELAUNCH: u8 = 2;
/// The bot is this close (m, from the hull's centre) when it binds.
const BIND_REACH_M: f32 = 2.8;
/// A closer reach after the server said the bot was too far.
const BIND_REACH_CLOSE_M: f32 = 1.6;
/// The same for a hull bigger than a rowboat, as the gap to the hull box (the server measures the
/// reach to the box, less the bot's own radius).
const HULL_REACH_M: f32 = 1.4;
const HULL_REACH_CLOSE_M: f32 = 0.4;
/// Pause between two binds.
const BIND_RETRY_MS: u64 = 2500;
/// Binds tried.
const MAX_BINDS: u8 = 3;
/// Board phase limit.
const BOARD_LIMIT_MS: u64 = 60_000;
/// `SCSlaveBound` seen, but no seat after this long: report it.
const NO_ATTACH_MS: u64 = 3000;
/// Wait for the first ship movement after the bind (and the hull's portal time, during which the
/// server ignores the helm).
const FIRST_SAMPLE_MS: u64 = 6000;
/// The log line about a sailing trip comes this often.
const STATUS_MS: u64 = 60_000;
/// The stuck window: less than [`STUCK_MOVE_M`] of progress in this time with the engine on.
const STUCK_WINDOW_MS: u64 = 8000;
const STUCK_MOVE_M: f32 = 3.0;
/// Stuck episodes after which the trip is abandoned.
const MAX_STUCK: u8 = 3;
/// How long to back off after getting stuck (and a tenth of a second per metre of hull).
const REVERSE_MS: u64 = 4000;
/// The hull ran at this speed (m/s) a moment ago and now stands still with the engine on: it hit
/// the ground or a wall (the server's `stuck` flag is never set).
const IMPACT_SPEED: f32 = 1.5;
const IMPACT_WINDOW_MS: u64 = 6000;
/// The engine has run this long without any movement (and the server says nothing is wrong).
const NO_THRUST_MS: u64 = 5000;
/// Applied steering of at least this much with no turn at all for this long: the wheel is dead.
const DEAD_WHEEL_STEER: i8 = 60;
const DEAD_WHEEL_MS: u64 = 10_000;
/// Braking at the landing: at most this long, down to this speed.
const BRAKE_MS: u64 = 12_000;
const LAND_SPEED: f32 = 1.2;
/// Wait for the `SCUnitDetached` after a discard.
const DETACH_WAIT_MS: u64 = 6000;
/// Wait for the boat to be gone after a despawn.
const DESPAWN_WAIT_MS: u64 = 8000;
/// The way out of a failure: wait this long for the boat to be gone, and for the bot to be home.
const ABORT_BOAT_MS: u64 = 12_000;
const ABORT_HOME_MS: u64 = 120_000;
/// The route following parameters of the rowboat, which needs 84 m to turn round at full speed.
pub const PARAMS: Params = Params::basic(70.0, 40.0, 18.0);

/// The phases of a trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Get a scroll.
    Gear,
    /// Choose the launch beach, the landing and the way.
    Plan,
    /// Walk to the launch beach.
    Shore,
    /// Cast the summon, wait for the boat.
    Summon,
    /// Swim to the boat and sit down.
    Board,
    /// Steer along the route.
    Sail,
    /// Stop, leave the boat, despawn it, come ashore.
    Land,
    /// The way out of a failure.
    Abort,
}

impl Phase {
    /// Whether the bot is on or at the boat (the goal then owns the combat decisions and the bot
    /// ticks fast).
    pub fn at_sea(self) -> bool {
        matches!(
            self,
            Phase::Board | Phase::Sail | Phase::Land | Phase::Abort
        )
    }
}

/// The scroll chosen from the data.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pick {
    /// Scroll item template.
    pub item: u32,
    /// Its use skill.
    pub skill: u32,
    /// The skill's cast time.
    pub cast_ms: u32,
    /// The boat it summons.
    pub class: ShipClass,
}

impl Pick {
    /// The boat's hull, for the helm.
    pub fn hull(&self) -> Hull {
        Hull::of_class(&self.class)
    }

    /// The route following parameters of the boat: the rowboat's own, the hull's for the others.
    pub fn params(&self) -> Params {
        let hull = self.hull();
        if self.class.size == HullSize::Small {
            Params {
                arrive: PARAMS.arrive.max(0.6 * self.class.length),
                ..Params::for_hull(&hull, self.class.length, false)
            }
        } else {
            Params::for_hull(&hull, self.class.length, true)
        }
    }
}

/// What the bot knows this tick.
pub struct Input<'a> {
    /// The bot clock.
    pub now_ms: u64,
    /// The bot's character id.
    pub char_id: u32,
    /// The bot's position.
    pub pos: [f32; 3],
    /// GM commands are allowed.
    pub gm: bool,
    /// The scroll the trip uses.
    pub pick: Pick,
    /// The scroll in the bag now: item id and template.
    pub held: Option<(u64, u32)>,
    /// The sailor (None before any boat event or sail goal; the goal creates it at its start).
    pub sailor: Option<&'a Sailor>,
    /// The boat's unit in view: its position.
    pub boat_unit: Option<[f32; 3]>,
    /// Recent server errors, newest last.
    pub errors: &'a [(u64, StateError)],
    /// What the movement controller is doing.
    pub motion: MotionStatus,
    /// The planner and the map.
    pub ops: &'a dyn Ops,
}

/// What the machine asks the bot to do, in order.
#[derive(Debug, Clone)]
pub enum Act {
    /// A line for the bot's event log.
    Say(String),
    /// A GM command line (`/item add self ...`).
    Gm(String),
    /// Walk (or `/move`) to a point, stopping `radius` metres short.
    Travel { x: f32, y: f32, radius: f32 },
    /// Turn towards a point.
    Face { x: f32, y: f32 },
    /// Stop walking.
    StopMotion,
    /// The bot is where the boat is: place the movement controller there.
    Sync([f32; 3]),
    /// A trip on foot or by boat is on: the combat controller only defends against real attackers.
    Travelling,
    /// An exact position update before the next packet.
    Pin,
    /// Cast the summon scroll on the bot itself.
    Summon {
        skill: u32,
        item: u64,
        template: u32,
    },
    /// Take the driver's seat of the slave with timeline id `tl`.
    Bind(u16),
    /// Leave the seat on the slave with timeline id `tl`.
    Discard(u16),
    /// Despawn the slave `obj`.
    Despawn(u32),
    /// The helm request for ship `ship`.
    Helm { ship: u32, cmd: Cmd },
    /// File a report about the server.
    Report(Box<IssueReport>),
    /// Leave a finding of the standard detectors.
    Find(Finding),
}

/// The world-side services the machine needs (the real ones use the map and the NPC data).
pub trait Ops {
    /// Closed areas around hostile sea monsters near `from`.
    fn danger(&self, from: (f32, f32)) -> Vec<Disc>;
    /// A trip for a bot at `from`.
    fn plan(&self, from: (f32, f32), avoid: &[Disc]) -> Result<Plan, PlanError>;
    /// The waypoints from the boat at `start` to the plan's landing water.
    fn route(&self, start: (f32, f32), plan: &Plan, avoid: &[Disc]) -> Option<Vec<(f32, f32)>>;
    /// The nearest beach to `from` within `max_r` metres.
    fn shore_near(&self, from: (f32, f32), max_r: f32) -> Option<ShoreSpot>;
    /// Terrain height, for "is the boat in deep water".
    fn ground(&self, x: f32, y: f32) -> Option<f32>;
}

/// What the machine reports back.
pub type Step = GoalStatus;

/// The state of a trip. See the module docs.
#[derive(Debug)]
pub struct Machine {
    phase: Phase,
    since_ms: u64,
    started_ms: u64,
    home: (f32, f32),
    plan: Option<Plan>,
    danger: Vec<Disc>,
    tried: Vec<Disc>,
    relaunch: u8,
    // gear
    gm_asked_ms: u64,
    stale_ms: u64,
    // shore
    shore_tries: u8,
    // summon
    face_ms: u64,
    cast_ms: u64,
    next_cast_ms: u64,
    casts: u8,
    // trip
    limit_ms: u64,
    params: Params,
    // board
    face_yaw: f32,
    approach_ms: u64,
    bind_ms: u64,
    binds: u8,
    reach: f32,
    bound_seen_ms: u64,
    // errors up to this bot-clock time (exclusive) are handled
    err_cursor_ms: u64,
    reported: u16,
    // sail
    route: Vec<(f32, f32)>,
    leg: usize,
    dist_end: f32,
    helm: Helm,
    last_cmd: Option<(Cmd, u64)>,
    sail_ms: u64,
    status_ms: u64,
    top_seen: f32,
    win: ((f32, f32), u64),
    stuck: u8,
    reverse_until_ms: u64,
    thrust_ms: u64,
    dead_wheel_ms: u64,
    fast_ms: u64,
    sample_wait_ms: u64,
    // land
    brake_ms: u64,
    discard_ms: u64,
    discards: u8,
    despawn_ms: u64,
    despawns: u8,
    ashore_tried: bool,
    // abort
    abort: Option<FailReason>,
    homing_ms: u64,
    /// Whether any phase beyond planning ran (the trip counts as one).
    pub sailed: bool,
}

/// Bits of [`Machine::reported`].
mod once {
    pub const NO_SLAVE: u16 = 1;
    pub const NO_BIND: u16 = 2;
    pub const NO_ATTACH: u16 = 4;
    pub const NO_THRUST: u16 = 8;
    pub const DESPAWN: u16 = 16;
    pub const DISCARD: u16 = 32;
    pub const DEAD_WHEEL: u16 = 64;
    pub const NO_SAMPLE: u16 = 128;
    pub const NO_PROBE: u16 = 256;
    pub const BIND_REACH: u16 = 512;
    pub const SLOW: u16 = 1024;
}

fn dist2(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn xy(p: [f32; 3]) -> (f32, f32) {
    (p[0], p[1])
}

fn issue(
    kind: IssueKind,
    key: String,
    title: &str,
    expected: &str,
    actual: String,
    suspect: Suspect,
) -> IssueReport {
    IssueReport::new(kind, key, title)
        .expected(expected)
        .actual(actual)
        .suspect(suspect)
        .severity(Severity::Medium)
        .area(AREA)
}

impl Machine {
    pub(super) fn underway(&self) -> bool {
        self.phase == Phase::Sail
    }

    /// A trip starting at bot clock `now_ms` from `pos`.
    pub fn new(now_ms: u64, pos: [f32; 3]) -> Machine {
        Machine {
            phase: Phase::Gear,
            since_ms: now_ms,
            started_ms: now_ms,
            home: xy(pos),
            plan: None,
            danger: Vec::new(),
            tried: Vec::new(),
            relaunch: 0,
            limit_ms: TRIP_LIMIT_MS,
            params: PARAMS,
            face_yaw: 0.0,
            gm_asked_ms: 0,
            stale_ms: 0,
            shore_tries: 0,
            face_ms: 0,
            cast_ms: 0,
            next_cast_ms: 0,
            casts: 0,
            approach_ms: 0,
            bind_ms: 0,
            binds: 0,
            reach: BIND_REACH_M,
            bound_seen_ms: 0,
            err_cursor_ms: now_ms,
            reported: 0,
            route: Vec::new(),
            leg: 0,
            dist_end: 0.0,
            helm: Helm::default(),
            last_cmd: None,
            sail_ms: 0,
            status_ms: 0,
            top_seen: 0.0,
            win: ((0.0, 0.0), 0),
            stuck: 0,
            reverse_until_ms: 0,
            thrust_ms: 0,
            dead_wheel_ms: 0,
            fast_ms: 0,
            sample_wait_ms: 0,
            brake_ms: 0,
            discard_ms: 0,
            discards: 0,
            despawn_ms: 0,
            despawns: 0,
            ashore_tried: false,
            abort: None,
            homing_ms: 0,
            sailed: false,
        }
    }

    /// The current phase.
    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The planned trip, once there is one.
    #[cfg(test)]
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// The helm (for the tests).
    #[cfg(test)]
    pub fn helm(&self) -> &Helm {
        &self.helm
    }

    fn enter(&mut self, phase: Phase, now_ms: u64) {
        self.phase = phase;
        self.since_ms = now_ms;
    }

    fn once(&mut self, bit: u16) -> bool {
        let first = self.reported & bit == 0;
        self.reported |= bit;
        first
    }

    /// One tick.
    pub fn step(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        if self.phase != Phase::Abort && now.saturating_sub(self.started_ms) > self.limit_ms {
            return self.abort(io, acts, FailReason::Timeout);
        }
        if self.phase.at_sea() {
            acts.push(Act::Travelling);
        }
        let step = match self.phase {
            Phase::Gear => self.gear(io, acts),
            Phase::Plan => self.plan_phase(io, acts),
            Phase::Shore => self.shore(io, acts),
            Phase::Summon => self.summon(io, acts),
            Phase::Board => self.board(io, acts),
            Phase::Sail => self.sail(io, acts),
            Phase::Land => self.land(io, acts),
            Phase::Abort => self.abort_step(io, acts),
        };
        self.err_cursor_ms = now + 1;
        step
    }

    // ---- gear ------------------------------------------------------------------------------

    fn gear(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        // a boat left over from an earlier trip, or from a cast that was interrupted: out of the
        // way first (the bot only ever has one)
        if let Some(b) = io.sailor.and_then(|s| s.boat.as_ref()) {
            if now.saturating_sub(self.started_ms) > STALE_BOAT_MS {
                return GoalStatus::Failed(FailReason::Timeout);
            }
            if self.stale_ms == 0 || now - self.stale_ms >= 4000 {
                if self.stale_ms == 0 {
                    acts.push(Act::Say(format!(
                        "sail: removing the boat {} left over",
                        b.obj
                    )));
                }
                self.stale_ms = now.max(1);
                if io.sailor.is_some_and(|s| s.my_seat_on_boat() != 0) {
                    acts.push(Act::Discard(b.tl));
                }
                if !b.despawning {
                    acts.push(Act::Despawn(b.obj));
                }
            }
            return GoalStatus::Running;
        }
        if io.held.is_some() {
            self.enter(Phase::Plan, now);
            return GoalStatus::Running;
        }
        if !io.gm {
            return GoalStatus::Failed(FailReason::NoTarget);
        }
        if self.gm_asked_ms == 0 {
            acts.push(Act::Gm(format!("/item add self {} 1", io.pick.item)));
            self.gm_asked_ms = now.max(1);
        } else if now.saturating_sub(self.gm_asked_ms) > GEAR_WAIT_MS {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        GoalStatus::Running
    }

    // ---- plan ------------------------------------------------------------------------------

    fn plan_phase(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let from = xy(io.pos);
        if self.danger.is_empty() {
            self.danger = io.ops.danger(from);
        }
        let mut avoid = self.danger.clone();
        avoid.extend(self.tried.iter().copied());
        match io.ops.plan(from, &avoid) {
            Ok(plan) => {
                acts.push(Act::Say(format!(
                    "sail: {} of {:.0} m from {:.0}, {:.0}",
                    match plan.kind {
                        super::trip::Kind::Cross => "crossing",
                        super::trip::Kind::Loop { .. } => "loop",
                    },
                    plan.route_m,
                    plan.launch.stand.0,
                    plan.launch.stand.1
                )));
                let sail_ms = (plan.route_m / io.pick.class.top_speed.max(0.5) * 2500.0) as u64;
                self.limit_ms = TRIP_LIMIT_MS.max(8 * 60_000 + sail_ms);
                self.plan = Some(plan);
                self.shore_tries = 0;
                self.enter(Phase::Shore, io.now_ms);
                GoalStatus::Running
            }
            Err(PlanError::NoCoast) => {
                acts.push(Act::Say(
                    "sail: no launch beach with room for the boat near".into(),
                ));
                GoalStatus::Failed(FailReason::Other("no coast near"))
            }
            Err(PlanError::NoTrip) => {
                acts.push(Act::Say("sail: no trip from here".into()));
                GoalStatus::Failed(FailReason::Other("no trip from here"))
            }
        }
    }

    // ---- shore -----------------------------------------------------------------------------

    fn shore(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let Some(plan) = self.plan.as_ref() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        let (stand, water) = (plan.launch.stand, plan.launch.water);
        acts.push(Act::Travelling);
        let d = dist2(xy(io.pos), stand);
        if io.motion == MotionStatus::Moving {
            return GoalStatus::Running;
        }
        if d <= STAND_M {
            acts.push(Act::Face {
                x: water.0,
                y: water.1,
            });
            self.face_ms = 0;
            self.enter(Phase::Summon, io.now_ms);
            return GoalStatus::Running;
        }
        if self.shore_tries >= 3 {
            return GoalStatus::Failed(FailReason::Stuck);
        }
        self.shore_tries += 1;
        acts.push(Act::Travel {
            x: stand.0,
            y: stand.1,
            radius: 1.0,
        });
        GoalStatus::Running
    }

    // ---- summon ----------------------------------------------------------------------------

    fn summon(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        let Some(plan) = self.plan.as_ref() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        let water = plan.launch.water;
        if io.sailor.is_some_and(|s| s.live_boat().is_some()) {
            self.sailed = true;
            self.reach = BIND_REACH_M;
            self.enter(Phase::Board, now);
            acts.push(Act::Say("sail: the boat is here".into()));
            return GoalStatus::Running;
        }
        if self.face_ms == 0 {
            self.face_ms = now.max(1);
            acts.push(Act::Face {
                x: water.0,
                y: water.1,
            });
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.face_ms) < FACE_WAIT_MS || now < self.next_cast_ms {
            return GoalStatus::Running;
        }
        if self.cast_ms == 0 {
            let Some((item, template)) = io.held else {
                // the scroll is gone (used by a cast the bot lost track of)
                return GoalStatus::Failed(FailReason::Other("the scroll is gone"));
            };
            // (the server lays the hull across the caster's view, bow to the caster's left)
            self.face_yaw = yaw_of(water.0 - io.pos[0], water.1 - io.pos[1]);
            acts.push(Act::Pin);
            acts.push(Act::Summon {
                skill: io.pick.skill,
                item,
                template,
            });
            self.cast_ms = now.max(1);
            self.casts += 1;
            return GoalStatus::Running;
        }
        // the server's answers to the cast
        let from = self.err_cursor_ms.max(self.cast_ms);
        for &(_, e) in io.errors.iter().filter(|(t, _)| *t >= from) {
            match e {
                StateError::Skill { skill, result } if skill == io.pick.skill => {
                    if result == skill_result::INVALID_LOCATION {
                        return self.relaunch(io, acts, "the server refused the place");
                    }
                    if refusal(result) == Refusal::Unexpected {
                        acts.push(Act::Find(Finding::SkillRefused {
                            skill,
                            code: result,
                        }));
                    }
                    return GoalStatus::Failed(FailReason::Other("the summon was refused"));
                }
                StateError::Msg(m) if m == emsg::SLAVE_SPAWN_SHIP_NEED_MORE_SPACE || m == 583 => {
                    return self.relaunch(io, acts, "no room for the boat here");
                }
                StateError::Msg(m) if m == emsg::SLAVE_SPAWN_ITEM_LOCKED => {
                    return GoalStatus::Failed(FailReason::Other("the scroll is locked"));
                }
                StateError::Msg(m) => self.unexpected_error(m, "the summon", io, acts),
                _ => {}
            }
        }
        let wait = u64::from(io.pick.cast_ms) + SUMMON_EXTRA_MS;
        if now.saturating_sub(self.cast_ms) > wait {
            if self.once(once::NO_SLAVE) {
                acts.push(Act::Report(Box::new(issue(
                    IssueKind::NoResponse,
                    format!("sail:no_slave:{}", io.pick.skill),
                    "A boat summon scroll was cast and no boat appeared",
                    "SCSlaveCreated and the slave's unit state after the cast time",
                    format!(
                        "no slave {} ms after the cast of skill {} with item {}, no refusal",
                        now - self.cast_ms,
                        io.pick.skill,
                        io.pick.item
                    ),
                    Suspect::Server,
                ))));
            }
            return GoalStatus::Failed(FailReason::Timeout);
        }
        GoalStatus::Running
    }

    /// The place was refused: try another beach (a few times).
    fn relaunch(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, why: &str) -> Step {
        let Some(plan) = self.plan.as_ref() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        self.tried.push(Disc {
            x: plan.launch.stand.0,
            y: plan.launch.stand.1,
            r: 60.0 + io.pick.class.length,
        });
        self.relaunch += 1;
        if self.relaunch > MAX_RELAUNCH {
            return GoalStatus::Failed(FailReason::Other("no room to summon a boat"));
        }
        acts.push(Act::Say(format!("sail: {why}; trying another beach")));
        self.cast_ms = 0;
        self.face_ms = 0;
        self.next_cast_ms = io.now_ms + SUMMON_COOLDOWN_MS;
        self.enter(Phase::Plan, io.now_ms);
        GoalStatus::Running
    }

    fn unexpected_error(&mut self, code: i16, what: &str, io: &Input<'_>, acts: &mut Vec<Act>) {
        // (one report per code and bot is enough: the book dedups across bots)
        acts.push(Act::Report(Box::new(
            issue(
                IssueKind::Other,
                format!("sail:error_code:{code}"),
                "An unexpected error message answered a boat action",
                "no error message",
                format!("SCErrorMsg {code} after {what}"),
                Suspect::Unknown,
            )
            .severity(Severity::Low)
            .fact("error", code)
            .fact("phase", format!("{:?}", self.phase))
            .fact("item", io.pick.item),
        )));
    }

    // ---- board -----------------------------------------------------------------------------

    fn board(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        let Some(s) = io.sailor else {
            return GoalStatus::Failed(FailReason::Other("no boat"));
        };
        let Some(boat) = s.live_boat() else {
            return self.abort(io, acts, FailReason::Other("the boat is gone"));
        };
        if s.my_seat_on_boat() == 1 {
            self.enter(Phase::Sail, now);
            self.sail_ms = now;
            self.sample_wait_ms = 0;
            self.helm = Helm::new(io.pick.hull());
            self.params = io.pick.params();
            acts.push(Act::StopMotion);
            acts.push(Act::Say(format!("sail: at the helm of boat {}", boat.obj)));
            return GoalStatus::Running;
        }
        if let Some(seat) = s.seat {
            // somewhere else on a boat (a leftover seat): leave it first
            if let Some(f) = s.foreign.filter(|f| f.obj == seat.parent) {
                acts.push(Act::Discard(f.tl));
            }
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.since_ms) > BOARD_LIMIT_MS {
            return self.abort(io, acts, FailReason::Timeout);
        }
        // answers to a bind
        if self.bind_ms != 0 {
            let from = self.err_cursor_ms.max(self.bind_ms);
            for &(_, e) in io.errors.iter().filter(|(t, _)| *t >= from) {
                match e {
                    StateError::Msg(m)
                        if m == emsg::SLAVE_CANNOT_BIND || m == emsg::TOO_FAR_AWAY =>
                    {
                        // too far for the server: get closer than the first try
                        if self.reach == BIND_REACH_CLOSE_M && self.once(once::BIND_REACH) {
                            // (already as close as the bot can swim to the hull's middle: the
                            // bot's idea of the distance is wrong)
                            acts.push(Act::Report(Box::new(
                                issue(
                                    IssueKind::Other,
                                    "sail:bind_reach".into(),
                                    "The server says the bot is too far from a boat it stands next to",
                                    "a bind within a few metres of the hull to be accepted",
                                    format!(
                                        "error {m} with the bot {:.1} m from the hull's reported position",
                                        dist2(xy(io.pos), boat_xy(io, s).unwrap_or(xy(io.pos)))
                                    ),
                                    Suspect::Bot,
                                )
                                .severity(Severity::Low),
                            )));
                        }
                        self.reach = BIND_REACH_CLOSE_M;
                    }
                    StateError::Msg(m) if m == emsg::SLAVE_ALREADY_HAS_MASTER => {
                        return self.abort(io, acts, FailReason::Other("the seat is taken"));
                    }
                    StateError::Msg(m) if m == emsg::SLAVE_UNBIND_FIRST => {
                        acts.push(Act::Discard(boat.tl));
                    }
                    StateError::Msg(m) => self.unexpected_error(m, "the bind", io, acts),
                    _ => {}
                }
            }
        }
        // the server said it bound the bot, but no seat came
        if boat.driver_char != 0 && boat.driver_char == io.char_id {
            if self.bound_seen_ms == 0 {
                self.bound_seen_ms = now.max(1);
            } else if now - self.bound_seen_ms > NO_ATTACH_MS && self.once(once::NO_ATTACH) {
                acts.push(Act::Report(Box::new(issue(
                    IssueKind::NoResponse,
                    "sail:bind_no_attach".into(),
                    "A bound boat seat never came with an attach packet",
                    "SCUnitAttached for the bot before or with SCSlaveBound",
                    format!("SCSlaveBound named the bot, no SCUnitAttached in {NO_ATTACH_MS} ms"),
                    Suspect::Server,
                ))));
            }
        }
        let Some(bpos) = boat_xy(io, s) else {
            // (the boat's place is not known yet: the summoner's record has not come)
            return GoalStatus::Running;
        };
        // a rowboat is reached at its middle; a bigger hull is boarded from its side: the
        // server measures the bind reach to the hull box
        let (d, aim, reach) = if io.pick.class.size == HullSize::Small {
            (dist2(xy(io.pos), bpos), bpos, self.reach)
        } else {
            let (gap, near) = hull_gap(&io.pick.class, bpos, self.face_yaw, xy(io.pos));
            let reach = if self.reach == BIND_REACH_CLOSE_M {
                HULL_REACH_CLOSE_M
            } else {
                HULL_REACH_M
            };
            (gap, near, reach)
        };
        if d > reach {
            if now.saturating_sub(self.approach_ms) >= 1200 || self.approach_ms == 0 {
                acts.push(Act::Travel {
                    x: aim.0,
                    y: aim.1,
                    radius: 0.5,
                });
                self.approach_ms = now.max(1);
            }
            return GoalStatus::Running;
        }
        if self.bind_ms == 0 || now.saturating_sub(self.bind_ms) >= BIND_RETRY_MS {
            if self.binds >= MAX_BINDS {
                if self.once(once::NO_BIND) {
                    acts.push(Act::Report(Box::new(issue(
                        IssueKind::NoResponse,
                        "sail:bind_no_response".into(),
                        "A boat bind got no answer",
                        "SCUnitAttached and SCSlaveBound, or an error message",
                        format!(
                            "{} binds within {:.1} m of the hull in {} ms, nothing came back",
                            self.binds,
                            d,
                            now - self.since_ms
                        ),
                        Suspect::Unknown,
                    ))));
                }
                return self.abort(io, acts, FailReason::Other("the bind got no answer"));
            }
            acts.push(Act::StopMotion);
            acts.push(Act::Pin);
            acts.push(Act::Bind(boat.tl));
            self.bind_ms = now.max(1);
            self.binds += 1;
        }
        GoalStatus::Running
    }

    // ---- sail ------------------------------------------------------------------------------

    fn sail(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        let Some(s) = io.sailor else {
            return GoalStatus::Failed(FailReason::Other("no boat"));
        };
        let Some(boat) = s.live_boat() else {
            return self.abort(io, acts, FailReason::Other("the boat is gone"));
        };
        if s.my_seat_on_boat() != 1 {
            return self.abort(io, acts, FailReason::Other("left the boat"));
        }
        let ship = boat.obj;
        let Some(tr) = s.track.filter(|t| t.cur.obj == ship) else {
            // no movement report yet: get the engine going, so that the ship reports
            if self.sample_wait_ms == 0 {
                self.sample_wait_ms = now.max(1);
                self.send(
                    acts,
                    ship,
                    Cmd {
                        throttle: 60,
                        steering: 0,
                    },
                    now,
                );
            } else if now - self.sample_wait_ms
                > FIRST_SAMPLE_MS + u64::from(io.pick.class.portal_ms)
            {
                if self.once(once::NO_SAMPLE) {
                    acts.push(Act::Report(Box::new(issue(
                        IssueKind::NoResponse,
                        "sail:no_ship_movement".into(),
                        "A driven boat never reported its movement",
                        "SCOneUnitMovement of the ship after a throttle request",
                        format!(
                            "no ship movement {} ms after the first request",
                            now - self.sample_wait_ms
                        ),
                        Suspect::Server,
                    ))));
                }
                return self.abort(io, acts, FailReason::Other("the boat does not report"));
            }
            return GoalStatus::Running;
        };
        let p = tr.cur.pos;
        let here = (p.0, p.1);
        acts.push(Act::Sync([p.0, p.1, p.2]));

        // the route, from where the boat is
        if self.route.is_empty() {
            let Some(plan) = self.plan.as_ref() else {
                return self.abort(io, acts, FailReason::Other("no plan"));
            };
            match io.ops.route(here, plan, &self.danger) {
                Some(r) => {
                    self.route = r;
                    self.leg = 0;
                    self.win = (here, now);
                    acts.push(Act::Say(format!(
                        "sail: {} waypoints, {:.0} m",
                        self.route.len(),
                        route_len(&self.route)
                    )));
                }
                None => return self.abort(io, acts, FailReason::Other("no sea route")),
            }
        }

        // the probe: which way does the wheel turn
        if let Some(cmd) = self.helm.probe(now, tr.cur.yaw) {
            self.send(acts, ship, cmd, now);
            return GoalStatus::Running;
        }
        if self.helm.probed()
            && self.sail_ms != 0
            && !self.helm.known()
            && self.once(once::NO_PROBE)
        {
            acts.push(Act::Say(
                "sail: the probe saw no turn; steering with the documented sign".into(),
            ));
        }

        let g = guide(&self.route, &mut self.leg, here, &self.params);
        self.dist_end = g.dist_end;
        if g.arrived {
            self.brake_ms = 0;
            self.enter(Phase::Land, now);
            if self.top_seen < 0.75 * io.pick.class.top_speed
                && route_len(&self.route) > 600.0
                && self.once(once::SLOW)
            {
                let template = io
                    .sailor
                    .and_then(|s| s.boat.as_ref())
                    .map_or(0, |b| b.template);
                acts.push(Act::Report(Box::new(
                    issue(
                        IssueKind::Other,
                        format!("sail:slow_boat:{}", io.pick.item),
                        "A boat is slower than its data says",
                        "a top speed near ship_models.velocity * MoveSpeedMul^2 / 8",
                        format!(
                            "top speed seen {:.1} m/s over a trip of {:.0} m, the data says {:.1} m/s",
                            self.top_seen,
                            route_len(&self.route),
                            io.pick.class.top_speed
                        ),
                        Suspect::Unknown,
                    )
                    .severity(Severity::Low)
                    .fact("template", template)
                    .fact("item", io.pick.item),
                )));
            }
            acts.push(Act::Say(format!(
                "sail: arrived (sign {}, flips {}, top speed seen {:.1} m/s)",
                self.helm.sign(),
                self.helm.flips(),
                self.top_seen
            )));
            return GoalStatus::Running;
        }
        let fwd = tr.cur.forward_speed();
        self.top_seen = self.top_seen.max(fwd);
        let err = wrap_pi(g.target_yaw - tr.cur.yaw);
        let rate = tr.yaw_rate();
        let hull = io.pick.hull();
        let mut cmd = Cmd {
            throttle: throttle(fwd, wanted_speed(&hull, err, g.v_limit)),
            steering: self.helm.steer(err, rate, fwd),
        };
        if self.helm.watch(now, err, cmd.steering, rate) {
            acts.push(Act::Say(format!(
                "sail: the wheel turned the other way; steering sign {}",
                self.helm.sign()
            )));
        }
        if now.saturating_sub(self.status_ms) >= STATUS_MS {
            self.status_ms = now;
            acts.push(Act::Say(format!(
                "sail: {:.1} m/s, throttle {} steering {}, {:.0} deg off, {:.0} m to go",
                fwd,
                cmd.throttle,
                cmd.steering,
                err.to_degrees(),
                g.dist_end
            )));
        }
        // stuck: no progress with the engine on
        if let Some(step) = self.stuck_check(io, acts, &tr, here, &mut cmd) {
            return step;
        }
        self.dead_wheel_check(io, acts, &tr);
        self.send(acts, ship, cmd, now);
        GoalStatus::Running
    }

    /// Detects a boat that does not get anywhere and backs it off. `Some` ends the tick.
    fn stuck_check(
        &mut self,
        io: &Input<'_>,
        acts: &mut Vec<Act>,
        tr: &Track,
        here: (f32, f32),
        cmd: &mut Cmd,
    ) -> Option<Step> {
        let now = io.now_ms;
        if now < self.reverse_until_ms {
            // backing off: straight astern
            *cmd = Cmd {
                throttle: -80,
                steering: 0,
            };
            return None;
        }
        let engine_on = tr.cur.throttle >= 40;
        // the server never says the hull is aground: it ran, now it stands, with the engine on
        let fwd = tr.cur.forward_speed();
        if fwd >= IMPACT_SPEED {
            self.fast_ms = now.max(1);
        }
        let impact = engine_on
            && fwd < 0.25
            && self.fast_ms != 0
            && now - self.fast_ms <= IMPACT_WINDOW_MS
            && now - self.fast_ms >= 500;
        // the engine runs and nothing moves, and the server does not say the hull is aground
        if engine_on && tr.cur.speed < 0.3 && !tr.cur.stuck {
            if self.thrust_ms == 0 {
                self.thrust_ms = now.max(1);
            } else if now - self.thrust_ms >= NO_THRUST_MS
                && io.ops.ground(here.0, here.1).is_some_and(|z| z < 96.0)
                && self.once(once::NO_THRUST)
            {
                acts.push(Act::Report(Box::new(
                    issue(
                        IssueKind::Other,
                        "sail:no_thrust".into(),
                        "A boat with its engine on does not move in deep water",
                        "the hull accelerates with the throttle applied",
                        format!(
                            "throttle {} applied for {} ms, speed {:.2} m/s, terrain {:.0} m, not aground",
                            tr.cur.throttle,
                            now - self.thrust_ms,
                            tr.cur.speed,
                            io.ops.ground(here.0, here.1).unwrap_or(0.0)
                        ),
                        Suspect::Server,
                    )
                    .fact("template", io.sailor.and_then(|s| s.boat.as_ref()).map_or(0, |b| b.template)),
                )));
            }
        } else {
            self.thrust_ms = 0;
        }
        let window_ms = if io.pick.class.size == HullSize::Small {
            STUCK_WINDOW_MS
        } else {
            STUCK_WINDOW_MS + 4000
        };
        let windowed = now.saturating_sub(self.win.1) >= window_ms;
        let aground = tr.cur.stuck || impact;
        if windowed || aground {
            let moved = dist2(here, self.win.0);
            self.win = (here, now);
            if (engine_on && moved < STUCK_MOVE_M && windowed) || aground {
                self.stuck += 1;
                if self.stuck > MAX_STUCK {
                    return Some(self.abort(io, acts, FailReason::Stuck));
                }
                acts.push(Act::Say(format!(
                    "sail: stuck at {:.0}, {:.0} (episode {}); backing off",
                    here.0, here.1, self.stuck
                )));
                self.reverse_until_ms = now + REVERSE_MS + (io.pick.class.length * 100.0) as u64;
                self.fast_ms = 0;
                // the way may be blocked for good: plan again from here after the back-off
                self.route.clear();
                *cmd = Cmd {
                    throttle: -80,
                    steering: 0,
                };
            }
        }
        None
    }

    /// A wheel held hard over for a long while with a hull that does not turn at all.
    fn dead_wheel_check(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, tr: &Track) {
        let now = io.now_ms;
        let hard = tr.cur.steering.unsigned_abs() >= DEAD_WHEEL_STEER as u8
            && tr.cur.forward_speed() > 2.0
            && tr.yaw_rate().abs() < 0.003;
        if !hard {
            self.dead_wheel_ms = 0;
        } else if self.dead_wheel_ms == 0 {
            self.dead_wheel_ms = now.max(1);
        } else if now - self.dead_wheel_ms >= DEAD_WHEEL_MS && self.once(once::DEAD_WHEEL) {
            acts.push(Act::Report(Box::new(issue(
                IssueKind::Other,
                "sail:dead_wheel".into(),
                "A boat does not turn with the helm hard over",
                "the heading changes while a steering value is applied at speed",
                format!(
                    "steering {} applied for {} ms at {:.1} m/s, yaw rate {:.4} rad/s",
                    tr.cur.steering,
                    now - self.dead_wheel_ms,
                    tr.cur.forward_speed(),
                    tr.yaw_rate()
                ),
                Suspect::Server,
            ))));
        }
    }

    /// Sends a helm request when it is worth a packet (a change or the heartbeat).
    fn send(&mut self, acts: &mut Vec<Act>, ship: u32, cmd: Cmd, now: u64) {
        if helm::worth_sending(self.last_cmd, cmd, now) {
            acts.push(Act::Helm { ship, cmd });
            self.last_cmd = Some((cmd, now));
        }
    }

    // ---- land ------------------------------------------------------------------------------

    fn land(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        let Some(plan) = self.plan.as_ref() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        let stand = plan.landing.stand;
        let s = io.sailor;
        let seated = s.is_some_and(|s| s.my_seat_on_boat() != 0);
        let boat = s.and_then(|s| s.boat.as_ref());
        if seated {
            let (Some(s), Some(boat)) = (s, boat) else {
                return GoalStatus::Running;
            };
            let tr = s.track.filter(|t| t.cur.obj == boat.obj);
            if let Some(tr) = &tr {
                let p = tr.cur.pos;
                acts.push(Act::Sync([p.0, p.1, p.2]));
            }
            if self.brake_ms == 0 {
                self.brake_ms = now.max(1);
            }
            if self.discard_ms == 0 {
                let fwd = tr.map_or(0.0, |t| t.cur.forward_speed());
                if fwd > LAND_SPEED && now - self.brake_ms < BRAKE_MS {
                    // brake: astern while fast, then let her glide
                    let cmd = Cmd {
                        throttle: if fwd > 2.5 { -60 } else { 0 },
                        steering: 0,
                    };
                    self.send(acts, boat.obj, cmd, now);
                    return GoalStatus::Running;
                }
                self.send(
                    acts,
                    boat.obj,
                    Cmd {
                        throttle: 0,
                        steering: 0,
                    },
                    now,
                );
                acts.push(Act::Discard(boat.tl));
                self.discard_ms = now.max(1);
                self.discards = 1;
                return GoalStatus::Running;
            }
            if now - self.discard_ms > DETACH_WAIT_MS {
                if self.discards >= 2 {
                    if self.once(once::DISCARD) {
                        acts.push(Act::Report(Box::new(issue(
                            IssueKind::NoResponse,
                            "sail:discard_ignored".into(),
                            "Leaving a boat seat got no answer",
                            "SCUnitDetached after CSDiscardSlave",
                            format!("two discards, no detach in {} ms", now - self.discard_ms),
                            Suspect::Server,
                        ))));
                    }
                    return self.abort(io, acts, FailReason::Other("cannot leave the boat"));
                }
                acts.push(Act::Discard(boat.tl));
                self.discards = 2;
                self.discard_ms = now;
            }
            return GoalStatus::Running;
        }
        // out of the seat: get rid of the boat and walk ashore
        if let Some(b) = boat {
            if !b.despawning && self.despawn_ms == 0 {
                acts.push(Act::Despawn(b.obj));
                self.despawn_ms = now.max(1);
                self.despawns = 1;
            }
        }
        if self.despawn_ms != 0 && boat.is_some() && now - self.despawn_ms > DESPAWN_WAIT_MS {
            if self.despawns >= 2 {
                if self.once(once::DESPAWN) {
                    acts.push(Act::Report(Box::new(issue(
                        IssueKind::NoResponse,
                        "sail:despawn_ignored".into(),
                        "Despawning a boat got no answer",
                        "SCSlaveDespawn and SCSlaveRemoved after CSDespawnSlave",
                        format!(
                            "two despawns, the boat still there {} ms later",
                            now - self.despawn_ms
                        ),
                        Suspect::Server,
                    ))));
                }
                // the server removes it when the bot leaves; do not wait any longer
                acts.push(Act::Say("sail: the boat did not despawn".into()));
            } else if let Some(b) = boat {
                acts.push(Act::Despawn(b.obj));
                self.despawns = 2;
                self.despawn_ms = now;
            }
        }
        let boat_gone =
            boat.is_none() || self.despawns >= 2 && now - self.despawn_ms > DESPAWN_WAIT_MS;
        let d = dist2(xy(io.pos), stand);
        match io.motion {
            MotionStatus::Moving => {}
            _ if d <= 4.0 => {
                if boat_gone {
                    return GoalStatus::Done;
                }
            }
            MotionStatus::Failed(_) if self.ashore_tried => {
                return GoalStatus::Failed(FailReason::Stuck);
            }
            MotionStatus::Failed(_) => {
                self.ashore_tried = true;
                // another way: the nearest beach from where the bot is
                if let Some(spot) = io.ops.shore_near(xy(io.pos), 600.0) {
                    acts.push(Act::Travel {
                        x: spot.stand.0,
                        y: spot.stand.1,
                        radius: 1.0,
                    });
                }
            }
            _ => {
                acts.push(Act::Travel {
                    x: stand.0,
                    y: stand.1,
                    radius: 1.0,
                });
            }
        }
        GoalStatus::Running
    }

    // ---- abort -----------------------------------------------------------------------------

    /// Starts the way out of a failure (once) and takes its first step.
    fn abort(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, why: FailReason) -> Step {
        if self.phase != Phase::Abort {
            self.abort = Some(why);
            self.enter(Phase::Abort, io.now_ms);
            self.discard_ms = 0;
            self.despawn_ms = 0;
            self.homing_ms = 0;
            acts.push(Act::Say(format!("sail: giving up ({why:?})")));
            acts.push(Act::StopMotion);
        }
        self.abort_step(io, acts)
    }

    fn abort_step(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> Step {
        let now = io.now_ms;
        let why = self.abort.unwrap_or(FailReason::Other("aborted"));
        let s = io.sailor;
        // leave the seat
        if let Some(seat) = s.and_then(|s| s.seat) {
            if let Some(tr) = s.and_then(|s| s.track).filter(|t| t.cur.obj == seat.parent) {
                let p = tr.cur.pos;
                acts.push(Act::Sync([p.0, p.1, p.2]));
            }
            let tl = s
                .and_then(|s| s.boat.as_ref())
                .filter(|b| b.obj == seat.parent)
                .map(|b| b.tl)
                .or_else(|| {
                    s.and_then(|s| s.foreign)
                        .filter(|f| f.obj == seat.parent)
                        .map(|f| f.tl)
                });
            if let Some(tl) = tl {
                if self.discard_ms == 0 || now - self.discard_ms >= 3000 {
                    if let Some(b) = s.and_then(|s| s.boat.as_ref()) {
                        // (a stopped helm first, so the hull does not run away)
                        acts.push(Act::Helm {
                            ship: b.obj,
                            cmd: Cmd::default(),
                        });
                    }
                    acts.push(Act::Discard(tl));
                    self.discard_ms = now.max(1);
                }
            }
            if now.saturating_sub(self.since_ms) < ABORT_BOAT_MS {
                return GoalStatus::Running;
            }
        }
        // despawn the boat
        if let Some(b) = s.and_then(|s| s.boat.as_ref()) {
            if !b.despawning && (self.despawn_ms == 0 || now - self.despawn_ms >= 5000) {
                acts.push(Act::Despawn(b.obj));
                self.despawn_ms = now.max(1);
            }
            if now.saturating_sub(self.since_ms) < ABORT_BOAT_MS {
                return GoalStatus::Running;
            }
        }
        // go to the nearest beach
        let here = xy(io.pos);
        if self.homing_ms == 0 {
            let spot = io
                .ops
                .shore_near(here, 1500.0)
                .map_or(self.home, |s| s.stand);
            acts.push(Act::Travel {
                x: spot.0,
                y: spot.1,
                radius: 1.0,
            });
            self.homing_ms = now.max(1);
            return GoalStatus::Running;
        }
        if io.motion == MotionStatus::Moving && now - self.homing_ms < ABORT_HOME_MS {
            return GoalStatus::Running;
        }
        GoalStatus::Failed(why)
    }

    /// The goal ends (done, failed or interrupted): leave the seat and despawn the boat, whatever
    /// the phase. Idempotent; the packets are only sent while there is something to undo.
    pub fn stop(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) {
        let Some(s) = io.sailor else {
            return;
        };
        if let Some(seat) = s.seat {
            if let Some(b) = s.boat.as_ref().filter(|b| b.obj == seat.parent) {
                acts.push(Act::Helm {
                    ship: b.obj,
                    cmd: Cmd::default(),
                });
                acts.push(Act::Discard(b.tl));
            } else if let Some(f) = s.foreign.filter(|f| f.obj == seat.parent) {
                acts.push(Act::Discard(f.tl));
            }
        }
        if let Some(b) = s.boat.as_ref().filter(|b| !b.despawning) {
            acts.push(Act::Despawn(b.obj));
        }
        acts.push(Act::StopMotion);
    }

    /// Whether the goal drives the combat decisions itself (see
    /// [`Goal::owns_combat`](super::super::Goal::owns_combat)).
    pub fn owns_combat(&self) -> bool {
        self.phase.at_sea()
    }

    /// One line about what the trip is doing.
    pub fn describe(&self, out: &mut String) {
        use std::fmt::Write;
        let _ = match self.phase {
            Phase::Gear => write!(out, "getting a boat scroll"),
            Phase::Plan => write!(out, "planning a trip"),
            Phase::Shore => write!(out, "walking to the launch beach"),
            Phase::Summon => write!(out, "summoning a boat"),
            Phase::Board => write!(out, "boarding the boat"),
            Phase::Sail => write!(out, "sailing, {:.0} m to go", self.dist_end),
            Phase::Land => write!(out, "coming ashore"),
            Phase::Abort => write!(out, "leaving the boat"),
        };
    }
}

/// The boat's position for the approach: the unit in view, else the record's last position (`None`
/// while neither is known).
fn boat_xy(io: &Input<'_>, s: &Sailor) -> Option<(f32, f32)> {
    if let Some(p) = io.boat_unit {
        return Some(xy(p));
    }
    s.boat
        .as_ref()
        .filter(|b| b.pos != [0.0; 3])
        .map(|b| xy(b.pos))
}

/// The gap in metres between a point and the box of a hull (`class`) whose origin is at
/// `boat` and whose bow points along the heading `yaw + 90 degrees` of a caster facing `yaw`, and
/// the point to swim to (the box's nearest point, a little outside it).
pub(super) fn hull_gap(
    class: &ShipClass,
    boat: (f32, f32),
    face_yaw: f32,
    p: (f32, f32),
) -> (f32, (f32, f32)) {
    let yaw = face_yaw + std::f32::consts::FRAC_PI_2;
    let (fx, fy) = (-yaw.sin(), yaw.cos());
    let (rx, ry) = (yaw.cos(), yaw.sin());
    let centre = (boat.0 + fx * class.center_y, boat.1 + fy * class.center_y);
    let (ox, oy) = (p.0 - centre.0, p.1 - centre.1);
    let (lx, ly) = (ox * rx + oy * ry, ox * fx + oy * fy);
    let (hx, hy) = (class.beam / 2.0, class.length / 2.0);
    let (cx, cy) = (lx.clamp(-hx, hx), ly.clamp(-hy, hy));
    let gap = (lx - cx).hypot(ly - cy);
    let near = (centre.0 + rx * cx + fx * cy, centre.1 + ry * cx + fy * cy);
    if gap < 1e-3 {
        return (0.0, near);
    }
    // a step outside the box along the line from the box to the bot
    let k = 0.3 / gap;
    (
        gap,
        (near.0 + (p.0 - near.0) * k, near.1 + (p.1 - near.1) * k),
    )
}

fn route_len(r: &[(f32, f32)]) -> f32 {
    r.windows(2).map(|w| dist2(w[0], w[1])).sum()
}
