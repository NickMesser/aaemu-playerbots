//! The bot side of portal travel: what a tick does with [`Journey`]s and the recording duty.
//!
//! * [`Journey::tick`] runs one step of a journey the caller owns.
//! * [`far_trip`] is the hook of `Ctx::travel`: a trip over [`plan::FAR_TRIP_M`] metres goes to an
//!   implicit journey when the planner finds a portal route.
//! * [`duty`] is called by the brain on every tick: it steps the implicit journey, records
//!   Teleport Book entries at useful places, lets an idle bot hop through another bot's gate now and
//!   then and files the symptoms the casts showed.
//! * [`on_goal_end`] drops an implicit journey when the goal that asked for it ends.
//!
//! The decisions are in `journey.rs` (the state machine), `plan.rs` (the route) and `places.rs`
//! (where to record); this file only reads the bot, builds the [`Snap`] and carries out the [`Act`]s.

use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use aa_proto::cs;
use aa_world::travel::portal::{
    ZoneOrigins, BOUND_STONE_ITEM, MEMORY_INK_ITEM, RECORD_PORTAL_SKILL, STONE_ITEM,
    TELEPORT_BOOK_ITEM,
};
use smallvec::SmallVec;

use super::book::{self, dist, STOOD_TOLERANCE_M};
use super::journey::{
    Act, Acts, Dest, Fault, GateView, Journey, JourneyFail, JourneyStatus, Snap, OPEN_SKILL,
    RECALL_SKILL, REFUSALS,
};
use super::places;
use super::plan::{self, Cand, PlanIn, Route, FAR_TRIP_M, NEAR_ENTRY_M};
use super::{Block, Inner, Rec};
use crate::bot::cmd::GoalKind;
use crate::bot::ctx::Ctx;
use crate::bot::motion::{Pace, SWIM_THRESHOLD};
use crate::bot::state::StateError;
use crate::issues::{IssueKind, IssueReport, Severity, Suspect};

/// First record after the bot came into the world: not before this many ms (and up to twice that).
const FIRST_RECORD_MS: u64 = 60_000;
/// Time between two records of one bot, ms (jittered by half either way).
const RECORD_GAP_MS: u64 = 10 * 60_000;
/// How long after the record cast was sent the entry may take (the 5 s cast plus slack).
const RECORD_WAIT_MS: u64 = 10_000;
/// How long `CSDeletePortal` may take to be answered.
const DELETE_WAIT_MS: u64 = 4_000;
/// Another record is not tried within this long of the last failure, ms.
const RECORD_RETRY_MS: u64 = 5 * 60_000;
/// A place where the server refused the record cast is left alone for this long, ms.
const NO_RECORD_MS: u64 = 30 * 60_000;
/// ... within this many metres.
const NO_RECORD_M: f32 = 150.0;
/// `SkillResult` of a cast a buff forbids (`SKILL_REQ_FAIL`).
const SKILL_REQ_FAIL: u8 = 50;

/// The most Memory Ink a GM top-up adds.
const INK_TOPUP: u32 = 3;
/// Stones added on top of what the gate needs when a GM tops up.
const STONE_SPARE: u32 = 2;
/// A bot with fewer hit points (percent) does not record or hop.
const CALM_HP_PCT: u8 = 60;
/// Gate hops: a gate this close may be taken, metres.
const HOP_RANGE_M: f32 = 15.0;
/// Gate hops: a gate older than this is about to vanish, ms.
const HOP_MAX_AGE_MS: u64 = 18_000;
/// Gate hops: chance that an idle bot takes a gate it sees.
const HOP_CHANCE: f32 = 0.2;
/// Gate hops: pause between two hops of one bot, ms.
const HOP_GAP_MS: u64 = 30 * 60_000;
/// A destination whose portal route failed is walked for this long, ms.
const BLOCK_MS: u64 = 120_000;
/// A destination with no portal route at all is not asked again for this long, ms.
const NO_ROUTE_MS: u64 = 30_000;
/// After a failed open (refused, no gate) no gate is opened for this long, ms.
const OPEN_OFF_MS: u64 = 180_000;
/// After a failed recall none is tried for this long, ms.
const RECALL_OFF_MS: u64 = 30 * 60_000;
/// After a journey was blocked by a buff of the bot (the resurrection sickness lasts minutes) no
/// gate is opened and no recall is tried for this long, ms.
const BLOCKED_OFF_MS: u64 = 60_000;
/// Implicit journeys that no goal asked for within this long are dropped, ms.
const STALE_MS: u64 = 20_000;
/// Gates not in the unit view and older than this are forgotten, ms.
const GATE_FORGET_MS: u64 = 60_000;
/// The file the learned zone origin cells are kept in, next to the roster.
const ORIGINS_FILE: &str = "portal_zone_origins.txt";

// ---- counters ------------------------------------------------------------------------------------

/// Process-wide counters of the portal travel, for the logs and the live checks.
#[derive(Debug, Default)]
pub struct Counters {
    records_sent: AtomicU32,
    records_ok: AtomicU32,
    records_failed: AtomicU32,
    deletes: AtomicU32,
    opens_sent: AtomicU32,
    gates_seen: AtomicU32,
    uses_sent: AtomicU32,
    arrivals: AtomicU32,
    recalls_sent: AtomicU32,
    hops: AtomicU32,
    failures: AtomicU32,
    refusals: [AtomicU32; 6],
    faults: AtomicU32,
}

/// A copy of the counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Record casts sent.
    pub records_sent: u32,
    /// Records the server answered with a new entry.
    pub records_ok: u32,
    /// Records that failed.
    pub records_failed: u32,
    /// Book entries deleted to make room.
    pub deletes: u32,
    /// Open-gate casts sent.
    pub opens_sent: u32,
    /// Gates of the bot's own that appeared.
    pub gates_seen: u32,
    /// `CSUsePortal` sent.
    pub uses_sent: u32,
    /// Journeys that came through (a gate or a recall).
    pub arrivals: u32,
    /// Recall casts sent.
    pub recalls_sent: u32,
    /// Hops through another bot's gate begun.
    pub hops: u32,
    /// Journeys that failed (not counting "no route").
    pub failures: u32,
    /// Use refusals by `SCErrorMsg`: 154, 287, 609, 610, 611, other.
    pub refusals: [u32; 6],
    /// Symptoms reported as issues.
    pub faults: u32,
}

impl Counters {
    const fn new() -> Counters {
        Counters {
            records_sent: AtomicU32::new(0),
            records_ok: AtomicU32::new(0),
            records_failed: AtomicU32::new(0),
            deletes: AtomicU32::new(0),
            opens_sent: AtomicU32::new(0),
            gates_seen: AtomicU32::new(0),
            uses_sent: AtomicU32::new(0),
            arrivals: AtomicU32::new(0),
            recalls_sent: AtomicU32::new(0),
            hops: AtomicU32::new(0),
            failures: AtomicU32::new(0),
            refusals: [
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            faults: AtomicU32::new(0),
        }
    }

    pub(crate) fn record_ok(&self) {
        self.records_ok.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn gate_seen(&self) {
        self.gates_seen.fetch_add(1, Ordering::Relaxed);
    }

    fn bump(c: &AtomicU32) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    fn refusal(&self, code: i16) {
        let i = REFUSALS.iter().position(|c| *c == code).unwrap_or(5);
        Self::bump(&self.refusals[i]);
    }

    fn snapshot(&self) -> Stats {
        let g = |c: &AtomicU32| c.load(Ordering::Relaxed);
        Stats {
            records_sent: g(&self.records_sent),
            records_ok: g(&self.records_ok),
            records_failed: g(&self.records_failed),
            deletes: g(&self.deletes),
            opens_sent: g(&self.opens_sent),
            gates_seen: g(&self.gates_seen),
            uses_sent: g(&self.uses_sent),
            arrivals: g(&self.arrivals),
            recalls_sent: g(&self.recalls_sent),
            hops: g(&self.hops),
            failures: g(&self.failures),
            refusals: std::array::from_fn(|i| g(&self.refusals[i])),
            faults: g(&self.faults),
        }
    }
}

pub(crate) static STATS: Counters = Counters::new();

/// The process-wide counters of the portal travel.
pub fn stats() -> Stats {
    STATS.snapshot()
}

impl Stats {
    /// One line with every counter, for the logs.
    pub fn line(&self) -> String {
        format!(
            "portals: records {}/{} ok ({} failed, {} deleted), gates opened {} (seen {}), uses sent {}, \
             arrived {}, recalls {}, hops {}, failures {}, refusals 154:{} 287:{} 609:{} 610:{} 611:{} other:{}, issues {}",
            self.records_ok,
            self.records_sent,
            self.records_failed,
            self.deletes,
            self.opens_sent,
            self.gates_seen,
            self.uses_sent,
            self.arrivals,
            self.recalls_sent,
            self.hops,
            self.failures,
            self.refusals[0],
            self.refusals[1],
            self.refusals[2],
            self.refusals[3],
            self.refusals[4],
            self.refusals[5],
            self.faults,
        )
    }
}

// ---- zone origins, shared by all bots ----------------------------------------------------------------

struct Origins {
    table: ZoneOrigins,
    loaded: bool,
}

static ORIGINS: Mutex<Origins> = Mutex::new(Origins {
    table: ZoneOrigins::EMPTY,
    loaded: false,
});

/// Runs `f` on the learned zone origin cells, which every bot shares (the cells are a property of
/// the world, not of a character). The table is read from `dir` on first use and written back when
/// `f` changed it.
fn with_origins<R>(dir: Option<&Path>, f: impl FnOnce(&mut ZoneOrigins) -> R) -> R {
    let mut g = ORIGINS.lock().unwrap_or_else(|e| e.into_inner());
    let path = dir.map(|d| d.join(ORIGINS_FILE));
    if !g.loaded {
        g.loaded = true;
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            g.table = ZoneOrigins::from_text(&text);
        }
    }
    let before = g.table.clone();
    let r = f(&mut g.table);
    if g.table != before {
        if let Some(p) = &path {
            let _ = std::fs::write(p, g.table.to_text());
        }
    }
    r
}

// ---- running a journey ------------------------------------------------------------------------------

impl Journey {
    /// One tick of a journey the caller owns: reads the bot, steps the state machine and does what
    /// it asks (walks to the gate, casts, sends `CSUsePortal`). Call it every tick of the goal
    /// until it returns something other than [`JourneyStatus::Running`]; on `Failed` the caller
    /// walks. While [`Journey::holds`] is true the caller must not move the bot or start casts.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> JourneyStatus {
        if !ctx.cfg.behavior.portals {
            return JourneyStatus::Failed(JourneyFail::NoRoute);
        }
        let Some(mut inner) = ctx.state.travel.take(true) else {
            return JourneyStatus::Failed(JourneyFail::NoRoute);
        };
        let st = run(self, ctx, &mut inner);
        ctx.state.travel.put(inner);
        st
    }

    /// Abandons the journey (the owner changed its mind): stops walking to a gate.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if self.holds() && !self.casting() && ctx.motion.is_moving() {
            ctx.motion.stop();
        }
    }
}

/// One step of `j`, with its side effects.
fn run(j: &mut Journey, ctx: &mut Ctx<'_>, inner: &mut Inner) -> JourneyStatus {
    refresh(ctx, inner);
    let route = match j.dest() {
        Dest::Point { x, y } if j.needs_route() => Some(plan_route(ctx, inner, [x, y])),
        _ => None,
    };
    let spot = j.needs_spot().then(|| gate_spot(ctx)).flatten();
    let gates = live_gates(ctx, inner);
    let now = ctx.now_ms;
    let snap = Snap {
        now_ms: now,
        pos: ctx.state.me.pos,
        inst: ctx.state.place.instance_id(),
        in_instance: ctx.state.place.in_instance(),
        me_char: ctx.state.me.char_id,
        dead: ctx.state.me.dead,
        hostile: hostile(ctx),
        laden: laden(ctx),
        open_blocked: blocked_by_buff(ctx, OPEN_SKILL),
        recall_blocked: blocked_by_buff(ctx, RECALL_SKILL),
        gm: ctx.gm_allowed(),
        book: ctx
            .state
            .find_item(TELEPORT_BOOK_ITEM)
            .map(|(_, item)| item.id),
        stones: stones(ctx),
        open_ready: ctx.state.cooldown_ready(OPEN_SKILL, now) && now >= inner.open_ready_ms,
        skill: inner.skill,
        gates: &gates,
        used: inner.used,
        refusal: newest_refusal(ctx),
        route,
        spot,
    };
    let mut acts = Acts::new();
    let st = j.step(&snap, &mut acts);
    perform(ctx, inner, &acts);
    settle(ctx, inner, j, st);
    st
}

/// Pauses the portal use the failed journey `why` shows to be pointless for a while.
fn back_off(inner: &mut Inner, why: JourneyFail, recall: bool, now: u64) {
    match why {
        JourneyFail::OpenRefused(_)
        | JourneyFail::OpenError(_)
        | JourneyFail::NoGate
        | JourneyFail::UseRefused(_)
        | JourneyFail::NoTeleport
        | JourneyFail::NoReply
            if !recall =>
        {
            inner.open_off_until_ms = now + OPEN_OFF_MS;
        }
        JourneyFail::RecallRefused(_) | JourneyFail::NoTeleport if recall => {
            inner.recall_off_until_ms = now + RECALL_OFF_MS;
        }
        // The buff is the bot's, not the destination's: asking again for the next destination
        // (the quest goal picks a new one every few seconds) fails the same way, once per ask.
        JourneyFail::Blocked(_) => {
            inner.open_off_until_ms = inner.open_off_until_ms.max(now + BLOCKED_OFF_MS);
            inner.recall_off_until_ms = inner.recall_off_until_ms.max(now + BLOCKED_OFF_MS);
        }
        _ => {}
    }
}

/// What the finished journey changes for the next ones, and the log line.
fn settle(ctx: &mut Ctx<'_>, inner: &mut Inner, j: &Journey, st: JourneyStatus) {
    let now = ctx.now_ms;
    match st {
        JourneyStatus::Running => {}
        JourneyStatus::Arrived => {
            Counters::bump(&STATS.arrivals);
            let how = if j.is_recall() { "recall" } else { "gate" };
            ctx.log
                .say(format!("travel: came through a {how} to {}", spot(ctx)));
            tracing::info!(bot = %ctx.persona.name, how, "portals: arrived");
        }
        JourneyStatus::Failed(why) => {
            if !matches!(why, JourneyFail::NoRoute | JourneyFail::Laden) {
                Counters::bump(&STATS.failures);
                ctx.log
                    .say(format!("travel: portal trip failed ({})", why.word()));
                tracing::info!(bot = %ctx.persona.name, why = why.word(), ?why, "portals: journey failed");
            }
            back_off(inner, why, j.is_recall(), now);
            if let JourneyFail::UseRefused(code) = why {
                STATS.refusal(code);
            }
            if let JourneyFail::OpenError(code) = why {
                STATS.refusal(code);
            }
        }
    }
}

fn spot(ctx: &Ctx<'_>) -> String {
    let p = ctx.state.me.pos;
    ctx.world
        .zone_key_at(p[0], p[1])
        .and_then(|k| ctx.world.zone_name(k))
        .map_or_else(|| format!("{:.0}, {:.0}", p[0], p[1]), str::to_owned)
}

/// Carries out the acts of a step.
fn perform(ctx: &mut Ctx<'_>, inner: &mut Inner, acts: &Acts) {
    let now = ctx.now_ms;
    let me = ctx.state.me.obj;
    for act in acts {
        match *act {
            Act::Stop => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Act::Walk { x, y, radius } => {
                let heading_there = ctx.motion.is_moving()
                    && ctx
                        .motion
                        .destination()
                        .is_some_and(|d| dist([d.0, d.1], [x, y]) < 1.0);
                if !heading_there {
                    ctx.motion.go_to(x, y, radius, Pace::Run, now);
                }
            }
            Act::TopUp { item, n } => {
                if ctx.gm(&format!("/item add self {item} {n}")) {
                    ctx.log.say(format!("travel: GM top-up {n} x item {item}"));
                }
            }
            Act::Open {
                book,
                kind,
                entry,
                at,
            } => {
                ctx.send(cs::open_portal(
                    me,
                    book,
                    kind,
                    i32::try_from(entry).unwrap_or(0),
                    (at[0], at[1], at[2]),
                ));
                let kit = ctx.world.travel().portals.kit();
                inner.open_ready_ms = now + u64::from(kit.open.cast_ms + kit.open.cooldown_ms);
                Counters::bump(&STATS.opens_sent);
                ctx.log.say(format!(
                    "travel: opening a gate to book entry {entry} ({})",
                    if kind == cs::PORTAL_KIND_DISTRICT {
                        "district"
                    } else {
                        "private"
                    }
                ));
                tracing::info!(bot = %ctx.persona.name, entry, kind, "portals: gate opened");
            }
            Act::Use { gate } => {
                if let Some(pin) = ctx.motion.pin(now, false) {
                    ctx.send(pin);
                }
                ctx.send(cs::use_portal(gate, false));
                Counters::bump(&STATS.uses_sent);
                tracing::info!(bot = %ctx.persona.name, gate, "portals: gate used");
            }
            Act::Recall => {
                ctx.send(cs::start_skill_unit(RECALL_SKILL, me, me));
                let kit = ctx.world.travel().portals.kit();
                inner.recall_ready_ms = now + u64::from(kit.recall.cooldown_ms);
                Counters::bump(&STATS.recalls_sent);
                ctx.log.say("travel: recalling to the bound point");
                tracing::info!(bot = %ctx.persona.name, "portals: recall");
            }
            Act::Fault(f) => report_fault(ctx, f),
        }
    }
}

fn hostile(ctx: &Ctx<'_>) -> bool {
    ctx.combat.busy() || !ctx.state.threats.is_empty()
}

/// Whether the bot carries a trade pack (worn in the back slot). The server refuses every
/// `CSUsePortal` of such a bot with `SCErrorMsg` 610, so it neither opens nor enters a gate.
fn laden(ctx: &Ctx<'_>) -> bool {
    ctx.state
        .equipment
        .iter()
        .any(|e| !ctx.world.specialty_buyers(e.template).is_empty())
}

/// How many gate stones the bag holds (every kind the open skill takes).
fn stones(ctx: &Ctx<'_>) -> i64 {
    ctx.state.item_count(BOUND_STONE_ITEM) + ctx.state.item_count(STONE_ITEM)
}

/// The newest `SCErrorMsg` of the portal code the bot heard.
fn newest_refusal(ctx: &Ctx<'_>) -> Option<(i16, u64)> {
    ctx.state.errors.iter().rev().find_map(|(at, e)| match e {
        StateError::Msg(c) if REFUSALS.contains(c) => Some((*c, *at)),
        _ => None,
    })
}

/// The gates of the bot's memory that are still in the unit view.
fn live_gates(ctx: &Ctx<'_>, inner: &Inner) -> SmallVec<[GateView; 6]> {
    inner
        .gates
        .iter()
        .filter(|g| ctx.state.units.contains_key(&g.obj))
        .copied()
        .collect()
}

/// Learns zone cells (from a fresh record and from the districts) and gives the private entries
/// their world positions.
fn refresh(ctx: &Ctx<'_>, inner: &mut Inner) {
    if !inner.book.is_dirty() && inner.recorded.is_none() {
        return;
    }
    let portals = &ctx.world.travel().portals;
    let recall_at = |id: u32| portals.recall(id).map(|r| (r.zone_key, [r.pos.x, r.pos.y]));
    let dir = ctx.cfg.bots.roster.parent();
    let recorded = inner.recorded.take();
    if let Some(r) = recorded {
        // (the entry arrived: the next record is a while away)
        inner.next_record_ms = ctx.now_ms + RECORD_GAP_MS;
        ctx.log.say(format!("travel: recorded book entry {}", r.id));
        tracing::info!(bot = %ctx.persona.name, entry = r.id, "portals: recorded");
    }
    let book = &mut inner.book;
    with_origins(dir, |origins| {
        if let Some(r) = recorded {
            if let Some(e) = book.private_by_id(r.id).cloned() {
                if e.zone != 0 {
                    let learned = origins.learn_from(
                        e.zone,
                        (r.at[0], r.at[1]),
                        (e.off[0], e.off[1]),
                        STOOD_TOLERANCE_M,
                    );
                    if learned.is_none() {
                        tracing::warn!(
                            entry = r.id,
                            zone = e.zone,
                            "portals: a recorded entry does not fit the zone cell grid"
                        );
                    }
                }
                book.set_world(r.id, [r.at[0], r.at[1]]);
            }
        }
        book.resolve(origins, recall_at);
    });
}

/// The route for a trip to `dest`.
fn plan_route(ctx: &Ctx<'_>, inner: &Inner, dest: [f32; 2]) -> Route {
    let gate_ok = blocked_by_buff(ctx, OPEN_SKILL).is_none();
    let recall_ok = blocked_by_buff(ctx, RECALL_SKILL).is_none();
    // a carried pack blocks every gate (610); a buff that blocks the skill makes the trip walk
    // from the start instead of failing at a gate (the "blocked by a buff" log line)
    if !plan::portal_allowed(laden(ctx), gate_ok, recall_ok) {
        return Route::Walk;
    }
    let now = ctx.now_ms;
    let me = [ctx.state.me.pos[0], ctx.state.me.pos[1]];
    let world = ctx.world;
    let portals = &world.travel().portals;
    let recall_at = |id: u32| portals.recall(id).map(|r| (r.zone_key, [r.pos.x, r.pos.y]));
    let me_zone = world.zone_key_at(me[0], me[1]).unwrap_or(0);
    let can_open = now >= inner.open_off_until_ms
        && (ctx.state.find_item(TELEPORT_BOOK_ITEM).is_some() || ctx.gm_allowed());
    let known: SmallVec<[book::Target; 16]> = inner.book.targets(recall_at).collect();
    let mut near: SmallVec<[(f32, book::Target); 8]> = known
        .iter()
        .map(|t| (dist(t.at, dest), *t))
        .filter(|(d, _)| *d <= NEAR_ENTRY_M)
        .collect();
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    near.truncate(6);
    let cands: SmallVec<[Cand; 6]> = near
        .iter()
        .map(|(_, t)| {
            let rows = portals.reagents(portals.same_continent(me_zone, t.zone));
            let afford = rows
                .iter()
                .any(|r| ctx.state.item_count(r.item) >= i64::from(r.amount));
            let stone = rows
                .iter()
                .find(|r| r.item == STONE_ITEM)
                .or(rows.last())
                .map_or((STONE_ITEM, 1 + STONE_SPARE), |r| {
                    (r.item, r.amount + STONE_SPARE)
                });
            Cand {
                target: *t,
                afford,
                topup: stone,
            }
        })
        .collect();
    let bound = inner.book.bound;
    let recall = (bound != 0
        && now >= inner.recall_ready_ms
        && now >= inner.recall_off_until_ms
        && ctx.state.cooldown_ready(RECALL_SKILL, now))
    .then(|| portals.recall(bound))
    .flatten()
    .map(|r| [r.pos.x, r.pos.y]);
    let route = plan::choose(&PlanIn {
        me,
        dest,
        cands: &cands,
        recall_at: recall,
        can_open,
        can_topup: ctx.gm_allowed(),
        gate_ok,
        recall_ok,
    });
    tracing::debug!(
        bot = %ctx.persona.name,
        entries = inner.book.private.len(),
        districts = inner.book.districts.len(),
        resolved = known.len(),
        near = cands.len(),
        can_open,
        route = ?route,
        "portals: route"
    );
    route
}

/// A free spot 1.8 m from the bot, on land and at about its height, to open the gate on.
fn gate_spot(ctx: &Ctx<'_>) -> Option<[f32; 3]> {
    let p = ctx.state.me.pos;
    let yaw = ctx.state.me.yaw;
    for k in 0..8 {
        let a = yaw + k as f32 * std::f32::consts::FRAC_PI_4;
        // (0 = +Y, counter-clockwise)
        let (x, y) = (p[0] - a.sin() * 1.8, p[1] + a.cos() * 1.8);
        if !crate::bot::goals::walkable(ctx.world, x, y) {
            continue;
        }
        let z = match ctx.world.ground_z(x, y) {
            Some(g) if (g - p[2]).abs() <= 1.2 => g,
            _ => p[2],
        };
        return Some([x, y, z]);
    }
    None
}

// ---- symptoms -------------------------------------------------------------------------------------

const AREA: &str = "rust/crates/aaemu-game/src/systems/npcs/portal/";

/// The report of a symptom of the portal code.
fn fault_report(f: Fault) -> IssueReport {
    let base = |key: String, title: &str| IssueReport::new(IssueKind::Other, key, title).area(AREA);
    match f {
        Fault::OpenNoGate { accepted, consumed } => base(
            format!("portal:open_no_gate consumed={consumed}"),
            "A Teleport Book cast opened no portal gate",
        )
        .expected(
            "the open-gate cast (skill 11216, 10 s) summons the entrance gate NPC 3891 within 5 m of the caster",
        )
        .actual(match (accepted, consumed) {
            (_, true) => "the cast ran, the stones left the bag and no gate NPC appeared".to_string(),
            (true, false) => "the cast was accepted, no stones were taken and no gate NPC appeared".to_string(),
            (false, false) => "no answer to the cast, no stones taken, no gate NPC".to_string(),
        })
        .suspect(if consumed { Suspect::Server } else { Suspect::Unknown })
        .severity(if consumed { Severity::High } else { Severity::Medium })
        .fact("accepted", accepted)
        .fact("stones_consumed", consumed),
        Fault::OpenRefused(code) => base(
            format!("portal:open_refused code={code}"),
            "The server refused to start a Teleport Book cast",
        )
        .expected("a ready Teleport Book with stones in the bag starts the open-gate cast")
        .actual(format!("SCSkillStarted with SkillResult {code}"))
        .suspect(match crate::bot::watch::classify::refusal(code) {
            crate::bot::watch::classify::Refusal::Bot => Suspect::Bot,
            _ => Suspect::Unknown,
        })
        .severity(Severity::Medium)
        .fact("skill_result", code),
        Fault::UseRefused(code) => base(
            format!("portal:use_refused code={code}"),
            "The server refused CSUsePortal for the bot's own gate",
        )
        .expected("a bot next to its own open gate, without a trade pack, goes through")
        .actual(format!("SCErrorMsg {code}"))
        .suspect(match code {
            154 | 610 | 611 => Suspect::Bot,
            287 => Suspect::Data,
            _ => Suspect::Unknown,
        })
        .severity(Severity::Medium)
        .fact("error", code),
        Fault::UseNoTeleport => base(
            "portal:use_no_teleport".to_string(),
            "A portal gate was used and the bot was not moved",
        )
        .expected("SCUnitPortalUsed is followed by SCTeleportUnit (or SCLoadInstance)")
        .actual("SCUnitPortalUsed arrived, no teleport followed within 5 s")
        .suspect(Suspect::Server)
        .severity(Severity::High),
        Fault::UseNoReply => base(
            "portal:use_no_reply".to_string(),
            "CSUsePortal got no answer",
        )
        .expected("the server teleports the bot or answers with an error message")
        .actual("nothing came within 5 s")
        .suspect(Suspect::Unknown)
        .severity(Severity::Medium),
        Fault::RecallRefused(code) => base(
            format!("portal:recall_refused code={code}"),
            "The server refused to start the recall skill",
        )
        .expected("a bot with a bound recall point and a ready recall starts the cast")
        .actual(format!("SCSkillStarted with SkillResult {code}"))
        .suspect(match crate::bot::watch::classify::refusal(code) {
            crate::bot::watch::classify::Refusal::Bot => Suspect::Bot,
            _ => Suspect::Unknown,
        })
        .severity(Severity::Low)
        .fact("skill_result", code),
        Fault::RecallNoTeleport => base(
            "portal:recall_no_teleport".to_string(),
            "The recall cast ended and the bot was not moved",
        )
        .expected("recall (skill 10594) teleports the bot to its bound recall point after the 10 s cast")
        .actual("no teleport within 18 s of the cast")
        .suspect(Suspect::Unknown)
        .severity(Severity::Medium),
    }
}

/// The ids of the bot's buffs, comma separated, for issue facts.
fn buff_list(ctx: &Ctx<'_>) -> String {
    ctx.state
        .buffs
        .iter()
        .map(|b| b.1.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn report_fault(ctx: &mut Ctx<'_>, f: Fault) {
    Counters::bump(&STATS.faults);
    tracing::info!(bot = %ctx.persona.name, fault = ?f, "portals: symptom");
    let mut report = fault_report(f);
    let p = ctx.state.me.pos;
    report = report.fact("pos", format!("{:.0}, {:.0}, {:.0}", p[0], p[1], p[2]));
    if matches!(f, Fault::OpenRefused(_) | Fault::RecallRefused(_)) {
        // the buffs say which `skill_reqs` requirement refused the skill
        report = report.fact("buffs", buff_list(ctx));
    }
    ctx.report_issue(report);
}

// ---- the hook of Ctx::travel ----------------------------------------------------------------------------

/// The hook of `Ctx::travel`: gives a trip to (x, y) of more than [`FAR_TRIP_M`] metres to an
/// implicit journey when a portal route beats walking. Returns whether the journey took the trip
/// over (the caller then does not walk); on arrival, failure and for every short trip it returns
/// `false` and the caller walks as before.
///
/// Calling it again for the same destination continues the journey; goals that re-issue their
/// trip every few seconds need no change. A destination whose portal route failed is walked for
/// [`BLOCK_MS`].
pub fn far_trip(ctx: &mut Ctx<'_>, x: f32, y: f32) -> bool {
    if !ctx.cfg.behavior.portals || ctx.state.me.dead || !ctx.state.me.spawned {
        return false;
    }
    let p = ctx.state.me.pos;
    let far = dist([p[0], p[1]], [x, y]);
    // a GM `/move` (bots.long_travel_teleport_m) is faster than any gate
    if far <= FAR_TRIP_M || ctx.would_teleport(far) {
        return false;
    }
    // a bot that never heard of a book or a recall point has nothing to open a gate to
    let Some(mut inner) = ctx.state.travel.take(false) else {
        return false;
    };
    let took = far_trip_with(ctx, &mut inner, x, y);
    tracing::debug!(bot = %ctx.persona.name, x, y, far, took, "portals: far trip asked");
    ctx.state.travel.put(inner);
    took
}

/// Whether a trip to (x, y) would go by portal: it is far, the destination is not blocked and the
/// planner finds a gate or a recall that beats walking (or a journey to it is under way). Creates
/// nothing; for goals that split their walks into legs and would rather ask for a far trip in one
/// piece.
pub fn has_route(ctx: &mut Ctx<'_>, x: f32, y: f32) -> bool {
    if !ctx.cfg.behavior.portals || ctx.state.me.dead || !ctx.state.me.spawned {
        return false;
    }
    let p = ctx.state.me.pos;
    let far = dist([p[0], p[1]], [x, y]);
    // a GM `/move` (bots.long_travel_teleport_m) is faster than any gate
    if far <= FAR_TRIP_M || ctx.would_teleport(far) {
        return false;
    }
    let Some(mut inner) = ctx.state.travel.take(false) else {
        return false;
    };
    let now = ctx.now_ms;
    let cell = cell_of(x, y);
    let running = inner.auto.as_ref().is_some_and(|j| j.goes_to(x, y, 40.0));
    let blocked = inner
        .blocked
        .iter()
        .any(|b| b.cell == cell && b.until_ms > now);
    let found = running
        || (!blocked && {
            refresh(ctx, &mut inner);
            !matches!(plan_route(ctx, &inner, [x, y]), Route::Walk)
        });
    ctx.state.travel.put(inner);
    found
}

fn far_trip_with(ctx: &mut Ctx<'_>, inner: &mut Inner, x: f32, y: f32) -> bool {
    // the server refuses a gate to a bot with a trade pack, and a buff can block the skill: walk
    // (or /move) like before, without starting a journey that can only fail
    if !plan::portal_allowed(
        laden(ctx),
        blocked_by_buff(ctx, OPEN_SKILL).is_none(),
        blocked_by_buff(ctx, RECALL_SKILL).is_none(),
    ) {
        return false;
    }
    let now = ctx.now_ms;
    inner.blocked.retain(|b| b.until_ms > now);
    let cell = cell_of(x, y);
    if inner.blocked.iter().any(|b| b.cell == cell) {
        return false;
    }
    match &inner.auto {
        Some(j) if j.goes_to(x, y, 40.0) => {}
        // a hop through somebody's gate: the bot is not going anywhere on its own
        Some(j) if matches!(j.dest(), Dest::Gate { .. }) => return false,
        _ => inner.auto = Some(Journey::new([x, y])),
    }
    let status = step_auto(ctx, inner);
    match status {
        JourneyStatus::Running => true,
        JourneyStatus::Arrived => false,
        JourneyStatus::Failed(why) => {
            let ms = if matches!(why, JourneyFail::NoRoute | JourneyFail::Laden) {
                NO_ROUTE_MS
            } else {
                BLOCK_MS
            };
            inner.blocked.push(Block {
                cell,
                until_ms: now + ms,
            });
            if inner.blocked.len() > 4 {
                inner.blocked.remove(0);
            }
            false
        }
    }
}

fn cell_of(x: f32, y: f32) -> (i32, i32) {
    ((x / 64.0).floor() as i32, (y / 64.0).floor() as i32)
}

/// Steps the implicit journey once per tick; a journey that finished is dropped.
fn step_auto(ctx: &mut Ctx<'_>, inner: &mut Inner) -> JourneyStatus {
    let now = ctx.now_ms;
    let Some(mut j) = inner.auto.take() else {
        return JourneyStatus::Failed(JourneyFail::NoRoute);
    };
    if inner.auto_step_ms == now && j.holds() {
        inner.auto = Some(j);
        return JourneyStatus::Running;
    }
    inner.auto_step_ms = now;
    let st = run(&mut j, ctx, inner);
    if st == JourneyStatus::Running {
        inner.auto = Some(j);
    }
    st
}

/// The goal that asked for the implicit journey ended: drop it.
pub fn on_goal_end(ctx: &mut Ctx<'_>) {
    let Some(mut inner) = ctx.state.travel.take(false) else {
        return;
    };
    if let Some(mut j) = inner.auto.take() {
        j.stop(ctx);
    }
    ctx.state.travel.put(inner);
}

// ---- the duty ---------------------------------------------------------------------------------------------

/// The brain's per-tick portal duty (see the module docs). Returns whether the bot's goal must
/// wait this tick: a journey is casting or at a gate, or a record is under way.
pub fn duty(ctx: &mut Ctx<'_>, goal: Option<GoalKind>) -> bool {
    if !ctx.cfg.behavior.portals || ctx.state.me.dead || !ctx.state.me.spawned {
        return false;
    }
    let Some(mut inner) = ctx.state.travel.take(true) else {
        return false;
    };
    let hold = duty_with(ctx, &mut inner, goal);
    ctx.state.travel.put(inner);
    hold
}

fn duty_with(ctx: &mut Ctx<'_>, inner: &mut Inner, goal: Option<GoalKind>) -> bool {
    let now = ctx.now_ms;
    if !inner.started {
        inner.started = true;
        inner.next_record_ms = now + FIRST_RECORD_MS + ctx.rng.u64(0..FIRST_RECORD_MS);
        inner.next_hop_ms = now + 2 * 60_000 + ctx.rng.u64(0..6 * 60_000);
    }
    refresh(ctx, inner);
    forget_old_gates(ctx, inner);
    visit_step(ctx, inner);

    let mut hold = false;
    if inner.auto.is_some() {
        let stale = inner
            .auto
            .as_ref()
            .is_some_and(|j| !j.holds() && now.saturating_sub(inner.auto_step_ms) > STALE_MS);
        if stale {
            inner.auto = None;
        } else {
            step_auto(ctx, inner);
            hold = inner.auto.as_ref().is_some_and(Journey::holds);
        }
    }
    if hold {
        return true;
    }
    if record_step(ctx, inner, goal) {
        return true;
    }
    hop_step(ctx, inner, goal);
    inner.auto.as_ref().is_some_and(Journey::holds)
}

fn forget_old_gates(ctx: &Ctx<'_>, inner: &mut Inner) {
    let now = ctx.now_ms;
    inner.gates.retain(|g| {
        ctx.state.units.contains_key(&g.obj) || now.saturating_sub(g.seen_ms) < GATE_FORGET_MS
    });
}

/// A recall point this close counts as visited: the real client reports every sub zone it enters
/// (`CSNotifySubZone`) and the server adds that sub zone's recall points to the book as districts,
/// which cost no ink to open a gate to.
const VISIT_M: f32 = 120.0;
/// How often the bot looks for a recall district to visit, ms.
const VISIT_EVERY_MS: u64 = 5_000;

/// Tells the server the bot entered the sub zone of a recall point it stands near (what the 1.2
/// client does by itself), once per sub zone.
fn visit_step(ctx: &mut Ctx<'_>, inner: &mut Inner) {
    let now = ctx.now_ms;
    if now < inner.next_visit_ms || ctx.state.place.in_instance() || ctx.state.place.loading() {
        return;
    }
    inner.next_visit_ms = now + VISIT_EVERY_MS;
    let p = ctx.state.me.pos;
    let here = aa_world::Vec3::new(p[0], p[1], p[2]);
    let book = &inner.book;
    let found = ctx
        .world
        .travel()
        .portals
        .recalls_within(here, VISIT_M)
        .find(|r| {
            r.sub_zone != 0
                && !inner.notified.contains(&r.sub_zone)
                && !book.districts.iter().any(|d| d.id == r.id)
        })
        .map(|r| (r.sub_zone, r.id));
    let Some((sub_zone, id)) = found else { return };
    if inner.notified.len() >= 6 {
        inner.notified.remove(0);
    }
    inner.notified.push(sub_zone);
    ctx.send(cs::notify_sub_zone(sub_zone));
    ctx.log.say(format!(
        "travel: entered sub zone {sub_zone} (recall point {id})"
    ));
    tracing::debug!(bot = %ctx.persona.name, sub_zone, "portals: sub zone entered");
}

/// Whether the bot is calm enough to stand still for a few seconds on a duty of its own.
fn calm(ctx: &Ctx<'_>, goal: Option<GoalKind>) -> bool {
    let free_goal = !matches!(
        goal,
        Some(
            GoalKind::Duel
                | GoalKind::Trade
                | GoalKind::TradeRun
                | GoalKind::Dungeon
                | GoalKind::Sail
                | GoalKind::Auction
                | GoalKind::Market
                | GoalKind::Guild
                | GoalKind::Gank
                | GoalKind::Warzone
                | GoalKind::Event
                | GoalKind::Companion
                | GoalKind::Farm
                | GoalKind::Gear
        )
    );
    let st = &ctx.state;
    // a trade pack's buff forbids the record skill and the server refuses every gate
    free_goal
        && !laden(ctx)
        && !hostile(ctx)
        && st.me.hp_pct() >= CALM_HP_PCT
        && !ctx.motion.busy()
        && !st.trade.active()
        && !st.place.in_instance()
        && !st.place.loading()
        && ctx
            .world
            .ground_z(st.me.pos[0], st.me.pos[1])
            .is_none_or(|g| g >= SWIM_THRESHOLD + 1.0)
}

/// The buff on the bot that makes the server refuse `skill` (`skill_reqs`), if any: a stun, the
/// landing after a fall, a trade pack, and for the record skill the peace-zone aura around a Nui
/// statue (the skill is not on the lists of skills the aura lets through).
fn blocked_by_buff(ctx: &Ctx<'_>, skill: u32) -> Option<u32> {
    ctx.world
        .travel()
        .portals
        .blocked_by(skill, ctx.state.buffs.iter().map(|b| b.1))
}

/// The recording duty. Returns whether it holds the goal this tick.
fn record_step(ctx: &mut Ctx<'_>, inner: &mut Inner, goal: Option<GoalKind>) -> bool {
    let now = ctx.now_ms;
    match inner.rec.clone() {
        Rec::Cast {
            sent_ms,
            ink,
            at,
            zone: _,
        } => {
            let refused = inner
                .skill
                .filter(|n| n.skill == RECORD_PORTAL_SKILL && n.at_ms >= sent_ms && n.result != 0);
            if let Some(n) = refused {
                finish_record(ctx, inner, "refused");
                let report = IssueReport::new(
                    IssueKind::Other,
                    format!("portal:record_refused code={}", n.result),
                    "The server refused to start the Teleport Book record cast",
                )
                .area(AREA)
                .expected("a bot with a Memory Ink and a free book id starts the record cast (skill 11215)")
                .actual(format!("SCSkillStarted with SkillResult {}", n.result))
                .suspect(match crate::bot::watch::classify::refusal(n.result) {
                    crate::bot::watch::classify::Refusal::Bot => Suspect::Bot,
                    _ => Suspect::Unknown,
                })
                .fact("skill_result", n.result)
                .fact("buffs", buff_list(ctx));
                if n.result == SKILL_REQ_FAIL {
                    // a buff of the place (a peace zone at a Nui statue) lets only listed skills
                    // through: leave the place alone for a while
                    let p = ctx.state.me.pos;
                    inner.no_record.retain(|r| r.2 > now);
                    if inner.no_record.len() >= 4 {
                        inner.no_record.remove(0);
                    }
                    inner.no_record.push((p[0], p[1], now + NO_RECORD_MS));
                }
                ctx.report_issue(report);
                return false;
            }
            if hostile(ctx) {
                // damage breaks the cast; that is not the server's doing
                finish_record(ctx, inner, "interrupted");
                return false;
            }
            if now.saturating_sub(sent_ms) > RECORD_WAIT_MS {
                let consumed = ctx.state.item_count(MEMORY_INK_ITEM) < ink;
                let accepted = inner
                    .skill
                    .is_some_and(|n| n.skill == RECORD_PORTAL_SKILL && n.at_ms >= sent_ms);
                finish_record(ctx, inner, "no entry");
                Counters::bump(&STATS.faults);
                let report = IssueReport::new(
                    IssueKind::Other,
                    format!("portal:record_no_entry consumed={consumed}"),
                    "A Teleport Book record cast added no entry",
                )
                .area(AREA)
                .expected("the record cast (skill 11215, 5 s) adds a book entry (SCCharacterPortals) and takes one Memory Ink")
                .actual(if consumed {
                    "the Memory Ink left the bag and no entry arrived"
                } else {
                    "no entry arrived and no ink was taken"
                })
                .suspect(if consumed { Suspect::Server } else { Suspect::Unknown })
                .severity(if consumed { Severity::High } else { Severity::Medium })
                .fact("accepted", accepted)
                .fact("ink_consumed", consumed)
                .fact("pos", format!("{:.0}, {:.0}, {:.0}", at[0], at[1], at[2]));
                ctx.report_issue(report);
                return false;
            }
            return true;
        }
        Rec::Delete { sent_ms, .. } => {
            if now.saturating_sub(sent_ms) > DELETE_WAIT_MS {
                inner.rec = Rec::Idle;
                inner.next_record_ms = now + RECORD_RETRY_MS;
                return false;
            }
            return true;
        }
        Rec::Idle => {}
    }
    let pos = ctx.state.me.pos;
    let calm = calm(ctx, goal) && blocked_by_buff(ctx, RECORD_PORTAL_SKILL).is_none();
    let input = places::RecordIn {
        now_ms: now,
        next_record_ms: inner.next_record_ms,
        next_probe_ms: inner.next_probe_ms,
        busy: inner.auto.is_some(),
        calm,
        nearest_entry_m: inner.book.nearest_private([pos[0], pos[1]]).map(|(d, _)| d),
        ink: ctx.state.item_count(MEMORY_INK_ITEM),
        gm: ctx.gm_allowed(),
        entries: inner.book.private.len(),
    };
    if now >= inner.next_record_ms && now >= inner.next_probe_ms && input.calm {
        inner.next_probe_ms = now + 4_000;
    }
    let homes: SmallVec<[[f32; 3]; 2]> = ctx.state.houses.iter().map(|h| h.pos).collect();
    let mut found = None;
    let refused_here = places::refused_near(&inner.no_record, now, [pos[0], pos[1]], NO_RECORD_M);
    let decision = places::decide(&input, || {
        found = if refused_here {
            None
        } else {
            places::worth(ctx.world, pos, &homes)
        };
        found
    });
    let Some(spot) = found else {
        return false;
    };
    match decision {
        places::Decision::Wait => {
            if input
                .nearest_entry_m
                .is_some_and(|d| d < places::ENTRY_NEAR_M)
            {
                inner.next_probe_ms = now + 30_000;
            }
            return false;
        }
        places::Decision::TopUpInk => {
            if ctx.gm(&format!("/item add self {MEMORY_INK_ITEM} {INK_TOPUP}")) {
                ctx.log.say("travel: GM top-up of Memory Ink");
                inner.next_probe_ms = now + 3_000;
            } else {
                inner.next_record_ms = now + RECORD_GAP_MS;
            }
            return false;
        }
        places::Decision::NoInk => {
            inner.next_record_ms = now + RECORD_GAP_MS;
            return false;
        }
        places::Decision::MakeRoom => {
            if let Some(id) = inner.book.least_useful() {
                ctx.send(cs::delete_portal(cs::PORTAL_KIND_PRIVATE, id));
                inner.rec = Rec::Delete { id, sent_ms: now };
                Counters::bump(&STATS.deletes);
                ctx.log
                    .say(format!("travel: the book is full; deleting entry {id}"));
                return true;
            }
            inner.next_record_ms = now + RECORD_GAP_MS;
            return false;
        }
        places::Decision::Record => {}
    }
    let ink = input.ink;
    let name = places::name_for(
        ctx.world,
        pos,
        spot,
        inner.book.private.iter().map(|p| &*p.name),
    );
    ctx.motion.stop();
    ctx.send(cs::record_portal(ctx.state.me.obj, &name));
    inner.rec = Rec::Cast {
        sent_ms: now,
        at: pos,
        zone: ctx.world.zone_key_at(pos[0], pos[1]).unwrap_or_default(),
        ink,
    };
    Counters::bump(&STATS.records_sent);
    ctx.log
        .say(format!("travel: recording {name:?} ({})", spot.word()));
    tracing::info!(bot = %ctx.persona.name, name = %name, spot = spot.word(), "portals: record cast");
    true
}

fn finish_record(ctx: &mut Ctx<'_>, inner: &mut Inner, why: &str) {
    inner.rec = Rec::Idle;
    inner.next_record_ms = ctx.now_ms + RECORD_RETRY_MS;
    Counters::bump(&STATS.records_failed);
    ctx.log.say(format!("travel: the record failed ({why})"));
    tracing::info!(bot = %ctx.persona.name, why, "portals: record failed");
}

/// The idle bot that sees another bot's gate nearby may step through it (now and then).
fn hop_step(ctx: &mut Ctx<'_>, inner: &mut Inner, goal: Option<GoalKind>) {
    let now = ctx.now_ms;
    if inner.auto.is_some()
        || inner.rec != Rec::Idle
        || now < inner.next_hop_ms
        || !matches!(goal, None | Some(GoalKind::Wander | GoalKind::Rest))
        || !calm(ctx, goal)
    {
        return;
    }
    let me = ctx.state.me.char_id;
    let pos = ctx.state.me.pos;
    let hub = ctx.hub;
    let Some(g) = inner.gates.iter().find(|g| {
        g.owner != me
            && g.obj != inner.rolled_gate
            && ctx.state.units.contains_key(&g.obj)
            && now.saturating_sub(g.seen_ms) <= HOP_MAX_AGE_MS
            && dist([pos[0], pos[1]], [g.pos[0], g.pos[1]]) <= HOP_RANGE_M
            && hub.is_bot_char(g.owner)
    }) else {
        return;
    };
    let (obj, owner) = (g.obj, g.owner);
    inner.rolled_gate = obj;
    if ctx.rng.f32() >= HOP_CHANCE {
        return;
    }
    inner.auto = Some(Journey::follow_gate(owner));
    inner.auto_step_ms = 0;
    inner.next_hop_ms = now + HOP_GAP_MS;
    Counters::bump(&STATS.hops);
    ctx.log.say("travel: hopping through another bot's gate");
    tracing::info!(bot = %ctx.persona.name, owner, "portals: hop");
    step_auto(ctx, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_count_and_print() {
        let c = Counters::new();
        Counters::bump(&c.records_sent);
        c.record_ok();
        c.refusal(154);
        c.refusal(610);
        c.refusal(610);
        c.refusal(999);
        let s = c.snapshot();
        assert_eq!((s.records_sent, s.records_ok), (1, 1));
        assert_eq!(s.refusals, [1, 0, 0, 2, 0, 1]);
        let line = s.line();
        assert!(
            line.contains("records 1/1 ok") && line.contains("610:2"),
            "{line}"
        );
    }

    #[test]
    fn a_buff_that_blocks_the_portals_pauses_them_for_every_destination() {
        let mut inner = Inner::default();
        back_off(&mut inner, JourneyFail::Blocked(2385), false, 1_000);
        assert_eq!(inner.open_off_until_ms, 1_000 + BLOCKED_OFF_MS);
        assert_eq!(inner.recall_off_until_ms, 1_000 + BLOCKED_OFF_MS);
        // a longer pause that is running already stays
        let mut inner = Inner::default();
        back_off(&mut inner, JourneyFail::NoGate, false, 5_000);
        back_off(&mut inner, JourneyFail::RecallRefused(8), true, 5_000);
        back_off(&mut inner, JourneyFail::Blocked(2385), false, 6_000);
        assert_eq!(inner.open_off_until_ms, 5_000 + OPEN_OFF_MS);
        assert_eq!(inner.recall_off_until_ms, 5_000 + RECALL_OFF_MS);
        // the other failures keep their pauses, and the ones that need none set none
        let mut inner = Inner::default();
        back_off(&mut inner, JourneyFail::Interrupted, false, 1_000);
        back_off(&mut inner, JourneyFail::Timeout, true, 1_000);
        assert_eq!((inner.open_off_until_ms, inner.recall_off_until_ms), (0, 0));
        back_off(&mut inner, JourneyFail::OpenRefused(15), false, 1_000);
        assert_eq!(inner.open_off_until_ms, 1_000 + OPEN_OFF_MS);
        assert_eq!(inner.recall_off_until_ms, 0);
    }

    #[test]
    fn destinations_fall_into_64_metre_cells() {
        assert_eq!(cell_of(0.0, 0.0), (0, 0));
        assert_eq!(cell_of(63.9, 64.0), (0, 1));
        assert_eq!(cell_of(-0.1, -64.1), (-1, -2));
    }

    #[test]
    fn every_fault_has_a_report_with_a_stable_key_and_a_suspect() {
        let faults = [
            Fault::OpenNoGate {
                accepted: true,
                consumed: true,
            },
            Fault::OpenNoGate {
                accepted: false,
                consumed: false,
            },
            Fault::OpenRefused(0x0F),
            Fault::UseRefused(154),
            Fault::UseRefused(287),
            Fault::UseRefused(609),
            Fault::UseNoTeleport,
            Fault::UseNoReply,
            Fault::RecallRefused(0x08),
            Fault::RecallNoTeleport,
        ];
        let mut keys = std::collections::HashSet::new();
        for f in faults {
            let r = fault_report(f);
            assert_eq!(r.kind, IssueKind::Other);
            assert!(r.key.starts_with("portal:"), "{}", r.key);
            assert!(!r.expected.is_empty() && !r.actual.is_empty(), "{f:?}");
            assert!(r.area.is_some());
            keys.insert(r.key);
        }
        assert_eq!(keys.len(), faults.len(), "keys tell the symptoms apart");
        // the bot's own mistakes stay local
        assert_eq!(fault_report(Fault::UseRefused(154)).suspect, Suspect::Bot);
        assert_eq!(fault_report(Fault::UseRefused(610)).suspect, Suspect::Bot);
        assert_eq!(fault_report(Fault::UseRefused(287)).suspect, Suspect::Data);
        assert_eq!(
            fault_report(Fault::OpenRefused(0x0F)).suspect,
            Suspect::Bot,
            "too far is the bot's"
        );
        // stones taken and no gate: the server's
        assert_eq!(
            fault_report(Fault::OpenNoGate {
                accepted: true,
                consumed: true
            })
            .suspect,
            Suspect::Server
        );
        assert_eq!(fault_report(Fault::UseNoTeleport).suspect, Suspect::Server);
    }

    #[test]
    fn origins_are_shared_and_kept_in_a_file() {
        let dir = crate::testutil::temp_dir("portal-origins");
        // (the table is process-wide: use zones no other test uses)
        with_origins(Some(&dir), |o| {
            assert!(o.learn(60_001, (7, 8)));
        });
        let text = std::fs::read_to_string(dir.join(ORIGINS_FILE)).unwrap();
        assert!(text.contains("60001 7 8"), "{text}");
        let got = with_origins(None, |o| o.get(60_001));
        assert_eq!(got, Some((7, 8)));
    }
}
