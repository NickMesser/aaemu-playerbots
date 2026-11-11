//! The movement controller: walks a bot to a destination and tells the server about it.
//!
//! The server does not validate movement, so a bot is its own physics: [`Motion::tick`]
//! integrates the position along straight lines through the waypoints at run or walk speed, puts
//! the bot on the terrain ([`surface_z`]) and sends `CSMoveUnit` while moving plus one stopped
//! update on arrival. A stuck detector (less than [`STUCK_MIN_PROGRESS_M`] of progress in
//! [`STUCK_WINDOW_MS`]) inserts side steps and finally reports [`MoveFailure::Stuck`]; every trip
//! also has a [`TRIP_TIMEOUT_MS`] limit.
//!
//! While a real player can see the bot, the bot loop ticks the controller every
//! `bots.move_update_ms` (125 ms) and every tick of a walk sends an update, at an even pace like
//! the client's own; a bot nobody can see sends one a second. The server only knows the position
//! of the last `CSMoveUnit`, so it can see such a bot a run-second (5 m) behind. Before a packet
//! whose range the server checks (loot, talk, sell, craft, cast) the caller asks [`Motion::pin`]
//! for an exact update.
//!
//! A bot that has a [`NavLink`] ([`Motion::attach_nav`]) does not walk the straight line when the
//! navigation mesh knows the way: its [`NavState`] plans the trip and the controller integrates
//! the position along the route instead (see [`super::nav`]). The packets, the stopped update on
//! arrival, the stuck detector and [`Motion::pin`] work the same in both cases, and every
//! situation the mesh cannot help with (unreachable goal, planner late) is the straight line.
//!
//! Nothing here reads a clock or talks to a socket: the caller passes the bot clock and an
//! outbox ([`MotionEnv`]), which keeps the controller testable.

use aa_proto::cs::{self, MoveUnit, Out};
use aa_proto::game::LEVEL_GAME;
use aa_proto::opcodes::cs as op;
use smallvec::SmallVec;

use super::manner::{self, AirStep, Life, Look};
use super::nav::{NavLink, NavState, NavStep, StepIn, TripEnd};
use super::place::Place;
use super::state::{dist2d, dist3d};

/// Sea level of the 1.2 world in metres.
pub const SEA_LEVEL: f32 = 100.0;
/// Terrain below this height is sea floor: the bot swims instead of walking on it.
pub const SWIM_THRESHOLD: f32 = 99.0;
/// Height of a swimming bot (just under the surface).
pub const SWIM_Z: f32 = 98.5;
/// Minimum progress inside [`STUCK_WINDOW_MS`] for the bot not to count as stuck.
pub const STUCK_MIN_PROGRESS_M: f32 = 0.3;
/// Length of the stuck observation window.
pub const STUCK_WINDOW_MS: u64 = 3000;
/// Failed windows after which a trip is abandoned.
pub const STUCK_MAX_TRIES: u8 = 5;
/// Longest a single trip may take.
pub const TRIP_TIMEOUT_MS: u64 = 90_000;
/// How long a `/move` teleport may take before the bot walks instead.
pub const TELEPORT_WAIT_MS: u64 = 10_000;
/// Longest time step one tick integrates (a stalled task must not teleport the bot).
const MAX_STEP_MS: u64 = 1000;
/// A facing change smaller than this (radians) is not worth a packet.
const FACE_EPSILON: f32 = 0.09;
/// The server's idea of the position may be this far off before [`Motion::pin`] corrects it.
const PIN_EPSILON_M: f32 = 0.15;
/// A moving update is due this many ms before its gap is over: the timer of the bot loop wakes a
/// little late or early, and a tick that just misses the gap must not skip an update (which
/// would double the next gap).
const SEND_SLACK_MS: u64 = 20;

/// Where the ground is. [`aa_world::World`] answers from the baked height grid.
pub trait Ground {
    /// Terrain height at (x, y), `None` where unknown.
    fn ground_z(&self, x: f32, y: f32) -> Option<f32>;
}

impl Ground for aa_world::World {
    fn ground_z(&self, x: f32, y: f32) -> Option<f32> {
        aa_world::World::ground_z(self, x, y)
    }
}

impl Ground for aa_world::terrain::Terrain {
    fn ground_z(&self, x: f32, y: f32) -> Option<f32> {
        self.height(x, y)
    }
}

impl<F: Fn(f32, f32) -> Option<f32>> Ground for F {
    fn ground_z(&self, x: f32, y: f32) -> Option<f32> {
        self(x, y)
    }
}

/// The height a bot stands at: the terrain, the water surface over sea floor, or `fallback`
/// where the terrain is unknown. A bot is never put under the sea surface.
pub fn surface_z(ground: Option<f32>, fallback: f32) -> f32 {
    match ground {
        Some(g) if g < SWIM_THRESHOLD => SWIM_Z,
        Some(g) => g,
        None => fallback,
    }
}

/// `a` folded into `(-π, π]`.
fn wrap_pi(a: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    let r = (a + std::f32::consts::PI).rem_euclid(tau) - std::f32::consts::PI;
    if r <= -std::f32::consts::PI {
        r + tau
    } else {
        r
    }
}

/// How fast the bot goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// Run speed.
    Run,
    /// Walk speed.
    Walk,
}

/// Why a trip failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveFailure {
    /// No progress after several side steps.
    Stuck,
    /// The trip took longer than [`TRIP_TIMEOUT_MS`].
    Timeout,
}

/// What the controller is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionStatus {
    /// Standing, nothing asked.
    Idle,
    /// On the way.
    Moving,
    /// The last destination was reached.
    Arrived,
    /// The last trip was abandoned.
    Failed(MoveFailure),
}

/// Speeds of the controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionCfg {
    /// Run speed in m/s.
    pub run_speed: f32,
    /// Walk speed in m/s.
    pub walk_speed: f32,
}

impl Default for MotionCfg {
    fn default() -> Self {
        Self {
            run_speed: 5.4,
            walk_speed: 1.8,
        }
    }
}

/// What one [`Motion::tick`] works with.
pub struct MotionEnv<'a> {
    /// The terrain.
    pub ground: &'a dyn Ground,
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// Packets to send.
    pub out: &'a mut Vec<Out>,
    /// Randomness for side steps.
    pub rng: &'a mut fastrand::Rng,
    /// Minimum time between two "moving" updates (`bots.move_update_ms` while a real player can
    /// see the bot, a second otherwise).
    pub send_gap_ms: u64,
    /// The bot is fighting: weapons drawn.
    pub combat: bool,
}

/// What the navigation state did with a tick.
enum NavOutcome {
    /// A search runs: the bot stands.
    Wait,
    /// The bot walked; the direction and speed to announce.
    Moved { heading: (f32, f32), speed: f32 },
}

/// Verdict of the stuck detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckVerdict {
    /// Making progress, or the window is not over.
    Fine,
    /// No progress: try a side step.
    Sidestep,
    /// Too many failed windows.
    GiveUp,
}

/// Detects a bot that does not get anywhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stuck {
    anchor: [f32; 2],
    since_ms: u64,
    tries: u8,
    flip: bool,
}

impl Stuck {
    /// Starts watching from `pos` at `now_ms`.
    pub fn reset(&mut self, now_ms: u64, pos: [f32; 3]) {
        *self = Stuck {
            anchor: [pos[0], pos[1]],
            since_ms: now_ms,
            tries: 0,
            flip: false,
        };
    }

    /// Looks at the bot's position; call every tick while it should be moving.
    pub fn observe(&mut self, now_ms: u64, pos: [f32; 3]) -> StuckVerdict {
        if now_ms.saturating_sub(self.since_ms) < STUCK_WINDOW_MS {
            return StuckVerdict::Fine;
        }
        let moved = ((self.anchor[0] - pos[0]).powi(2) + (self.anchor[1] - pos[1]).powi(2)).sqrt();
        self.anchor = [pos[0], pos[1]];
        self.since_ms = now_ms;
        if moved >= STUCK_MIN_PROGRESS_M {
            self.tries = 0;
            return StuckVerdict::Fine;
        }
        self.tries += 1;
        if self.tries >= STUCK_MAX_TRIES {
            StuckVerdict::GiveUp
        } else {
            StuckVerdict::Sidestep
        }
    }

    /// The side the next side step goes to (alternates).
    fn next_side(&mut self) -> f32 {
        self.flip = !self.flip;
        if self.flip {
            1.0
        } else {
            -1.0
        }
    }
}

/// A glide in progress: a fixed heading, a forward speed and a descent. The 1.2 client's glider
/// physics is not on the wire, so the bot moves itself; see [`Motion::begin_glide`].
#[derive(Debug, Clone, Copy)]
struct Glide {
    /// Unit heading (x, y).
    dir: (f32, f32),
    /// Horizontal speed, m/s.
    speed: f32,
    /// Descent, m/s (positive).
    descent: f32,
    /// Bot clock the glide started.
    started_ms: u64,
    /// The ground has fallen away from under the bot (it is in the air). A glide starts a step
    /// or two before the edge: until then the bot runs along the heading on the ground.
    airborne: bool,
}

/// What the controller does besides walking its path: a GM teleport in flight, or a glide.
#[derive(Debug, Default)]
enum TripMode {
    /// Nothing special.
    #[default]
    None,
    /// Waiting for the server to move the bot after `/move`.
    Teleport {
        /// Bot clock after which the bot walks instead.
        deadline_ms: u64,
    },
    /// Flying down a ledge (boxed: a `Motion` is part of every `Bot` and must stay small).
    Glide(Box<Glide>),
}

/// Longest a glide may last, ms (the glide buff in the 1.2 data is 180 s; the bot is
/// conservative).
const MAX_GLIDE_MS: u64 = 30_000;
/// How long a glide may run along the ground before the ground falls away; past it the glide
/// ends as a landing (the edge was not there), ms.
const GLIDE_RUN_UP_MS: u64 = 2_000;
/// Height above the ground from which the bot counts as airborne (metres).
const GLIDE_AIRBORNE_M: f32 = 0.5;

/// The movement controller of one bot. See the module docs.
#[derive(Debug)]
pub struct Motion {
    cfg: MotionCfg,
    obj: u32,
    pos: [f32; 3],
    yaw: f32,
    path: SmallVec<[(f32, f32); 3]>,
    radius: f32,
    pace: Pace,
    status: MotionStatus,
    /// The last packet announced motion.
    moving_sent: bool,
    /// A stopped update is owed (after `stop`, a face change or an arrival).
    stop_due: bool,
    force_send: bool,
    last_tick_ms: u64,
    last_send_ms: u64,
    started_ms: u64,
    stuck: Stuck,
    mode: TripMode,
    /// Where the last `CSMoveUnit` (or the server's own placement) put the bot, as the server
    /// knows it.
    sent_pos: [f32; 3],
    /// The object the movement packets name: the bot itself, or the mate it rides (a riding
    /// player's own moves are ignored by the server, `world/session.rs:1191`; the client sends
    /// the mount's object id instead).
    move_obj: u32,
    /// Navigation mesh routes; `None`: every trip is the straight line.
    nav: Option<Box<NavState>>,
    /// Life-like movement (jumps, mannerisms, pauses; see [`super::manner`]); `None`: off.
    life: Option<Box<Life>>,
}

impl Motion {
    /// A controller that is not in the world yet; [`Motion::enter_world`] places it.
    pub fn new(cfg: MotionCfg) -> Motion {
        Motion {
            cfg,
            obj: 0,
            pos: [0.0; 3],
            yaw: 0.0,
            path: SmallVec::new(),
            radius: 0.0,
            pace: Pace::Run,
            status: MotionStatus::Idle,
            moving_sent: false,
            stop_due: false,
            force_send: false,
            last_tick_ms: 0,
            last_send_ms: 0,
            started_ms: 0,
            stuck: Stuck::default(),
            mode: TripMode::None,
            sent_pos: [0.0; 3],
            move_obj: 0,
            nav: None,
            life: None,
        }
    }

    /// Makes the bot move like a person (see [`super::manner`]). Without it the controller walks
    /// and stands exactly as it always did.
    pub fn attach_life(&mut self, life: Life) {
        self.life = Some(Box::new(life));
    }

    /// The life-like state, when the bot has one.
    pub fn life(&self) -> Option<&Life> {
        self.life.as_deref()
    }

    /// The life-like state, when the bot has one.
    pub fn life_mut(&mut self) -> Option<&mut Life> {
        self.life.as_deref_mut()
    }

    /// The object id the controller moves (0 before the bot is in the world).
    pub fn obj(&self) -> u32 {
        self.obj
    }

    /// The object the movement packets name: the bot's own object id, or the mate it rides
    /// (0 = the bot's own).
    pub fn move_obj(&self) -> u32 {
        if self.move_obj == 0 {
            self.obj
        } else {
            self.move_obj
        }
    }

    /// Moves the packets to the mate `obj` while the bot rides it, and back to the bot with 0.
    pub fn set_move_obj(&mut self, obj: u32) {
        let obj = if obj == self.obj { 0 } else { obj };
        if self.move_obj != obj {
            self.move_obj = obj;
            self.force_send = true;
        }
    }

    /// Puts the bot at `pos` without ending the trip (riding a mate shifts the rider to where the
    /// mate stood).
    pub fn set_position(&mut self, pos: [f32; 3]) {
        if pos.iter().all(|v| v.is_finite()) {
            self.pos = pos;
            self.stuck.reset(self.last_tick_ms, pos);
        }
    }

    /// Asks for an update that says where and how the bot stands (the stopped update that
    /// shows a bot getting up from a sit), unless it walks.
    pub fn announce(&mut self) {
        if self.obj != 0 && !self.is_moving() {
            self.stop_due = true;
        }
    }

    /// How long (ms) the bot loop may sleep at most before the controller needs it again: the
    /// next packet of a jump in progress.
    pub fn wake_in(&self, now_ms: u64) -> Option<u64> {
        self.life
            .as_ref()?
            .wake_at(now_ms)
            .map(|t| t.saturating_sub(now_ms))
    }

    /// The idle mannerisms of a bot that stands and may show off: a turn on the spot, now and
    /// then a small shuffle at walk pace. Called every tick by the goals that stand (rest,
    /// wander pauses); it does nothing while the bot walks, has no audience or is busy.
    pub fn fidget_tick(&mut self, now_ms: u64, rng: &mut fastrand::Rng, ground: &dyn Ground) {
        let Some(life) = self.life.as_deref_mut() else {
            return;
        };
        if self.obj == 0
            || self.status == MotionStatus::Moving
            || self.stop_due
            || !matches!(self.mode, TripMode::None)
            || life.jump.is_some()
            || !life.gate.show()
        {
            return;
        }
        let step = life.fidget.step(now_ms, life.traits.fidget, rng);
        match step {
            manner::Fid::None => {}
            manner::Fid::Turn(by) => {
                self.yaw = wrap_pi(self.yaw + by);
                self.stop_due = true;
            }
            manner::Fid::Shuffle { len, rel } => {
                // (counter-clockwise from +Y: the heading is (-sin, cos))
                let a = self.yaw + rel;
                let (x, y) = (self.pos[0] - a.sin() * len, self.pos[1] + a.cos() * len);
                let (here, there) = (
                    ground.ground_z(self.pos[0], self.pos[1]),
                    ground.ground_z(x, y),
                );
                // only over level, dry ground
                let flat = matches!((here, there), (Some(h), Some(t))
                    if h >= SWIM_THRESHOLD && t >= SWIM_THRESHOLD && (t - h).abs() <= 0.25);
                if flat {
                    self.go_via_counted(&[(x, y)], None, 0.0, Pace::Walk, now_ms, false);
                    // (a shuffle ends the mannerisms until the next command)
                    if let Some(l) = self.life.as_deref_mut() {
                        l.fidget = manner::Fidget::finished();
                    }
                }
            }
        }
    }

    /// Whether the trip waits for an answer of the planner pool (`BotCmd::PathReady`).
    pub fn awaits_path(&self) -> bool {
        self.nav.as_ref().is_some_and(|n| n.awaits_answer())
    }

    /// Lets this controller walk navmesh routes (see [`super::nav`]). Without it every trip is
    /// the straight line.
    pub fn attach_nav(&mut self, link: NavLink) {
        self.nav = Some(Box::new(NavState::new(link)));
    }

    /// Whether this bot walks on the navigation mesh (so a quest target off the mesh cannot be
    /// reached; without it the bot walks the straight line and tries).
    pub fn uses_navmesh(&self) -> bool {
        self.nav.is_some()
    }

    /// Tells the controller which world the bot is in (the main world's mesh, an instance
    /// world's, or none). Call once per tick before the goals run; cheap.
    pub fn sync_place(&mut self, place: &Place) {
        if let Some(n) = self.nav.as_deref_mut() {
            n.set_scope(place.in_instance(), place.world_name());
        }
    }

    /// How long (metres) a walk should be cut into legs by the goals that do: much longer than
    /// the straight line's 90 s when the mesh plans the way.
    pub fn leg_m(&self) -> f32 {
        self.nav
            .as_ref()
            .map_or(super::nav::trip::LEGACY_LEG_M, |n| n.leg_m())
    }

    /// The rest of the trip as map points (at most `max`, nearest first), empty when the bot is
    /// not moving. The route being followed, or the straight line to the destination.
    pub fn route_points(&self, out: &mut Vec<[f32; 2]>, max: usize) {
        if self.status != MotionStatus::Moving {
            return;
        }
        if self.nav.as_ref().is_some_and(|n| n.route_points(out, max)) {
            return;
        }
        if let Some(&(x, y)) = self.path.last() {
            out.push([self.pos[0], self.pos[1]]);
            out.push([x, y]);
        }
    }

    /// Ends the navigation trip, if any, and counts it as `how`.
    fn nav_end(&mut self, how: TripEnd) {
        if let Some(n) = self.nav.as_deref_mut() {
            n.end(how);
        }
    }

    /// Places the bot in the world (after the spawn): its object id, position and facing.
    pub fn enter_world(&mut self, obj: u32, pos: [f32; 3], yaw: f32, now_ms: u64) {
        self.obj = obj;
        self.move_obj = obj;
        self.pos = pos;
        self.sent_pos = pos;
        self.yaw = yaw;
        self.path.clear();
        self.nav_end(TripEnd::Cancelled);
        self.status = MotionStatus::Idle;
        self.moving_sent = false;
        self.stop_due = false;
        self.mode = TripMode::None;
        self.last_tick_ms = now_ms;
        self.life_reset();
    }

    /// Takes the bot out of the world (disconnect).
    pub fn leave_world(&mut self) {
        self.obj = 0;
        self.move_obj = 0;
        self.path.clear();
        self.nav_end(TripEnd::Cancelled);
        self.status = MotionStatus::Idle;
        self.moving_sent = false;
        self.stop_due = false;
        self.mode = TripMode::None;
        self.life_reset();
    }

    /// Current position.
    pub fn pos(&self) -> [f32; 3] {
        self.pos
    }

    /// Current facing in radians (0 = +Y, counter-clockwise).
    pub fn yaw(&self) -> f32 {
        self.yaw
    }

    /// What the controller is doing.
    pub fn status(&self) -> MotionStatus {
        self.status
    }

    /// Whether a trip is under way.
    pub fn is_moving(&self) -> bool {
        self.status == MotionStatus::Moving
    }

    /// Whether the controller still owes the server a packet (so the bot is not "idle" yet).
    pub fn busy(&self) -> bool {
        self.is_moving() || self.stop_due
    }

    /// Horizontal distance to the end of the path, `None` when not moving.
    pub fn remaining(&self) -> Option<f32> {
        let last = self.path.last()?;
        Some(dist2d(self.pos, [last.0, last.1, 0.0]))
    }

    /// The destination of the current trip.
    pub fn destination(&self) -> Option<(f32, f32)> {
        self.path.last().copied()
    }

    /// Speed of a pace in m/s.
    pub fn speed(&self, pace: Pace) -> f32 {
        let k = self.life.as_ref().map_or(1.0, |l| l.traits.speed_k);
        match pace {
            Pace::Run => self.cfg.run_speed * k,
            Pace::Walk => self.cfg.walk_speed * k,
        }
    }

    /// Starts a trip to (x, y); the bot stops `radius` metres short of it.
    pub fn go_to(&mut self, x: f32, y: f32, radius: f32, pace: Pace, now_ms: u64) {
        self.go_via_counted(&[(x, y)], None, radius, pace, now_ms, true);
    }

    /// Like [`Motion::go_to`], for a goal on a named floor: the navmesh search uses `z` to find
    /// the floor (a house's upper storey, a cave under the terrain) and the trip is not over
    /// until the bot is within `radius` of it in three dimensions.
    pub fn go_to_z(&mut self, x: f32, y: f32, z: f32, radius: f32, pace: Pace, now_ms: u64) {
        self.go_via_counted(&[(x, y)], Some(z), radius, pace, now_ms, true);
    }

    /// Starts a trip through `points` (the last one is the destination).
    pub fn go_via(&mut self, points: &[(f32, f32)], radius: f32, pace: Pace, now_ms: u64) {
        self.go_via_counted(points, None, radius, pace, now_ms, true);
    }

    /// Like [`Motion::go_to`] for the combat controller's movements (the chase of a monster, a
    /// step to the side, a retreat, the walk to loot): the trip is planned and walked the same,
    /// but it is not a trip in the navigation statistics, because a chase restarts whenever the
    /// monster moves and ends whenever the bot is in range, which is not a walk that fails to
    /// arrive.
    pub fn chase_to(&mut self, x: f32, y: f32, radius: f32, pace: Pace, now_ms: u64) {
        self.go_via_counted(&[(x, y)], None, radius, pace, now_ms, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn go_via_counted(
        &mut self,
        points: &[(f32, f32)],
        goal_z: Option<f32>,
        radius: f32,
        pace: Pace,
        now_ms: u64,
        counted: bool,
    ) {
        // A goal that asks for the same trip again every tick must not cancel a glide.
        if self.gliding()
            && points.len() == 1
            && self.path.len() == 1
            && self.path[0] == points[0]
            && (self.radius - radius).abs() < 0.01
        {
            return;
        }
        let pace = self.life_pace(points, pace, counted, now_ms);
        if let Some(l) = self.life.as_deref_mut() {
            l.fidget.reset();
            l.dawdle.reset();
            l.pause_until_ms = 0;
            l.rejump_ms = 0;
            l.cosmetic = counted;
        }
        self.path.clear();
        self.path.extend(
            points
                .iter()
                .copied()
                .filter(|p| p.0.is_finite() && p.1.is_finite()),
        );
        self.radius = radius.max(0.0);
        self.pace = pace;
        self.mode = TripMode::None;
        self.started_ms = now_ms;
        self.stuck.reset(now_ms, self.pos);
        self.force_send = true;
        self.status = if self.path.is_empty() {
            MotionStatus::Idle
        } else {
            MotionStatus::Moving
        };
        let pos = self.pos;
        let single = (self.path.len() == 1).then(|| self.path[0]);
        if let Some(n) = self.nav.as_deref_mut() {
            match single {
                Some((x, y)) => n.begin(
                    pos,
                    [x, y],
                    goal_z.filter(|z| z.is_finite()),
                    self.radius,
                    now_ms,
                    counted,
                ),
                None => n.end(TripEnd::Cancelled),
            }
        }
    }

    /// Like [`Motion::go_to`], but a trip longer than `teleport_over_m` (when positive) asks for a
    /// GM `/move`: the destination `[x, y, z]` is returned and the caller sends the command (all
    /// GM lines are made in one place, `Ctx::gm`) and then calls [`Motion::begin_teleport`]; if
    /// the server does not move the bot within [`TELEPORT_WAIT_MS`] the bot walks. Only call with
    /// `teleport_over_m > 0` when the account may use GM commands.
    pub fn travel_to(
        &mut self,
        x: f32,
        y: f32,
        radius: f32,
        pace: Pace,
        teleport_over_m: f32,
        env: &mut MotionEnv<'_>,
    ) -> Option<[f32; 3]> {
        self.go_to(x, y, radius, pace, env.now_ms);
        if teleport_over_m > 0.0 && dist2d(self.pos, [x, y, 0.0]) > teleport_over_m {
            let z = env
                .ground
                .ground_z(x, y)
                .map_or(self.pos[2], |g| surface_z(Some(g), self.pos[2]) + 1.0);
            return Some([x, y, z]);
        }
        None
    }

    /// The `/move` asked for by [`Motion::travel_to`] was sent: wait for the server to move the
    /// bot instead of walking.
    pub fn begin_teleport(&mut self, now_ms: u64) {
        self.life_end_jump();
        self.mode = TripMode::Teleport {
            deadline_ms: now_ms + TELEPORT_WAIT_MS,
        };
    }

    /// Whether a GM `/move` waits for the server to move the bot.
    pub fn teleporting(&self) -> bool {
        matches!(self.mode, TripMode::Teleport { .. })
    }

    /// Whether the bot is gliding down a ledge.
    pub fn gliding(&self) -> bool {
        matches!(self.mode, TripMode::Glide(_))
    }

    /// Puts the walking and running speeds (the mount feature drives both while the bot rides).
    pub fn set_speeds(&mut self, run: f32, walk: f32) {
        if self.cfg.run_speed != run {
            self.cfg.run_speed = run;
        }
        if self.cfg.walk_speed != walk {
            self.cfg.walk_speed = walk;
        }
    }

    /// Starts a glide along `dir` at `speed` m/s, descending `descent` m/s. The bot must be on a
    /// trip; the caller has found the ledge ([`crate::bot::mount::plan::find_ledge`]). The glide
    /// follows the fixed heading (it cuts the corner the navmesh would walk around) and ends when
    /// the terrain meets the descent; the navmesh route is planned again from there.
    pub fn begin_glide(&mut self, dir: (f32, f32), speed: f32, descent: f32, now_ms: u64) -> bool {
        let len = dir.0.hypot(dir.1);
        if self.obj == 0
            || self.status != MotionStatus::Moving
            || self.path.is_empty()
            || self.nav.as_ref().is_some_and(|n| n.waiting())
            || matches!(self.mode, TripMode::Teleport { .. })
            || !len.is_finite()
            || len < 0.9
            || !speed.is_finite()
            || speed <= 0.0
            || !descent.is_finite()
            || descent <= 0.0
        {
            return false;
        }
        self.life_end_jump();
        self.mode = TripMode::Glide(Box::new(Glide {
            dir: (dir.0 / len, dir.1 / len),
            speed,
            descent,
            started_ms: now_ms,
            airborne: false,
        }));
        self.force_send = true;
        self.stuck.reset(now_ms, self.pos);
        true
    }

    /// Ends a glide without a landing packet (a death, a teleport, a new trip). The next ordinary
    /// update puts the bot back on the ground.
    pub fn end_glide(&mut self) {
        if matches!(self.mode, TripMode::Glide(_)) {
            self.mode = TripMode::None;
            self.force_send = true;
        }
    }

    /// Puts the bot at height `z` (it keeps its place on the map and its trip): the server hears
    /// of it with the next update, which is sent at once. For the dungeon goal in an instance,
    /// where the bot has no ground under it, see `goals::dungeon`.
    pub fn set_height(&mut self, z: f32) {
        if !z.is_finite() {
            return;
        }
        self.pos[2] = z;
        self.force_send = true;
        self.life_end_jump();
        self.end_glide();
        if !self.is_moving() {
            self.stop_due = true;
        }
    }

    /// Stops walking; a stopped update follows on the next tick.
    pub fn stop(&mut self) {
        if self.status == MotionStatus::Moving || self.moving_sent {
            self.stop_due = true;
        }
        if self.life_end_jump() {
            // (the bot is in the air: the server has to hear that it stands)
            self.stop_due = true;
        }
        if let Some(l) = self.life.as_deref_mut() {
            l.fidget.reset();
            l.pause_until_ms = 0;
        }
        self.path.clear();
        self.mode = TripMode::None;
        self.status = MotionStatus::Idle;
        let pos = self.pos;
        if let Some(n) = self.nav.as_deref_mut() {
            n.stopped_at(pos);
        }
    }

    /// Turns towards (x, y); a stopped update with the new facing follows if the bot stands.
    pub fn face(&mut self, x: f32, y: f32) {
        let yaw = cs::yaw_towards((self.pos[0], self.pos[1]), (x, y));
        if (yaw - self.yaw).abs() > FACE_EPSILON {
            self.yaw = yaw;
            if !self.is_moving() {
                self.stop_due = true;
            }
        }
    }

    /// The server put the bot at `pos` (teleport, resurrection, instance load): forget the
    /// trip unless a GM teleport was what moved it.
    pub fn teleported(&mut self, pos: [f32; 3]) {
        self.pos = pos;
        self.sent_pos = pos;
        self.life_reset();
        self.nav_end(TripEnd::Cancelled);
        let was_teleport = matches!(self.mode, TripMode::Teleport { .. });
        self.mode = TripMode::None;
        if was_teleport {
            if let Some(&(x, y)) = self.path.last() {
                if dist2d(pos, [x, y, 0.0]) <= self.radius + 10.0 {
                    self.path.clear();
                    self.status = MotionStatus::Arrived;
                }
            }
            self.stuck.reset(self.last_tick_ms, pos);
        } else {
            self.path.clear();
            self.status = MotionStatus::Idle;
        }
        self.moving_sent = false;
        self.stop_due = false;
    }

    /// A navigation mesh search finished: `result` answers the request that was made with
    /// `ticket`. Delivered by `BotCmd::PathReady`; an answer for a request the trip no longer
    /// waits for is dropped.
    pub fn on_path_ready(&mut self, ticket: u32, result: aa_world::nav::PathResult) {
        let now = self.last_tick_ms;
        if let Some(n) = self.nav.as_deref_mut() {
            n.on_result(ticket, result, now);
        }
    }

    /// Ends a jump in progress. `true` when the bot was in the air.
    fn life_end_jump(&mut self) -> bool {
        match self.life.as_deref_mut() {
            Some(l) => {
                let was = l.jump.is_some();
                l.end_jump();
                was
            }
            None => false,
        }
    }

    /// Forgets everything life-like that belonged to the place the bot was at.
    fn life_reset(&mut self) {
        if let Some(l) = self.life.as_deref_mut() {
            l.end_jump();
            l.fidget.reset();
            l.dawdle.reset();
            l.pause_until_ms = 0;
            l.pace_memo = None;
        }
    }

    /// The pace of a new trip: in a town, in front of a player, some short trips are walked.
    /// The same goal started again keeps the pace it got.
    fn life_pace(&mut self, points: &[(f32, f32)], pace: Pace, counted: bool, now_ms: u64) -> Pace {
        /// Chance that a short trip in a town is walked.
        const TOWN_WALK: f32 = 0.25;
        /// Only trips this short (m) are walked for the looks.
        const TOWN_WALK_MAX_M: f32 = 60.0;
        let (Some(l), Some(&(gx, gy))) = (self.life.as_deref_mut(), points.last()) else {
            return pace;
        };
        if pace != Pace::Run || !counted || !l.gate.show() || !l.gate.town {
            return pace;
        }
        if let Some((g, until, walk)) = l.pace_memo {
            if now_ms < until && (g[0] - gx).abs() < 3.0 && (g[1] - gy).abs() < 3.0 {
                return if walk { Pace::Walk } else { Pace::Run };
            }
        }
        let near = dist2d(self.pos, [gx, gy, 0.0]) <= TOWN_WALK_MAX_M;
        let walk = near && l.rng.f32() < TOWN_WALK;
        l.pace_memo = Some(([gx, gy], now_ms + 60_000, walk));
        if walk {
            Pace::Walk
        } else {
            pace
        }
    }

    /// Straight way ahead before the next corner or the end of the trip (m).
    fn straight_ahead(&self) -> f32 {
        if let Some(s) = self.nav.as_ref().and_then(|n| n.straight_left()) {
            return s;
        }
        match self.path.first() {
            Some(&(x, y)) => {
                let stop_short = if self.path.len() == 1 {
                    self.radius
                } else {
                    0.0
                };
                (dist2d(self.pos, [x, y, 0.0]) - stop_short).max(0.0)
            }
            None => 0.0,
        }
    }

    /// Decides whether the bot jumps now, and starts the jump. Called once per tick while no
    /// jump is in progress.
    fn consider_jump(&mut self, env: &mut MotionEnv<'_>, heading: (f32, f32), speed: f32) {
        let now = env.now_ms;
        let moving = !self.path.is_empty() && heading != (0.0, 0.0);
        let Some(l) = self.life.as_deref_mut() else {
            return;
        };
        if l.rejump_ms != 0 {
            // the second jump of a double
            if now >= l.rejump_ms {
                l.rejump_ms = 0;
                if moving && l.gate.show() && l.jump.is_none() {
                    l.jump = Some(manner::Jump::start(now, manner::JumpKind::Full));
                    tracing::debug!(target: "pb_life", kind = ?manner::JumpKind::Full, "second jump");
                }
            }
            return;
        }
        if l.jump.is_some()
            || l.traits.jump_mult <= 0.0
            || !l.gate.show()
            || l.emotes.sitting
            || env.combat
            || !matches!(self.mode, TripMode::None)
            || self.pos[2] == SWIM_Z
            || env.out.iter().any(range_checked)
        {
            return;
        }
        let running = moving && speed > 3.0;
        let step_up = running && {
            let (x, y) = (
                self.pos[0] + heading.0 * manner::LOOKAHEAD_M,
                self.pos[1] + heading.1 * manner::LOOKAHEAD_M,
            );
            manner::is_step(
                env.ground.ground_z(self.pos[0], self.pos[1]),
                env.ground.ground_z(x, y),
            )
        };
        let look = Look {
            moving,
            running,
            straight_m: if running { self.straight_ahead() } else { 0.0 },
            step_up,
        };
        let Some(l) = self.life.as_deref_mut() else {
            return;
        };
        let mult = l.traits.jump_mult;
        if let Some(kind) = l.sched.decide(now, &look, mult, env.rng) {
            l.jump = Some(manner::Jump::start(now, kind));
            tracing::debug!(target: "pb_life", ?kind, "jump");
        }
    }

    /// Integrates the position by the time since the last tick and sends the packets.
    pub fn tick(&mut self, env: &mut MotionEnv<'_>) -> MotionStatus {
        let now = env.now_ms;
        let raw_dt_ms = now.saturating_sub(self.last_tick_ms);
        let dt_ms = raw_dt_ms.min(MAX_STEP_MS);
        self.last_tick_ms = now;
        if raw_dt_ms > MAX_STEP_MS {
            // The task was descheduled: the missing progress is not the terrain's fault.
            self.stuck.reset(now, self.pos);
        }
        if self.obj == 0 {
            return self.status;
        }
        match self.mode {
            TripMode::Teleport { deadline_ms } => {
                if now < deadline_ms {
                    return self.status; // waiting for SCTeleportUnit
                }
                self.mode = TripMode::None;
            }
            TripMode::Glide(_) => return self.glide_tick(env, dt_ms),
            TripMode::None => {}
        }
        if self.path.is_empty() {
            if self.stop_due {
                self.send(env, false, (0.0, 0.0));
            } else {
                self.stand_jump(env);
            }
            return self.status;
        }
        // (a route from the mesh may be much longer than the 90 s of the straight line)
        let limit = TRIP_TIMEOUT_MS.max(self.nav.as_ref().map_or(0, |n| n.limit_ms()));
        if now.saturating_sub(self.started_ms) > limit {
            return self.fail(env, MoveFailure::Timeout);
        }
        match self.stuck.observe(now, self.pos) {
            StuckVerdict::Fine => {}
            StuckVerdict::GiveUp => {
                if let Some(n) = self.nav.as_deref() {
                    n.note_stuck();
                }
                return self.fail(env, MoveFailure::Stuck);
            }
            StuckVerdict::Sidestep => {
                // a route is searched again first; the straight line side-steps
                let pos = self.pos;
                let replanned = self
                    .nav
                    .as_deref_mut()
                    .is_some_and(|n| n.on_stuck(pos, now));
                if !replanned {
                    self.sidestep(env);
                }
                if let Some(l) = self.life.as_deref_mut() {
                    l.sched.stuck();
                }
            }
        }

        // a pause at a corner: the bot stands, the trip waits
        if let Some(until) = self.life.as_deref().map(|l| l.pause_until_ms) {
            if until != 0 {
                if now < until {
                    self.started_ms += dt_ms;
                    self.stuck.reset(now, self.pos);
                    return self.status;
                }
                if let Some(l) = self.life.as_deref_mut() {
                    l.pause_until_ms = 0;
                }
                self.force_send = true;
            }
        }

        let (heading, speed) = match self.nav_step(env, dt_ms) {
            Some(NavOutcome::Wait) => return self.wait_for_plan(env),
            Some(NavOutcome::Moved { heading, speed }) => (heading, speed),
            None => self.walk_straight(env, dt_ms),
        };
        if heading != (0.0, 0.0) {
            self.yaw = cs::yaw_towards((0.0, 0.0), heading);
        }

        if self.status == MotionStatus::Arrived {
            self.nav_end(TripEnd::Arrived);
            self.send(env, false, (0.0, 0.0));
        } else if heading != (0.0, 0.0) {
            self.moving_tick(env, heading, speed);
        }
        self.status
    }

    /// The packets of a tick in which the bot walked: the ordinary moving update, a packet of a
    /// jump in progress, or the stopped update that starts a pause at a corner.
    fn moving_tick(&mut self, env: &mut MotionEnv<'_>, heading: (f32, f32), speed: f32) {
        let now = env.now_ms;
        let vel = (heading.0 * speed, heading.1 * speed);
        let mut jumping = self.life.as_deref().is_some_and(|l| l.jump.is_some());
        if !jumping && self.life_trip() {
            if self.pause_at_corner(env, heading) {
                self.send(env, false, (0.0, 0.0));
                return;
            }
            self.consider_jump(env, heading, speed);
            jumping = self.life.as_deref().is_some_and(|l| l.jump.is_some());
        }
        if jumping {
            if self.jump_ready(now) {
                self.send_air(env, true, vel);
            }
            return;
        }
        // (the loop ticks a walker in view every gap, so every such tick sends: evenly paced)
        let since = now.saturating_sub(self.last_send_ms);
        let due =
            self.force_send || !self.moving_sent || since + SEND_SLACK_MS >= env.send_gap_ms.max(1);
        if due {
            self.send(env, true, vel);
        }
    }

    /// Whether the trip is a walk for its own sake (not a chase, a loot walk or a shuffle), the
    /// only kind the bot shows off on.
    fn life_trip(&self) -> bool {
        self.life.as_deref().is_some_and(|l| l.cosmetic)
    }

    /// Whether the packet of the jump in progress is due (the server needs the `time` of two
    /// packets to differ).
    fn jump_ready(&self, now_ms: u64) -> bool {
        now_ms > self.last_send_ms && self.life.as_deref().is_some_and(|l| l.jump_due(now_ms))
    }

    /// Looks at the corner the bot just took and decides whether to pause there.
    fn pause_at_corner(&mut self, env: &MotionEnv<'_>, heading: (f32, f32)) -> bool {
        let now = env.now_ms;
        let far = self.remaining().is_some_and(|r| r >= 15.0);
        let Some(l) = self.life.as_deref_mut() else {
            return false;
        };
        if !l.gate.show() || env.combat || !far {
            l.dawdle.reset();
            return false;
        }
        let dawdle = l.traits.dawdle;
        match l.dawdle.on_heading(now, heading, dawdle, &mut l.rng) {
            Some(ms) => {
                l.pause_until_ms = now + ms;
                true
            }
            None => false,
        }
    }

    /// A standing bot: maybe a jump in place, and the packets of one in progress.
    fn stand_jump(&mut self, env: &mut MotionEnv<'_>) {
        let now = env.now_ms;
        if self.life.is_none() {
            return;
        }
        if self.life.as_deref().is_some_and(|l| l.jump.is_none()) {
            self.consider_jump(env, (0.0, 0.0), 0.0);
        }
        if self.jump_ready(now) {
            self.send_air(env, false, (0.0, 0.0));
        }
    }

    /// Sends the packet of the jump in progress that is due. The touchdown packet is followed by
    /// an ordinary ground update that ends the jump (a double jump takes off again a moment after
    /// the touchdown instead).
    fn send_air(&mut self, env: &mut MotionEnv<'_>, moving: bool, vel: (f32, f32)) {
        let now = env.now_ms;
        let step = self
            .life
            .as_deref_mut()
            .and_then(|l| l.jump.as_mut().and_then(|j| j.step(now)));
        let Some(step) = step else {
            return;
        };
        let end = step == AirStep::Settle
            || (step == AirStep::Land
                && self
                    .life
                    .as_deref()
                    .is_some_and(|l| l.jump.is_some_and(|j| j.again())));
        let air = (step != AirStep::Settle).then_some(step);
        if end {
            if let Some(l) = self.life.as_deref_mut() {
                let again = l.jump.is_some_and(|j| j.again());
                l.jump = None;
                l.rejump_ms = if again {
                    now + manner::DOUBLE_GAP_MS
                } else {
                    0
                };
            }
        }
        let packet = self.packet(now, env.combat, moving, vel, air);
        env.out.push(packet);
    }

    /// One tick of a glide: moves along the fixed heading and lets the descent meet the ground,
    /// then continues the trip (with a fresh navmesh route from where it landed).
    fn glide_tick(&mut self, env: &mut MotionEnv<'_>, dt_ms: u64) -> MotionStatus {
        let now = env.now_ms;
        let (dir, speed, descent, started_ms, was_airborne) = match &self.mode {
            TripMode::Glide(g) => (g.dir, g.speed, g.descent, g.started_ms, g.airborne),
            _ => return self.status,
        };
        let dt = dt_ms as f32 / 1000.0;
        self.pos[0] += dir.0 * speed * dt;
        self.pos[1] += dir.1 * speed * dt;
        self.yaw = cs::yaw_towards((0.0, 0.0), dir);
        let ground = env.ground.ground_z(self.pos[0], self.pos[1]);
        let surface = ground.map(|g| surface_z(Some(g), self.pos[2]));
        // The glide starts a step or two before the edge, on the ground: the bot keeps to the
        // ground until it falls away from under it and only then descends. (Landing on the first
        // tick of a start on level ground would end every glide before the edge.)
        let airborne = was_airborne || surface.is_some_and(|z| self.pos[2] > z + GLIDE_AIRBORNE_M);
        if airborne && !was_airborne {
            if let TripMode::Glide(g) = &mut self.mode {
                g.airborne = true;
            }
        }
        let run_up = !airborne && now.saturating_sub(started_ms) < GLIDE_RUN_UP_MS;
        let next_z = self.pos[2] - descent * dt;
        let land = match surface {
            Some(_) if run_up => false,
            Some(z) => next_z <= z + 0.02,
            // Without a ground under us, fall for three seconds and then settle.
            None => now.saturating_sub(started_ms) >= 3_000,
        };
        let reached = self
            .path
            .last()
            .is_some_and(|&(x, y)| dist2d(self.pos, [x, y, 0.0]) <= self.radius + 1.0);
        if land || reached || now.saturating_sub(started_ms) >= MAX_GLIDE_MS {
            if let Some(z) = surface {
                self.pos[2] = z;
            }
            self.mode = TripMode::None;
            self.stuck.reset(now, self.pos);
            if reached {
                self.path.clear();
                self.status = MotionStatus::Arrived;
            }
            self.replan_after_glide(now);
            self.force_send = true;
            let moving = self.status == MotionStatus::Moving;
            let vel = if moving {
                (dir.0 * speed, dir.1 * speed)
            } else {
                (0.0, 0.0)
            };
            let packet = self.packet(now, env.combat, moving, vel, None);
            env.out.push(packet);
            return self.status;
        }
        // (on the ground during the run-up, descending once the ground has fallen away)
        self.pos[2] = match surface {
            Some(z) if !airborne => z,
            _ => next_z,
        };
        self.status = MotionStatus::Moving;
        let due = self.force_send
            || !self.moving_sent
            || now.saturating_sub(self.last_send_ms) + SEND_SLACK_MS >= env.send_gap_ms.max(1);
        if due {
            let packet = self.packet(now, env.combat, true, (dir.0 * speed, dir.1 * speed), None);
            env.out.push(packet);
        }
        self.status
    }

    /// After a glide the navmesh route belongs to the old place (it may have run around the
    /// cliff): plan it again from where the bot landed, to the same goal.
    fn replan_after_glide(&mut self, now_ms: u64) {
        let Some(&(x, y)) = self.path.last() else {
            return;
        };
        if let Some(n) = self.nav.as_deref_mut() {
            // (the trip may have been asked for a floor: the new one keeps it)
            let goal_z = n.goal_z();
            n.end(TripEnd::Cancelled);
            n.begin(self.pos, [x, y], goal_z, self.radius, now_ms, false);
        }
    }

    /// Lets the navigation state move the bot for this tick. `None`: the straight line does.
    fn nav_step(&mut self, env: &MotionEnv<'_>, dt_ms: u64) -> Option<NavOutcome> {
        let (run, walk) = (self.speed(Pace::Run), self.speed(Pace::Walk));
        let nav = self.nav.as_deref_mut()?;
        let step = nav.step(&StepIn {
            now_ms: env.now_ms,
            dt_s: dt_ms as f32 / 1000.0,
            pos: self.pos,
            pace: self.pace,
            run,
            walk,
            ground: env.ground,
        });
        match step {
            NavStep::Legacy => None,
            NavStep::Wait => Some(NavOutcome::Wait),
            NavStep::Moved {
                pos,
                heading,
                speed,
                arrived,
            } => {
                self.pos = pos;
                if arrived {
                    self.path.clear();
                    self.status = MotionStatus::Arrived;
                }
                Some(NavOutcome::Moved { heading, speed })
            }
        }
    }

    /// The bot stands while a search runs: it turns towards the goal and, if the server last
    /// heard it moving, says that it stopped.
    fn wait_for_plan(&mut self, env: &mut MotionEnv<'_>) -> MotionStatus {
        self.stuck.reset(env.now_ms, self.pos);
        if let Some(&(gx, gy)) = self.path.last() {
            let yaw = cs::yaw_towards((self.pos[0], self.pos[1]), (gx, gy));
            if (yaw - self.yaw).abs() > FACE_EPSILON {
                self.yaw = yaw;
                self.stop_due = true;
            }
        }
        if self.moving_sent {
            self.stop_due = true;
        }
        if self.stop_due {
            self.send(env, false, (0.0, 0.0));
        }
        self.status
    }

    /// The straight line: integrates the position along the waypoints for `dt_ms` and puts the
    /// bot on the terrain. Returns the heading (zero when it did not move) and the speed.
    fn walk_straight(&mut self, env: &MotionEnv<'_>, dt_ms: u64) -> ((f32, f32), f32) {
        let speed = self.speed(self.pace);
        let from = self.pos;
        let mut budget = speed * dt_ms as f32 / 1000.0;
        let mut heading = (0.0f32, 0.0f32);
        while budget > 0.0 {
            let Some(&(tx, ty)) = self.path.first() else {
                break;
            };
            let last = self.path.len() == 1;
            let stop_short = if last { self.radius } else { 0.5 };
            let (dx, dy) = (tx - self.pos[0], ty - self.pos[1]);
            let dist = (dx * dx + dy * dy).sqrt();
            let to_go = dist - stop_short;
            if to_go <= 0.02 {
                if last {
                    self.path.clear();
                    self.status = MotionStatus::Arrived;
                    break;
                }
                self.path.remove(0);
                continue;
            }
            let (ux, uy) = (dx / dist, dy / dist);
            heading = (ux, uy);
            let step = budget.min(to_go);
            self.pos[0] += ux * step;
            self.pos[1] += uy * step;
            budget -= step;
            if step >= to_go - 1e-4 && last {
                self.path.clear();
                self.status = MotionStatus::Arrived;
                break;
            }
            if step >= to_go - 1e-4 {
                self.path.remove(0);
            }
        }
        self.pos[2] = surface_z(env.ground.ground_z(self.pos[0], self.pos[1]), self.pos[2]);
        if let Some(n) = self.nav.as_deref_mut() {
            n.walked(dist2d(from, self.pos));
        }
        (heading, speed)
    }

    fn fail(&mut self, env: &mut MotionEnv<'_>, why: MoveFailure) -> MotionStatus {
        self.path.clear();
        self.nav_end(TripEnd::Failed);
        self.status = MotionStatus::Failed(why);
        self.stop_due = self.moving_sent;
        if self.stop_due {
            self.send(env, false, (0.0, 0.0));
        }
        self.status
    }

    /// Inserts a waypoint 2..4 m to the side of the way ahead.
    fn sidestep(&mut self, env: &mut MotionEnv<'_>) {
        let Some(&(tx, ty)) = self.path.first() else {
            return;
        };
        let (dx, dy) = (tx - self.pos[0], ty - self.pos[1]);
        let len = (dx * dx + dy * dy).sqrt().max(0.001);
        let side = self.stuck.next_side();
        let off = 2.0 + env.rng.f32() * 2.0;
        let (px, py) = (-dy / len * off * side, dx / len * off * side);
        if self.path.len() < 6 {
            self.path.insert(0, (self.pos[0] + px, self.pos[1] + py));
        }
        self.force_send = true;
    }

    /// Queues one `CSMoveUnit` on the ground and updates the bookkeeping. A ground update ends
    /// a jump in progress (the bot lands where it is).
    fn send(&mut self, env: &mut MotionEnv<'_>, moving: bool, vel: (f32, f32)) {
        self.life_end_jump();
        let packet = self.packet(env.now_ms, env.combat, moving, vel, None);
        env.out.push(packet);
    }

    /// Builds one `CSMoveUnit` for the current position and updates the bookkeeping. `air` is
    /// the packet of a jump: the height above the ground and the vertical speed.
    fn packet(
        &mut self,
        now_ms: u64,
        combat: bool,
        moving: bool,
        vel: (f32, f32),
        air: Option<AirStep>,
    ) -> Out {
        let swimming = self.pos[2] == SWIM_Z;
        let gliding = self.gliding();
        let (lift, vz, jump) = if let TripMode::Glide(g) = &self.mode {
            (0.0, -g.descent, cs::JumpPhase::Air)
        } else {
            air.map_or((0.0, 0.0, cs::JumpPhase::None), |a| {
                (a.lift(), a.vz(), a.phase())
            })
        };
        let z = self.pos[2] + lift;
        let packet = cs::move_unit(&MoveUnit {
            obj: self.move_obj(),
            time: now_ms as u32,
            pos: (self.pos[0], self.pos[1], z),
            vel: (vel.0, vel.1, vz),
            yaw: self.yaw,
            moving,
            stance: if gliding {
                cs::STANCE_GLIDE
            } else if swimming {
                cs::STANCE_SWIM
            } else if combat {
                cs::STANCE_COMBAT
            } else {
                cs::STANCE_RELAXED
            },
            alertness: if combat {
                cs::ALERT_COMBAT
            } else {
                cs::ALERT_IDLE
            },
            jump,
        });
        self.moving_sent = moving;
        self.stop_due = false;
        self.force_send = false;
        self.last_send_ms = now_ms;
        self.sent_pos = [self.pos[0], self.pos[1], z];
        packet
    }

    /// An exact `CSMoveUnit` for the position the bot has now, when the server's idea of it is
    /// off: the bot stands but the server last heard it moving, or the last update is more than
    /// [`PIN_EPSILON_M`] away. Send it right before a packet the server checks a distance for
    /// (loot, talk, sell, craft, cast), at the position of the packet in the outbox: the movement
    /// updates of a walking bot are a second apart when nobody is near, which leaves the server
    /// metres behind. A standing bot gets a stopped update, a walking one a moving update.
    pub fn pin(&mut self, now_ms: u64, combat: bool) -> Option<Out> {
        if self.obj == 0 || self.teleporting() || self.gliding() {
            return None;
        }
        // (a bot that waits for a search stands, although its trip is under way)
        let waiting = self.nav.as_ref().is_some_and(|n| n.waiting());
        let paused = self
            .life
            .as_deref()
            .is_some_and(|l| l.pause_until_ms > now_ms);
        let moving = self.is_moving() && !waiting && !paused;
        let off = dist3d(self.sent_pos, self.pos) > PIN_EPSILON_M;
        // (a bot in the air lands: the server is told where it stands)
        let airborne = self.life_end_jump();
        if !off && !airborne && self.moving_sent == moving {
            return None;
        }
        let mut vel = (0.0, 0.0);
        let on_route = self.nav.as_ref().and_then(|n| n.velocity());
        if let Some((vx, vy)) = on_route.filter(|_| moving) {
            let len = (vx * vx + vy * vy).sqrt();
            if len > 1e-3 {
                vel = (vx, vy);
                self.yaw = cs::yaw_towards((0.0, 0.0), (vx / len, vy / len));
            }
        } else if moving {
            if let Some(&(tx, ty)) = self.path.first() {
                let (dx, dy) = (tx - self.pos[0], ty - self.pos[1]);
                let len = (dx * dx + dy * dy).sqrt();
                if len > 1e-3 {
                    let speed = self.speed(self.pace);
                    vel = (dx / len * speed, dy / len * speed);
                    self.yaw = cs::yaw_towards((0.0, 0.0), (dx / len, dy / len));
                }
            }
        }
        Some(self.packet(now_ms, combat, moving && vel != (0.0, 0.0), vel, None))
    }
}

/// Whether the server checks the distance between the bot and something for this packet: casts
/// (`TOO_FAR`), opening and emptying a loot window (5 m), talking to an NPC or doodad, quest
/// steps, crafting at a bench, selling a trade pack (2.5 m), buying from a merchant (3 m),
/// planting a seed (30 m) and boarding a mate (6 m).
pub fn range_checked(p: &Out) -> bool {
    p.level == LEVEL_GAME
        && matches!(
            p.opcode,
            op::CSStartSkillPacket
                | op::CSInteractNPCPacket
                | op::CSStartInteractionPacket
                | op::CSLootOpenBagPacket
                | op::CSLootItemPacket
                | op::CSStartQuestContextPacket
                | op::CSQuestTalkMadePacket
                | op::CSCompleteQuestContextPacket
                | op::CSExecuteCraft
                | op::CSSellBackpackGoodsPacket
                | op::CSBuyItemsPacket
                | op::CSCreateDoodadPacket
                | op::CSMountMatePacket
        )
}

/// Puts [`Motion::pin`]'s exact update in `out` right before the first [`range_checked`] packet
/// at or after index `from` (the packets the caller queued since it looked), when the server's
/// idea of the position is off.
pub fn pin_before_range_checked(
    motion: &mut Motion,
    out: &mut Vec<Out>,
    from: usize,
    now_ms: u64,
    combat: bool,
) {
    let Some(at) = out
        .get(from..)
        .and_then(|queued| queued.iter().position(range_checked))
    else {
        return;
    };
    if let Some(pin) = motion.pin(now_ms, combat) {
        out.insert(from + at, pin);
    }
}

/// Distance between two bot positions when the height matters (loot and interaction range).
pub fn reach(a: [f32; 3], b: [f32; 3]) -> f32 {
    dist3d(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    use aa_world::terrain::{HeightGridHeader, Terrain};

    const T0: u64 = 1_000;

    /// 64x64 samples of 4 m: a ramp rising 0.1 m per metre in x from 120 m.
    fn ramp() -> Terrain {
        let (w, h) = (64u32, 64u32);
        let header = HeightGridHeader {
            width: w,
            height: h,
            origin_x: 0.0,
            origin_y: 0.0,
            step: 4.0,
            scale: 1.0 / 32.0,
            offset: 0.0,
            flags: 0,
        };
        let mut samples = Vec::new();
        for _row in 0..h {
            for col in 0..w {
                let metres = 120.0 + col as f32 * 4.0 * 0.1;
                samples.push((metres * 32.0) as u16);
            }
        }
        Terrain::from_samples(header, samples).unwrap()
    }

    struct Rig {
        motion: Motion,
        out: Vec<Out>,
        rng: fastrand::Rng,
        now: u64,
    }

    impl Rig {
        fn new(pos: [f32; 3]) -> Rig {
            let mut motion = Motion::new(MotionCfg::default());
            motion.enter_world(0x400, pos, 0.0, T0);
            Rig {
                motion,
                out: Vec::new(),
                rng: fastrand::Rng::with_seed(4),
                now: T0,
            }
        }

        fn tick(&mut self, ground: &dyn Ground, dt: u64, gap: u64) -> MotionStatus {
            self.now += dt;
            let mut env = MotionEnv {
                ground,
                now_ms: self.now,
                out: &mut self.out,
                rng: &mut self.rng,
                send_gap_ms: gap,
                combat: false,
            };
            self.motion.tick(&mut env)
        }
    }

    fn flags(p: &Out) -> u8 {
        assert_eq!(p.opcode, op::CSMoveUnitPacket);
        p.body[8]
    }

    #[test]
    fn walks_a_straight_line_at_run_speed_and_stops_with_one_packet() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        assert_eq!(rig.motion.status(), MotionStatus::Moving);
        let mut ticks = 0;
        while rig.tick(&t, 250, 250) == MotionStatus::Moving {
            ticks += 1;
            assert!(ticks < 200);
        }
        assert_eq!(rig.motion.status(), MotionStatus::Arrived);
        // 50 m at 5.4 m/s = 9.26 s = 37 ticks of 250 ms (the last one is partial).
        assert!((36..=38).contains(&(ticks + 1)), "{ticks} ticks");
        let pos = rig.motion.pos();
        assert!(
            (pos[0] - 60.0).abs() < 0.01 && (pos[1] - 10.0).abs() < 0.01,
            "{pos:?}"
        );
        // z follows the terrain: 120 + 0.1 * 60
        assert!((pos[2] - 126.0).abs() < 0.2, "z = {}", pos[2]);
        // a moving update per tick, then exactly one stopped update
        let moving = rig.out.iter().filter(|p| flags(p) == 0x02).count();
        let stopped = rig.out.iter().filter(|p| flags(p) == 0x04).count();
        assert_eq!(stopped, 1);
        assert!(moving >= 36, "{moving}");
        assert_eq!(flags(rig.out.last().unwrap()), 0x04);
        // no further packets while standing
        let n = rig.out.len();
        rig.tick(&t, 250, 250);
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), n);
    }

    #[test]
    fn a_new_height_reaches_the_server_and_the_trip_goes_on() {
        // (inside an instance there is no ground)
        let none = |_: f32, _: f32| None::<f32>;
        let mut rig = Rig::new([10.0, 10.0, 167.0]);
        // standing: a stopped update follows at once
        rig.motion.set_height(110.0);
        assert_eq!(rig.motion.pos()[2], 110.0);
        rig.tick(&none, 250, 250);
        assert_eq!(rig.out.len(), 1);
        assert_eq!(flags(&rig.out[0]), 0x04);
        // walking: the trip goes on, at the new height
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        rig.tick(&none, 250, 250);
        rig.motion.set_height(120.0);
        assert_eq!(rig.motion.status(), MotionStatus::Moving);
        rig.tick(&none, 250, 250);
        assert_eq!(rig.motion.status(), MotionStatus::Moving);
        assert_eq!(rig.motion.pos()[2], 120.0);
        assert_eq!(flags(rig.out.last().unwrap()), 0x02);
        // a height that is no number is ignored
        rig.motion.set_height(f32::NAN);
        assert_eq!(rig.motion.pos()[2], 120.0);
    }

    #[test]
    fn faces_the_direction_of_travel() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        rig.tick(&t, 250, 250);
        // heading +X is yaw -pi/2 (0 faces +Y, positive is counter-clockwise)
        assert!((rig.motion.yaw() + std::f32::consts::FRAC_PI_2).abs() < 0.01);
        let first = &rig.out[0];
        assert_eq!(flags(first), 0x02);
        // rotation z byte at offset 3 (bc) + 1 + 4 + 1 + 9 + 6 + 2
        assert_eq!(
            first.body[26] as i8,
            cs::yaw_to_direction(-std::f32::consts::FRAC_PI_2)
        );
        // velocity x is the run speed in the client's units (32767 / 60 per m/s)
        let vx = i16::from_le_bytes([first.body[18], first.body[19]]);
        assert_eq!(vx, (5.4f32 * 32767.0 / 60.0).round() as i16);
    }

    /// The exact stopped update the bot would send now.
    fn stopped_here(rig: &Rig) -> Out {
        cs::move_unit(&MoveUnit {
            obj: 0x400,
            time: rig.now as u32,
            pos: (
                rig.motion.pos()[0],
                rig.motion.pos()[1],
                rig.motion.pos()[2],
            ),
            vel: (0.0, 0.0, 0.0),
            yaw: rig.motion.yaw(),
            moving: false,
            stance: cs::STANCE_RELAXED,
            alertness: cs::ALERT_IDLE,
            jump: cs::JumpPhase::None,
        })
    }

    #[test]
    fn pin_stops_the_server_lagging_behind_a_bot_that_halts() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        assert!(
            rig.motion.pin(rig.now, false).is_none(),
            "standing where the server put it"
        );
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        // nobody is near: one update, then a second between them
        for _ in 0..3 {
            rig.tick(&t, 250, 1000);
        }
        assert_eq!(rig.out.len(), 1, "the gap holds the updates back");
        // an action is about to be sent: the bot halts and the server must know where
        rig.motion.stop();
        let pin = rig
            .motion
            .pin(rig.now, false)
            .expect("the server is behind");
        assert_eq!(
            pin,
            stopped_here(&rig),
            "a stopped update at the exact position"
        );
        assert!(rig.motion.pos()[0] > 14.0, "the bot has run on");
        // nothing else is owed: no second stopped update, no second pin
        assert!(rig.motion.pin(rig.now, false).is_none());
        let n = rig.out.len();
        rig.tick(&t, 250, 1000);
        assert_eq!(rig.out.len(), n);
    }

    #[test]
    fn pin_of_a_walking_bot_is_a_moving_update_and_a_standing_bot_needs_none() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        for _ in 0..3 {
            rig.tick(&t, 250, 1000);
        }
        let pin = rig.motion.pin(rig.now, true).expect("the server is behind");
        assert_eq!(flags(&pin), 0x02, "still walking");
        // the walk goes on from the pinned position; the next update waits for the gap
        let n = rig.out.len();
        rig.tick(&t, 250, 1000);
        assert_eq!(rig.out.len(), n);
        // arrival sends the stopped update itself: a pin after it has nothing to add
        rig.motion.go_to(40.0, 10.0, 0.0, Pace::Run, rig.now);
        while rig.tick(&t, 250, 1000) == MotionStatus::Moving {}
        assert_eq!(flags(rig.out.last().unwrap()), 0x04);
        assert!(rig.motion.pin(rig.now, false).is_none());
        // a teleport is the server's own placement
        rig.motion.teleported([500.0, 500.0, 130.0]);
        assert!(rig.motion.pin(rig.now, false).is_none());
    }

    #[test]
    fn the_pin_goes_before_the_first_range_checked_packet() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        for _ in 0..3 {
            rig.tick(&t, 250, 1000);
        }
        rig.motion.stop();
        // a ping, then an unrelated chat, then a loot request and a cast
        let mut out = vec![cs::ping(1), cs::chat(cs::chat_type::SAY, "", "hi")];
        let from = out.len();
        out.push(cs::loot_open_bag(7, true));
        out.push(cs::start_skill_unit(18132, 0x400, 9));
        assert!(range_checked(&out[2]) && range_checked(&out[3]));
        assert!(!range_checked(&out[0]) && !range_checked(&out[1]));
        // boarding a mate is judged by the server against the position it last heard (6 m)
        assert!(range_checked(&cs::mount_mate(
            7,
            1,
            cs::MOUNT_REASON_FORWARD
        )));
        assert!(!range_checked(&cs::unmount_mate(
            7,
            1,
            cs::MOUNT_REASON_FORWARD
        )));
        pin_before_range_checked(&mut rig.motion, &mut out, from, rig.now, false);
        assert_eq!(out.len(), 5);
        assert_eq!(out[2], stopped_here(&rig), "right before the loot request");
        assert_eq!(out[3].opcode, op::CSLootOpenBagPacket);
        // once is enough: the second packet needs no second pin
        pin_before_range_checked(&mut rig.motion, &mut out, from, rig.now, false);
        assert_eq!(out.len(), 5);
        // packets that are not range checked, or queued before `from`, get none
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(60.0, 10.0, 0.0, Pace::Run, rig.now);
        for _ in 0..3 {
            rig.tick(&t, 250, 1000);
        }
        rig.motion.stop();
        let mut out = vec![cs::loot_open_bag(7, true), cs::ping(1)];
        pin_before_range_checked(&mut rig.motion, &mut out, 1, rig.now, false);
        assert_eq!(out.len(), 2);
        assert!(
            rig.motion.pin(rig.now, false).is_some(),
            "nothing was spent"
        );
    }

    #[test]
    fn stops_at_the_requested_radius() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(40.0, 10.0, 3.0, Pace::Run, rig.now);
        while rig.tick(&t, 250, 250) == MotionStatus::Moving {}
        let pos = rig.motion.pos();
        assert!((pos[0] - 37.0).abs() < 0.05, "{pos:?}");
        assert_eq!(rig.motion.status(), MotionStatus::Arrived);
        // already inside the radius: arrives without moving
        rig.motion.go_to(38.0, 10.0, 3.0, Pace::Run, rig.now);
        assert_eq!(rig.tick(&t, 250, 250), MotionStatus::Arrived);
        assert!((rig.motion.pos()[0] - 37.0).abs() < 0.05);
    }

    #[test]
    fn walk_pace_is_slower_and_waypoints_are_followed_in_order() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion
            .go_via(&[(10.0, 20.0), (20.0, 20.0)], 0.0, Pace::Walk, rig.now);
        let mut seconds = 0.0;
        while rig.tick(&t, 250, 250) == MotionStatus::Moving {
            seconds += 0.25;
            assert!(seconds < 60.0);
        }
        // 20 m at 1.8 m/s = 11 s
        assert!((10.5..=12.0).contains(&seconds), "{seconds}");
        let pos = rig.motion.pos();
        assert!((pos[0] - 20.0).abs() < 0.05 && (pos[1] - 20.0).abs() < 0.05);
        // it went via (10, 20): the first moving packet heads +Y (yaw 0)
        assert_eq!(rig.out[0].body[26] as i8, 0);
    }

    #[test]
    fn sea_floor_is_swum_over_and_unknown_ground_keeps_the_height() {
        let sea = |_: f32, _: f32| Some(60.0);
        let mut rig = Rig::new([10.0, 10.0, 98.5]);
        rig.motion.go_to(30.0, 10.0, 0.0, Pace::Run, rig.now);
        while rig.tick(&sea, 250, 250) == MotionStatus::Moving {}
        assert_eq!(rig.motion.pos()[2], SWIM_Z);

        let nothing = |_: f32, _: f32| None;
        let mut rig = Rig::new([10.0, 10.0, 133.0]);
        rig.motion.go_to(30.0, 10.0, 0.0, Pace::Run, rig.now);
        while rig.tick(&nothing, 250, 250) == MotionStatus::Moving {}
        assert_eq!(rig.motion.pos()[2], 133.0);

        assert_eq!(surface_z(Some(98.9), 0.0), SWIM_Z);
        assert_eq!(surface_z(Some(99.0), 0.0), 99.0);
        assert_eq!(surface_z(Some(300.0), 0.0), 300.0);
        assert_eq!(surface_z(None, 7.0), 7.0);
    }

    #[test]
    fn the_send_rate_drops_when_nobody_watches() {
        let t = ramp();
        let count = |gap: u64| {
            let mut rig = Rig::new([10.0, 10.0, 121.0]);
            rig.motion.go_to(400.0, 10.0, 0.0, Pace::Run, rig.now);
            for _ in 0..40 {
                rig.tick(&t, 250, gap); // 10 s
            }
            rig.out.iter().filter(|p| flags(p) == 0x02).count()
        };
        let busy = count(250);
        let quiet = count(1000);
        assert_eq!(busy, 40);
        assert!((10..=11).contains(&quiet), "{quiet}");
    }

    /// A walker a real player can see is ticked every `move_update_ms` and sends an update on
    /// every tick, evenly, also when the timer wakes it a few ms early or late; the position of
    /// each update is where the one before said the bot would be.
    #[test]
    fn a_walker_in_view_sends_evenly_paced_updates_that_agree_with_each_other() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(400.0, 10.0, 0.0, Pace::Run, rig.now);
        let mut times = Vec::new();
        for i in 0..80u64 {
            // 125 ms on average, the timer off by up to 8 ms either way
            let dt = 125 + (i % 3) * 8 - 8;
            rig.tick(&t, dt, 125);
            times.push(rig.now);
        }
        let moving: Vec<&Out> = rig.out.iter().filter(|p| flags(p) == 0x02).collect();
        assert_eq!(moving.len(), 80, "an update on every tick");
        let read = |p: &Out| {
            let mut s = aa_proto::PacketStream::from_bytes(p.body.clone());
            s.read_bc().unwrap();
            s.read_u8().unwrap();
            let time = s.read_u32().unwrap();
            s.read_u8().unwrap();
            let (x, y, _) = s.read_position().unwrap();
            let vx = f32::from(s.read_i16().unwrap()) * 60.0 / 32767.0;
            (time, x, y, vx)
        };
        for w in moving.windows(2) {
            let (t0, x0, _, vx) = read(w[0]);
            let (t1, x1, _, _) = read(w[1]);
            let dt = (t1 - t0) as f32 / 1000.0;
            assert!((117..=133).contains(&(t1 - t0)), "{}", t1 - t0);
            // (dead reckoning from the update before: within the position's wire precision)
            assert!((x0 + vx * dt - x1).abs() < 0.05, "{x0} {vx} {dt} {x1}");
        }
    }

    #[test]
    fn stop_and_face_send_a_stopped_update() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(400.0, 10.0, 0.0, Pace::Run, rig.now);
        rig.tick(&t, 250, 250);
        rig.tick(&t, 250, 250);
        rig.out.clear();
        rig.motion.stop();
        assert_eq!(rig.motion.status(), MotionStatus::Idle);
        assert!(rig.motion.busy());
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), 1);
        assert_eq!(flags(&rig.out[0]), 0x04);
        assert!(!rig.motion.busy());
        // facing a point north while standing
        rig.motion
            .face(rig.motion.pos()[0], rig.motion.pos()[1] + 5.0);
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), 2);
        assert_eq!(rig.out[1].body[26] as i8, 0);
        // facing the same way again sends nothing
        rig.motion
            .face(rig.motion.pos()[0], rig.motion.pos()[1] + 9.0);
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), 2);
        // standing still and stopping again sends nothing
        rig.motion.stop();
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), 2);
    }

    #[test]
    fn a_bot_that_cannot_move_side_steps_and_then_gives_up() {
        let t = ramp();
        let cfg = MotionCfg {
            run_speed: 0.05, // effectively frozen
            ..MotionCfg::default()
        };
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion = Motion::new(cfg);
        rig.motion.enter_world(0x400, [10.0, 10.0, 121.0], 0.0, T0);
        rig.motion.go_to(200.0, 10.0, 0.0, Pace::Run, rig.now);
        let mut seconds = 0;
        let mut sides = 0;
        let mut last_path_len = 1;
        loop {
            let st = rig.tick(&t, 1000, 1000);
            seconds += 1;
            let len = rig.motion.path.len();
            if len > last_path_len {
                sides += 1;
            }
            last_path_len = len;
            if st != MotionStatus::Moving {
                assert_eq!(st, MotionStatus::Failed(MoveFailure::Stuck));
                break;
            }
            assert!(seconds < 60);
        }
        assert!((12..=17).contains(&seconds), "gave up after {seconds} s");
        assert!(sides >= 3, "{sides} side steps");
        // the failure ends with a stopped update
        assert_eq!(flags(rig.out.last().unwrap()), 0x04);
    }

    #[test]
    fn a_slow_trip_times_out() {
        let t = ramp();
        let cfg = MotionCfg {
            run_speed: 0.5, // 1.5 m per window: not stuck, but never arrives
            ..MotionCfg::default()
        };
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion = Motion::new(cfg);
        rig.motion.enter_world(0x400, [10.0, 10.0, 121.0], 0.0, T0);
        rig.motion.go_to(10.0, 600.0, 0.0, Pace::Run, rig.now);
        let mut seconds = 0;
        let end = loop {
            let st = rig.tick(&t, 1000, 1000);
            seconds += 1;
            if st != MotionStatus::Moving {
                break st;
            }
            assert!(seconds < 200);
        };
        assert_eq!(end, MotionStatus::Failed(MoveFailure::Timeout));
        assert!((90..=92).contains(&seconds), "{seconds}");
    }

    #[test]
    fn the_stuck_detector_in_isolation() {
        let mut s = Stuck::default();
        s.reset(0, [0.0, 0.0, 0.0]);
        assert_eq!(s.observe(1000, [0.0, 0.0, 0.0]), StuckVerdict::Fine);
        assert_eq!(s.observe(3000, [0.1, 0.0, 0.0]), StuckVerdict::Sidestep);
        // progress clears the count
        assert_eq!(s.observe(6000, [5.0, 0.0, 0.0]), StuckVerdict::Fine);
        for i in 0..4 {
            assert_eq!(
                s.observe(9000 + 3000 * i, [5.0, 0.0, 0.0]),
                StuckVerdict::Sidestep,
                "try {i}"
            );
        }
        assert_eq!(s.observe(21_000, [5.0, 0.0, 0.0]), StuckVerdict::GiveUp);
        // the side alternates
        assert_ne!(s.next_side(), s.next_side());
    }

    #[test]
    fn a_long_stall_does_not_teleport_the_bot() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(400.0, 10.0, 0.0, Pace::Run, rig.now);
        rig.tick(&t, 60_000, 250); // the task was descheduled for a minute
        assert!(
            (rig.motion.pos()[0] - 15.4).abs() < 0.01,
            "{:?}",
            rig.motion.pos()
        );
    }

    #[test]
    fn gm_teleport_for_long_trips_and_the_walking_fallback() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        let now = rig.now;
        {
            let mut env = MotionEnv {
                ground: &t,
                now_ms: now,
                out: &mut rig.out,
                rng: &mut rig.rng,
                send_gap_ms: 250,
                combat: false,
            };
            // short trips walk
            let jump = rig
                .motion
                .travel_to(30.0, 10.0, 0.0, Pace::Run, 100.0, &mut env);
            assert_eq!(jump, None);
        }
        assert!(rig.out.is_empty());
        assert_eq!(rig.motion.status(), MotionStatus::Moving);
        {
            let mut env = MotionEnv {
                ground: &t,
                now_ms: now,
                out: &mut rig.out,
                rng: &mut rig.rng,
                send_gap_ms: 250,
                combat: false,
            };
            let jump = rig
                .motion
                .travel_to(200.0, 10.0, 2.0, Pace::Run, 100.0, &mut env);
            // the motion controller sends no chat itself: it names the destination and the
            // caller (Ctx::gm) makes the safe GM line
            let [jx, jy, _] = jump.expect("a long trip asks for a teleport");
            assert_eq!((jx, jy), (200.0, 10.0));
        }
        assert!(rig.out.is_empty());
        rig.motion.begin_teleport(now);
        // while the teleport is pending the bot does not walk
        rig.tick(&t, 250, 250);
        assert_eq!(rig.motion.pos()[0], 10.0);
        // the server answers: arrived
        rig.motion.teleported([199.0, 10.0, 140.0]);
        assert_eq!(rig.motion.status(), MotionStatus::Arrived);
        assert_eq!(rig.motion.pos(), [199.0, 10.0, 140.0]);

        // an unanswered /move falls back to walking after the wait
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        let now = rig.now;
        {
            let mut env = MotionEnv {
                ground: &t,
                now_ms: now,
                out: &mut rig.out,
                rng: &mut rig.rng,
                send_gap_ms: 250,
                combat: false,
            };
            let jump = rig
                .motion
                .travel_to(200.0, 10.0, 0.0, Pace::Run, 100.0, &mut env);
            assert!(jump.is_some());
        }
        rig.motion.begin_teleport(now);
        for _ in 0..5 {
            rig.tick(&t, 2500, 250);
        }
        assert!(rig.motion.pos()[0] > 10.0);
    }

    #[test]
    fn a_server_teleport_cancels_the_trip() {
        let t = ramp();
        let mut rig = Rig::new([10.0, 10.0, 121.0]);
        rig.motion.go_to(100.0, 10.0, 0.0, Pace::Run, rig.now);
        rig.tick(&t, 250, 250);
        rig.motion.teleported([500.0, 500.0, 130.0]);
        assert_eq!(rig.motion.status(), MotionStatus::Idle);
        assert_eq!(rig.motion.pos(), [500.0, 500.0, 130.0]);
        let n = rig.out.len();
        rig.tick(&t, 250, 250);
        assert_eq!(rig.out.len(), n);
    }
}
