//! The navigation state of one bot: plans a trip on the mesh and walks the route.
//!
//! [`NavState`] lives in the motion controller (`Motion::attach_nav`) and works on one trip at a
//! time. A trip starts with [`NavState::begin`] (the controller's `go_to`) and is driven by
//! [`NavState::step`] once per tick:
//!
//! ```text
//!   Unplanned ──► short trip: search on the bot task ──────────────┐
//!       │         long trip: request to the planner pool           ▼
//!       └──────► Planning (the bot stands, faces the goal) ──► Following ──► arrived
//!                    │ answer after >1.5 s, queue full,              │ route ends short of the goal
//!                    ▼ unreachable, off the mesh                     ▼
//!                  Legacy (the controller's straight line)      continuation (see below)
//! ```
//!
//! * **Continuations.** A partial answer (the search hit its cap) or a goal beyond
//!   [`MAX_PLAN_M`] is walked in pieces: the moment a route is installed that does not lead to the
//!   goal, the search for the next piece starts from the end of that route, so its answer is
//!   usually there before the bot is. At most [`MAX_CHAIN`] pieces per trip.
//! * **Stuck.** The controller's stuck detector asks [`NavState::on_stuck`]: the first
//!   [`MAX_STUCK_REPLANS`] times the route is searched again from where the bot is with the nodes
//!   on and just ahead of that spot made expensive; after that the trip is the controller's
//!   straight line with its side steps.
//! * **Answers are matched by ticket.** An answer for anything but the request the trip waits for
//!   is dropped (counted as stale).
//!
//! Nothing here reads a clock or talks to a socket except through the planner pool and the bot's
//! command channel; the caller passes the bot clock.

use std::sync::Arc;
use std::time::Instant;

use aa_world::geom::Vec3;
use aa_world::nav::{NavMesh, Path, PathResult};

use super::follow::{self, Route};
use super::metrics::{Fallback, NavMetrics, TripEnd};
use super::planner::{self, Job, NavWorld, PlanSpec, Planner};
use crate::bot::motion::{surface_z, Ground, Pace};
use crate::hub::BotSlot;

/// Trips up to this straight distance (metres) are searched on the bot's own task.
pub const INLINE_MAX_M: f32 = 150.0;
/// How long a bot waits for the planner before it walks the straight line.
pub const PLAN_WAIT_MS: u64 = 1_500;
/// The farthest goal one search is asked for; beyond it the trip goes in pieces.
pub const MAX_PLAN_M: f32 = 2_000.0;
/// The distance of the intermediate goal of such a piece.
pub const LEG_TARGET_M: f32 = 1_500.0;
/// Legs of a trip when the mesh is not used (the goals' own leg length for a 90 s trip).
pub const LEGACY_LEG_M: f32 = 280.0;
/// Most continuations of one trip.
pub const MAX_CHAIN: u8 = 10;
/// Most times one trip is searched again because the bot did not get anywhere.
pub const MAX_STUCK_REPLANS: u8 = 2;
/// Trips whose remaining distance (to the stop radius) is shorter than this are the straight line.
pub const MIN_NAV_M: f32 = 4.0;
/// Trips of at least this length count in the trip statistics (the chase of a monster that moves
/// restarts its trip every tick and is not a trip).
pub const COUNTED_TRIP_M: f32 = 15.0;
/// A partial answer that gets the bot less far than this is no progress.
const MIN_PARTIAL_M: f32 = 2.0;
/// A run slows down to a walk over the last metres before the stop point.
const TAIL_RAMP_M: f32 = 5.0;
/// An indoor segment longer than this (a bridge, a long stair) is run, not walked.
const LONG_INDOOR_RUN_M: f32 = 25.0;
/// Cost (metres) added to the node a bot got stuck on, and to the nodes next to it.
const STUCK_PENALTY_M: f32 = 80.0;
const STUCK_PENALTY_NEAR_M: f32 = 40.0;
/// A trip a goal stops with the bot this close (metres) to the stop point counts as arrived.
const STOP_NEAR_M: f32 = 6.0;
/// A trip started again toward a goal this close (metres) to the goal it has goes on instead.
const SAME_GOAL_M: f32 = 1.0;
/// Extra time a trip may take beyond what its route needs at 3 m/s.
const SLACK_MS: u64 = 30_000;

/// Everything a bot needs to plan. One per bot, built when the bot task starts.
pub struct NavLink {
    /// Path finding is on. Off: trips are the straight line (and only counted).
    pub enabled: bool,
    /// The meshes and the terrain.
    pub world: Arc<dyn NavWorld>,
    /// The planner pool; `None` searches everything on the bot task (tests).
    pub planner: Option<Arc<Planner>>,
    /// The bot's slot: answers of the pool go to its command channel.
    pub reply: Arc<BotSlot>,
    /// The process-wide counters.
    pub metrics: Arc<NavMetrics>,
    /// Name of the main world (the mesh the bot walks on outside instances).
    pub main: Arc<str>,
    /// A number that is the same for the same bot every time (its lateral habit comes from it).
    pub seed: u32,
}

/// The world the bot is in, as far as meshes go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    Main,
    Instance(Box<str>),
    /// An instance without a mesh name: straight lines.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// `begin` was called; the search has not been started.
    Unplanned,
    /// The answer to request `ticket` is awaited; the bot stands.
    Planning {
        ticket: u32,
        since_ms: u64,
        final_leg: bool,
    },
    /// Walking `Trip::route`.
    Following,
    /// The controller's straight line.
    Legacy,
}

/// The search for the piece after the route being walked.
#[derive(Debug)]
enum Chain {
    None,
    Pending {
        ticket: u32,
        final_leg: bool,
    },
    Ready {
        result: Box<PathResult>,
        final_leg: bool,
    },
}

#[derive(Debug)]
struct Trip {
    goal: [f32; 2],
    /// Height of the goal floor when the caller named one (an NPC on a building or cave floor).
    goal_z: Option<f32>,
    radius: f32,
    /// Straight distance from the start to the stop point.
    straight_m: f32,
    walked_m: f32,
    counted: bool,
    started_ms: u64,
    /// How long the trip may take (ms since it started); 0 = the controller's default.
    limit_ms: u64,
    phase: Phase,
    route: Option<Box<Route>>,
    chain: Chain,
    chain_steps: u8,
    stuck_replans: u8,
    /// Velocity of the last step.
    vel: (f32, f32),
    /// Last unit direction of travel.
    heading: (f32, f32),
}

/// What one [`NavState::step`] works with.
pub struct StepIn<'a> {
    /// The bot clock.
    pub now_ms: u64,
    /// Seconds to walk (the time since the last tick, at most a second).
    pub dt_s: f32,
    /// Where the bot is.
    pub pos: [f32; 3],
    /// What the caller asked for.
    pub pace: Pace,
    /// Run and walk speed in m/s.
    pub run: f32,
    pub walk: f32,
    /// The terrain the bot stands on.
    pub ground: &'a dyn Ground,
}

/// What a step did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NavStep {
    /// Walk the straight line like the controller always did.
    Legacy,
    /// Waiting for a search: stand and face the goal.
    Wait,
    /// The bot walked to `pos`.
    Moved {
        pos: [f32; 3],
        /// Unit direction at the end of the step.
        heading: (f32, f32),
        /// Speed at the end of the step (m/s).
        speed: f32,
        /// The stop point is reached.
        arrived: bool,
    },
}

enum Req {
    Done(PathResult, bool),
    Queued(u32, bool),
    Refused(Fallback),
}

/// The navigation state of one bot. See the module docs.
pub struct NavState {
    link: NavLink,
    scope: Scope,
    ticket: u32,
    /// The side and distance the bot likes to walk beside the line (metres, signed).
    pref: f32,
    trip: Option<Trip>,
}

impl std::fmt::Debug for NavState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavState")
            .field("enabled", &self.link.enabled)
            .field("scope", &self.scope)
            .field("phase", &self.trip.as_ref().map(|t| t.phase))
            .finish()
    }
}

fn d2(a: [f32; 3], g: [f32; 2]) -> f32 {
    ((a[0] - g[0]).powi(2) + (a[1] - g[1]).powi(2)).sqrt()
}

/// The gap to the goal, in three dimensions when the caller named a floor: a bot under a house
/// storey or under a cave floor is not there yet.
fn gap3(a: [f32; 3], goal: [f32; 2], goal_z: Option<f32>) -> f32 {
    let dz = goal_z.map_or(0.0, |z| a[2] - z);
    (d2(a, goal).powi(2) + dz * dz).sqrt()
}

/// The stable lateral habit of bot `seed`: 0.35 to 1.1 m, either side.
fn lateral_of(seed: u32) -> f32 {
    let mut h = seed.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    let mag = 0.35 + (h & 0xFF) as f32 / 255.0 * 0.75;
    if h & 0x100 == 0 {
        mag
    } else {
        -mag
    }
}

/// The goal of one search toward `goal` from `from`: the goal itself, or a point 1500 m on the
/// way when it is too far for one search. `true` when it is the goal.
fn leg_target(from: [f32; 3], goal: [f32; 2]) -> ([f32; 2], bool) {
    let d = d2(from, goal);
    if d <= MAX_PLAN_M {
        return (goal, true);
    }
    let k = LEG_TARGET_M / d;
    (
        [
            from[0] + (goal[0] - from[0]) * k,
            from[1] + (goal[1] - from[1]) * k,
        ],
        false,
    )
}

impl NavState {
    /// A state with no trip, for a bot in the main world.
    pub fn new(link: NavLink) -> NavState {
        let pref = lateral_of(link.seed);
        NavState {
            link,
            scope: Scope::Main,
            ticket: 0,
            pref,
            trip: None,
        }
    }

    /// Tells which world the bot is in (`world_name` is the instance world's name once known).
    pub fn set_scope(&mut self, in_instance: bool, world_name: Option<&str>) {
        let same = match (&self.scope, in_instance, world_name) {
            (Scope::Main, false, _) => true,
            (Scope::Instance(a), true, Some(b)) => **a == *b,
            (Scope::Unknown, true, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        self.scope = match (in_instance, world_name) {
            (false, _) => Scope::Main,
            (true, Some(n)) => Scope::Instance(n.into()),
            (true, None) => Scope::Unknown,
        };
    }

    /// The process-wide counters.
    pub fn metrics(&self) -> &NavMetrics {
        &self.link.metrics
    }

    /// Whether path finding is on.
    pub fn enabled(&self) -> bool {
        self.link.enabled
    }

    fn mesh(&self) -> Option<Arc<NavMesh>> {
        match &self.scope {
            Scope::Main => self.link.world.mesh(&self.link.main),
            Scope::Instance(n) => self.link.world.mesh(n),
            Scope::Unknown => None,
        }
    }

    /// How long a walk the goals should cut a trip into: long when the mesh plans the way, the
    /// 90 s length of the straight line otherwise.
    pub fn leg_m(&self) -> f32 {
        if self.link.enabled && self.scope != Scope::Unknown {
            LEG_TARGET_M
        } else {
            LEGACY_LEG_M
        }
    }

    // ---- the trip -------------------------------------------------------------------------

    /// The floor height the running trip was asked to reach (see [`NavState::begin`]), if it named
    /// one: a trip that is planned again (after a glide) keeps it.
    pub fn goal_z(&self) -> Option<f32> {
        self.trip.as_ref().and_then(|t| t.goal_z)
    }

    /// Starts a trip from `pos` to the stop point `radius` metres short of `goal`. A trip that is
    /// not `count`ed (the combat controller's) stays out of the trip statistics.
    pub fn begin(
        &mut self,
        pos: [f32; 3],
        goal: [f32; 2],
        goal_z: Option<f32>,
        radius: f32,
        now_ms: u64,
        count: bool,
    ) {
        // The same goal again (a goal that asks every tick, the chase of a monster that stands
        // still): the trip goes on with its route and its search.
        if let Some(t) = self.trip.as_mut() {
            let moved = ((t.goal[0] - goal[0]).powi(2) + (t.goal[1] - goal[1]).powi(2)).sqrt();
            let same_z = match (t.goal_z, goal_z) {
                (None, None) => true,
                (Some(a), Some(b)) => (a - b).abs() < 1.0,
                _ => false,
            };
            if t.phase != Phase::Legacy
                && moved < SAME_GOAL_M
                && same_z
                && (t.radius - radius).abs() < 0.01
            {
                t.started_ms = now_ms;
                t.limit_ms = t
                    .route
                    .as_ref()
                    .map_or(0, |r| (r.left_len() / 3.0 * 1000.0) as u64 + SLACK_MS);
                return;
            }
        }
        self.end(TripEnd::Cancelled);
        let straight = (d2(pos, goal) - radius).max(0.0);
        let counted = count && straight >= COUNTED_TRIP_M;
        if counted {
            self.link
                .metrics
                .trips
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.trip = Some(Trip {
            goal,
            goal_z,
            radius,
            straight_m: straight,
            walked_m: 0.0,
            counted,
            started_ms: now_ms,
            limit_ms: 0,
            phase: if self.link.enabled {
                Phase::Unplanned
            } else {
                Phase::Legacy
            },
            route: None,
            chain: Chain::None,
            chain_steps: 0,
            stuck_replans: 0,
            vel: (0.0, 0.0),
            heading: (0.0, 0.0),
        });
    }

    /// The trip is over (or abandoned); counted in the statistics when it was long enough.
    pub fn end(&mut self, how: TripEnd) {
        if let Some(t) = self.trip.take() {
            if t.counted {
                self.link.metrics.trip_ended(how, t.walked_m, t.straight_m);
            }
        }
    }

    /// The goal stopped the trip with the bot at `pos` (`Motion::stop`): when that is at the
    /// destination, the trip counts as arrived (goals stop a walk themselves as soon as they are
    /// in range), otherwise as cancelled.
    pub fn stopped_at(&mut self, pos: [f32; 3]) {
        let near = self
            .trip
            .as_ref()
            .is_some_and(|t| d2(pos, t.goal) - t.radius <= STOP_NEAR_M);
        self.end(if near {
            TripEnd::Arrived
        } else {
            TripEnd::Cancelled
        });
    }

    /// Whether a trip is under way.
    pub fn active(&self) -> bool {
        self.trip.is_some()
    }

    /// How long (ms since it started) the trip may take, 0 for the controller's default.
    pub fn limit_ms(&self) -> u64 {
        self.trip.as_ref().map_or(0, |t| t.limit_ms)
    }

    /// Adds metres the controller walked itself (the straight line).
    pub fn walked(&mut self, metres: f32) {
        if let Some(t) = self.trip.as_mut() {
            t.walked_m += metres;
        }
    }

    /// The stuck detector fired (counted whatever the trip does about it).
    pub fn note_stuck(&self) {
        self.link
            .metrics
            .stuck_events
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Velocity (x, y in m/s) of the bot while it follows a route, for the exact update
    /// `Motion::pin` sends.
    pub fn velocity(&self) -> Option<(f32, f32)> {
        let t = self.trip.as_ref()?;
        (t.phase == Phase::Following && t.route.is_some()).then_some(t.vel)
    }

    /// Metres of straight route ahead of the bot before the next corner, while it follows a
    /// route; `None` on the controller's straight line.
    pub fn straight_left(&self) -> Option<f32> {
        let t = self.trip.as_ref()?;
        if t.phase != Phase::Following {
            return None;
        }
        t.route.as_ref().map(|r| r.straight_left())
    }

    /// Whether the bot stands and waits for a search to finish.
    pub fn waiting(&self) -> bool {
        self.trip
            .as_ref()
            .is_some_and(|t| matches!(t.phase, Phase::Planning { .. } | Phase::Unplanned))
    }

    /// Whether an answer of the planner pool is awaited: the first route of the trip, or the
    /// piece after the route being walked.
    pub fn awaits_answer(&self) -> bool {
        self.trip.as_ref().is_some_and(|t| {
            matches!(t.phase, Phase::Planning { .. }) || matches!(t.chain, Chain::Pending { .. })
        })
    }

    /// Whether the trip walks the controller's straight line.
    pub fn is_legacy(&self) -> bool {
        self.trip.as_ref().is_none_or(|t| t.phase == Phase::Legacy)
    }

    /// Appends the rest of the route as map points (at most `max`), starting at the bot.
    /// Returns whether there is a route; the straight line has none.
    pub fn route_points(&self, out: &mut Vec<[f32; 2]>, max: usize) -> bool {
        let Some(route) = self.trip.as_ref().and_then(|t| t.route.as_ref()) else {
            return false;
        };
        if self.trip.as_ref().map(|t| t.phase) != Some(Phase::Following) {
            return false;
        }
        route.remaining_points(out);
        follow::decimate(out, max);
        true
    }

    // ---- planning -------------------------------------------------------------------------

    /// Asks for a route from `from` toward `goal`: answered at once for a short trip, queued
    /// for a long one.
    #[allow(clippy::too_many_arguments)]
    fn request(
        &mut self,
        mesh: Arc<NavMesh>,
        from: [f32; 3],
        goal: [f32; 2],
        goal_z: Option<f32>,
        penalties: Vec<(aa_world::nav::NodeRef, f32)>,
        boost: u8,
        prefer_pool: bool,
    ) -> Req {
        let (target, final_leg) = leg_target(from, goal);
        let terrain = self.scope == Scope::Main;
        let z = if terrain {
            // The last leg goes to the floor the caller named (a house storey, a cave), so the
            // search does not end on the terrain above or below the NPC.
            if final_leg { goal_z } else { None }
                .or_else(|| {
                    self.link
                        .world
                        .ground(target[0], target[1])
                        .filter(|z| *z > 0.0)
                })
                .unwrap_or(from[2])
        } else {
            from[2]
        };
        let mut spec = PlanSpec {
            from,
            to: [target[0], target[1], z],
            max_expansions: planner::POOL_MAX_EXPANSIONS,
            penalties,
            terrain,
        };
        let pooled =
            self.link.planner.is_some() && (prefer_pool || d2(from, target) > INLINE_MAX_M);
        if !pooled {
            if self.link.planner.is_some() {
                spec.max_expansions = planner::INLINE_MAX_EXPANSIONS;
            }
            let started = Instant::now();
            let world = &self.link.world;
            let r = planner::plan(&mesh, &spec, &|x, y| world.ground(x, y));
            self.link
                .metrics
                .record_query(&r, false, started.elapsed().as_micros() as u64);
            return Req::Done(r, final_leg);
        }
        spec.max_expansions = planner::POOL_MAX_EXPANSIONS << boost.min(3);
        self.ticket = self.ticket.wrapping_add(1).max(1);
        let ticket = self.ticket;
        let job = Job {
            ticket,
            mesh,
            world: self.link.world.clone(),
            spec,
            reply: self.link.reply.clone(),
            submitted: Instant::now(),
        };
        match self.link.planner.as_ref().map(|p| p.submit(job)) {
            Some(Ok(())) => Req::Queued(ticket, final_leg),
            _ => Req::Refused(Fallback::QueueFull),
        }
    }

    fn fall_back(&self, t: &mut Trip, why: Fallback) {
        t.phase = Phase::Legacy;
        t.route = None;
        t.chain = Chain::None;
        t.vel = (0.0, 0.0);
        self.link.metrics.fell_back(why);
    }

    /// Starts the search for the first route of the trip.
    fn start(&mut self, t: &mut Trip, pos: [f32; 3], now_ms: u64) {
        let Some(mesh) = self.mesh() else {
            self.fall_back(t, Fallback::NoMeshForWorld);
            return;
        };
        match self.request(mesh, pos, t.goal, t.goal_z, Vec::new(), 0, false) {
            Req::Done(r, final_leg) => self.apply(t, r, final_leg, now_ms),
            Req::Queued(ticket, final_leg) => {
                t.phase = Phase::Planning {
                    ticket,
                    since_ms: now_ms,
                    final_leg,
                }
            }
            Req::Refused(why) => self.fall_back(t, why),
        }
    }

    /// An answer for the route the trip needs now.
    fn apply(&mut self, t: &mut Trip, result: PathResult, final_leg: bool, now_ms: u64) {
        match result {
            PathResult::Found(p) => self.install(t, &p, final_leg, now_ms),
            PathResult::Partial(p) => {
                if p.length < MIN_PARTIAL_M || t.chain_steps >= MAX_CHAIN {
                    self.fall_back(t, Fallback::NoProgress);
                } else {
                    self.install(t, &p, false, now_ms);
                }
            }
            PathResult::Unreachable => self.fall_back(t, Fallback::Unreachable),
            PathResult::NoMesh => self.fall_back(t, Fallback::OffMesh),
        }
    }

    /// Makes `p` the route the bot walks. `complete`: it leads to the goal.
    fn install(&mut self, t: &mut Trip, p: &Path, complete: bool, now_ms: u64) {
        let pts: Vec<[f32; 3]> = p.points.iter().map(|v| [v.x, v.y, v.z]).collect();
        let mesh = self.mesh();
        let free = |x: f32, y: f32, z: f32| {
            mesh.as_ref().is_some_and(|m| {
                m.locate_detailed(Vec3::new(x, y, z), 0.0)
                    .is_some_and(|l| l.inside && l.dist < 2.0)
            })
        };
        let Some(route) = Route::new(&pts, &p.indoor, self.pref, &free) else {
            self.fall_back(t, Fallback::NoProgress);
            return;
        };
        let elapsed = now_ms.saturating_sub(t.started_ms);
        let needs = elapsed + (route.total_len() / 3.0 * 1000.0) as u64 + SLACK_MS;
        t.limit_ms = t.limit_ms.max(needs);
        let end = route.end_point();
        t.route = Some(Box::new(route));
        t.phase = Phase::Following;
        t.chain = Chain::None;
        if !complete {
            self.start_chain(t, end, now_ms);
        }
    }

    /// Starts the search for the piece after the route that ends at `from`.
    fn start_chain(&mut self, t: &mut Trip, from: [f32; 3], _now_ms: u64) {
        if t.chain_steps >= MAX_CHAIN {
            return;
        }
        t.chain_steps += 1;
        self.link
            .metrics
            .replans
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(mesh) = self.mesh() else {
            return;
        };
        t.chain = match self.request(
            mesh,
            from,
            t.goal,
            t.goal_z,
            Vec::new(),
            t.chain_steps,
            true,
        ) {
            Req::Done(r, final_leg) => Chain::Ready {
                result: Box::new(r),
                final_leg,
            },
            Req::Queued(ticket, final_leg) => Chain::Pending { ticket, final_leg },
            Req::Refused(_) => {
                self.link
                    .metrics
                    .queue_full
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Chain::None
            }
        };
    }

    /// The planner answered request `ticket` (`BotCmd::PathReady`).
    pub fn on_result(&mut self, ticket: u32, result: PathResult, now_ms: u64) {
        let Some(mut t) = self.trip.take() else {
            self.link
                .metrics
                .stale_results
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return;
        };
        match t.phase {
            Phase::Planning {
                ticket: waiting,
                final_leg,
                ..
            } if waiting == ticket => self.apply(&mut t, result, final_leg, now_ms),
            _ => match t.chain {
                Chain::Pending {
                    ticket: waiting,
                    final_leg,
                } if waiting == ticket => {
                    t.chain = Chain::Ready {
                        result: Box::new(result),
                        final_leg,
                    }
                }
                _ => {
                    self.link
                        .metrics
                        .stale_results
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            },
        }
        self.trip = Some(t);
    }

    /// The stuck detector fired at `pos`. Returns whether the trip took care of it (a new route
    /// was searched); `false` means the controller does its side step on the straight line.
    pub fn on_stuck(&mut self, pos: [f32; 3], now_ms: u64) -> bool {
        self.note_stuck();
        let Some(mut t) = self.trip.take() else {
            return false;
        };
        let handled = match t.phase {
            Phase::Following if t.stuck_replans < MAX_STUCK_REPLANS && self.mesh().is_some() => {
                t.stuck_replans += 1;
                self.link
                    .metrics
                    .stuck_replans
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let mesh = self.mesh().expect("checked above");
                // the way ahead is what blocked the bot: make the nodes there expensive
                let (hx, hy) = t.heading;
                let mut penalties = Vec::new();
                for ahead in [0.0f32, 2.0, 4.0, 6.0] {
                    let spot = Vec3::new(pos[0] + hx * ahead, pos[1] + hy * ahead, pos[2]);
                    if let Some(node) = mesh.locate(spot, 3.0) {
                        penalties.push((node, STUCK_PENALTY_M));
                        penalties.extend(
                            mesh.neighbours(node, 0.5)
                                .into_iter()
                                .map(|n| (n, STUCK_PENALTY_NEAR_M)),
                        );
                    }
                }
                t.route = None;
                t.chain = Chain::None;
                match self.request(
                    mesh,
                    pos,
                    t.goal,
                    t.goal_z,
                    penalties,
                    t.stuck_replans,
                    false,
                ) {
                    Req::Done(r, final_leg) => {
                        self.apply(&mut t, r, final_leg, now_ms);
                        t.phase != Phase::Legacy
                    }
                    Req::Queued(ticket, final_leg) => {
                        t.phase = Phase::Planning {
                            ticket,
                            since_ms: now_ms,
                            final_leg,
                        };
                        true
                    }
                    Req::Refused(why) => {
                        self.fall_back(&mut t, why);
                        false
                    }
                }
            }
            Phase::Legacy => false,
            _ => {
                self.fall_back(&mut t, Fallback::Stuck);
                false
            }
        };
        self.trip = Some(t);
        handled
    }

    // ---- walking --------------------------------------------------------------------------

    /// Advances the trip by one tick.
    pub fn step(&mut self, i: &StepIn<'_>) -> NavStep {
        let Some(mut t) = self.trip.take() else {
            return NavStep::Legacy;
        };
        let out = self.step_trip(&mut t, i);
        self.trip = Some(t);
        out
    }

    fn step_trip(&mut self, t: &mut Trip, i: &StepIn<'_>) -> NavStep {
        // (a phase change moves on to the next phase within the same tick, a few times at most)
        for _ in 0..4 {
            match t.phase {
                Phase::Legacy => return NavStep::Legacy,
                Phase::Unplanned => {
                    if gap3(i.pos, t.goal, t.goal_z) - t.radius <= MIN_NAV_M {
                        // (a hop the straight line does as well)
                        t.phase = Phase::Legacy;
                    } else {
                        self.start(t, i.pos, i.now_ms);
                    }
                }
                Phase::Planning { since_ms, .. } => {
                    if i.now_ms.saturating_sub(since_ms) > PLAN_WAIT_MS {
                        self.fall_back(t, Fallback::Timeout);
                    } else {
                        return NavStep::Wait;
                    }
                }
                Phase::Following => return self.follow(t, i),
            }
        }
        NavStep::Legacy
    }

    /// The speed on the current piece of the route.
    fn speed_on(route: &Route, straight_m: f32, gap: f32, i: &StepIn<'_>) -> f32 {
        let (run, walk) = (i.run, i.walk.min(i.run));
        if i.pace == Pace::Walk {
            return walk;
        }
        let mut v = run;
        if route.indoor_here() && route.segment_len_here() < LONG_INDOOR_RUN_M {
            v = walk;
        }
        if straight_m >= COUNTED_TRIP_M && gap < TAIL_RAMP_M {
            v = v.min(walk + (run - walk) * (gap / TAIL_RAMP_M).clamp(0.0, 1.0));
        }
        v.max(1e-3)
    }

    fn follow(&mut self, t: &mut Trip, i: &StepIn<'_>) -> NavStep {
        let mut pos = i.pos;
        let mut left_s = i.dt_s.max(0.0);
        let mut speed = i.run;
        let mut arrived = false;
        for _ in 0..64 {
            let (goal, goal_z, radius, straight_m) = (t.goal, t.goal_z, t.radius, t.straight_m);
            let Some(route) = t.route.as_mut() else {
                self.fall_back(t, Fallback::RouteEnded);
                break;
            };
            let gap = d2(pos, goal) - radius;
            if gap3(pos, goal, goal_z) - radius <= 0.02 {
                arrived = true;
                break;
            }
            speed = Self::speed_on(route, straight_m, gap, i);
            if left_s <= 1e-4 {
                break;
            }
            // (never past the stop point: near the goal the steps shrink to the gap)
            let ds = (speed * left_s).min(gap.max(0.05));
            let unused = route.advance(ds);
            let moved = ds - unused;
            t.walked_m += moved;
            left_s -= moved / speed;
            let p = route.position();
            pos = [p[0], p[1], p[2]];
            if !route.is_done() {
                continue;
            }
            // the end of the route
            if gap3(pos, goal, goal_z) - radius <= 0.02 {
                arrived = true;
                break;
            }
            match std::mem::replace(&mut t.chain, Chain::None) {
                Chain::Ready { result, final_leg } => {
                    t.route = None;
                    self.apply(t, *result, final_leg, i.now_ms);
                    if t.phase != Phase::Following {
                        break;
                    }
                }
                Chain::Pending { ticket, final_leg } => {
                    t.phase = Phase::Planning {
                        ticket,
                        since_ms: i.now_ms,
                        final_leg,
                    };
                    break;
                }
                Chain::None => {
                    // A route that ends at the spot above/below a named floor means the mesh
                    // has no way up/down: stop there and let the goal judge the 3D distance
                    // (it reports the target as unreachable) instead of walking the straight
                    // line for another 90 s.
                    if t.goal_z.is_some() && d2(pos, t.goal) - t.radius <= 0.02 {
                        arrived = true;
                        break;
                    }
                    self.fall_back(t, Fallback::RouteEnded);
                    break;
                }
            }
        }
        let z = match t.route.as_ref() {
            Some(r) => {
                let rz = r.position()[2];
                if r.indoor_here() {
                    rz
                } else {
                    i.ground
                        .ground_z(pos[0], pos[1])
                        .map_or(rz, |g| surface_z(Some(g), rz))
                }
            }
            None => surface_z(i.ground.ground_z(pos[0], pos[1]), pos[2]),
        };
        if let Some(r) = t.route.as_ref() {
            let h = r.heading();
            if h != (0.0, 0.0) {
                t.heading = h;
            }
        }
        let heading = t.heading;
        pos[2] = z;
        t.vel = (heading.0 * speed, heading.1 * speed);
        NavStep::Moved {
            pos,
            heading,
            speed,
            arrived,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_floor_goal_counts_the_height() {
        let goal = [10.0, 20.0];
        let under = [10.0, 20.0, 105.0];
        assert_eq!(gap3(under, goal, None), 0.0);
        let gap = gap3(under, goal, Some(129.63));
        assert!((gap - 24.63).abs() < 0.01, "{gap}");
        assert!(gap3([10.0, 20.0, 129.63], goal, Some(129.63)) < 0.01);
    }
}
