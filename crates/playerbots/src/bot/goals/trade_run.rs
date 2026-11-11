//! Trade run: craft a trade pack at a specialty workbench, carry it to a specialty NPC that buys
//! it, sell it and collect the payment mail.
//!
//! # The run
//!
//! ```text
//! Plan ─► Supply ─► ToBench ─► Craft ─► Crafting ─► ToBuyer ─► Sell ─► Selling ─► Payment ─► done
//!          (GM: /item add, /labor)        CSExecuteCraft                CSSellBackpackGoods
//! ```
//!
//! * **Plan** ([`plan`]): the bot picks a route once per run: a workbench spawn within reach that
//!   offers a pack craft, and the nearest spawn of an NPC buying the product, scored by the way
//!   there plus the carry (walking budgets [`WALK_BUDGETS_M`]; with `bots.gm_bootstrap` and
//!   `bots.long_travel_teleport_m` the budget is lifted because long trips become `/move`). A bot
//!   that already carries a pack skips the craft and goes to a buyer.
//! * **Supply**: crafting costs the recipe's materials and its labor, selling costs
//!   [`SELL_LABOR`] more. With `bots.gm_bootstrap` the bot gives itself exactly what is missing
//!   (`/item add self <item> <n>`, `/labor <n>`); without it only routes the bot can already pay
//!   for are planned.
//! * **ToBench / Craft / Crafting**: walk to the workbench, find its object in the entity view by
//!   template, stand within the craft skill's range and send `CSExecuteCraft`. The craft is done
//!   when the pack shows up in the equipment (the server equips it into the backpack slot); an
//!   `SCErrorMsg` 148 or silence ends the attempt, which is retried with backoff.
//! * **ToBuyer / Sell / Selling**: walk (the pack does not slow the bot down in the server's
//!   eyes; the bot just runs) to the NPC, stand within the server's 2.5 m, send
//!   `CSSellBackpackGoods`; the sale is confirmed by the pack leaving the equipment.
//! * **Payment**: the server pays by mail (instantly or after `TradePackMailDelayInMinutes`).
//!   Collecting it is a background duty that outlives the goal: [`collect_payments`] runs on every
//!   brain tick, reads and takes the payment mail whenever it shows up (see [`ledger`]).
//!
//! The state machine talks to the bot through the small [`Env`] trait so it can be tested with
//! synthetic server events; [`Live`] is the implementation around [`Ctx`].

pub mod ledger;
pub mod plan;

use std::fmt::Write as _;

use aa_proto::cs::{self, Out};
use aa_proto::sc_world::EquipSlot;
use aa_world::{Craft, Relation};
use smallvec::SmallVec;

use crate::bot::cmd::{GoalKind, GoalMask};
use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::{dist2d, BotState, UnitKind};

use super::quest::progress;
use super::{FailReason, GoalStatus};

pub use ledger::{spot_key, Ledger, Paid, SkipKind};
pub use plan::{Plan, Query, RouteData};

/// Longest total route (to the bench plus the carry) a bot walks: the planner tries the budgets in
/// turn and takes the first that has a route (~3 km preferred; around the Nuian start the nearest
/// route is 4 km).
pub const WALK_BUDGETS_M: [f32; 4] = [3_000.0, 5_000.0, 8_000.0, 12_000.0];
/// Bench search radius when `/move` makes distances irrelevant.
const TELEPORT_RADIUS_M: f32 = 40_000.0;
/// Labor the server charges for selling a pack (`sell_specialty`: at least 60, spends 60).
pub const SELL_LABOR: i32 = 60;
/// Labor kept above what the run needs.
const LABOR_MARGIN: i32 = 4;
/// Most copper-worth of materials (item count) a GM-supplied craft may ask for.
const MAX_GM_MATERIALS: u32 = 600;
/// `/item add` refuses more than this many items at once.
const GM_ITEM_CHUNK: u32 = 1_000;
/// The whole run must end within this long.
const RUN_TIMEOUT_MS: u64 = 40 * 60_000;
/// How long a bench, buyer or craft that did not work stays out of the plans.
const SKIP_MS: u64 = 30 * 60_000;

/// The bot stops this far from the bench (the craft skill reaches 4 m).
const BENCH_STOP_M: f32 = 1.8;
/// Closer stop after a craft was refused as out of range.
const BENCH_CLOSE_M: f32 = 0.6;
/// The bench is used from within this distance.
const BENCH_REACH_M: f32 = 3.2;
/// The bot stops this far from the buyer (the server accepts 2.5 m).
const NPC_STOP_M: f32 = 1.2;
/// Closer stop after a sale was refused as too far.
const NPC_CLOSE_M: f32 = 0.4;
/// The buyer is sold to from within this distance.
const NPC_REACH_M: f32 = 2.0;
/// A walk counts as arrived this much beyond its stop radius.
const ARRIVE_SLACK_M: f32 = 0.7;
/// The same while standing closer than usual.
const CLOSE_SLACK_M: f32 = 0.15;
/// A spawn and the object seen for it may differ by this much.
const SPAWN_MATCH_M: f32 = 15.0;
/// Walking trips are split into legs of this length (one trip may take 90 s at most).
const LEG_M: f32 = 280.0;
/// Failed trips in a row after which the walk is given up.
const MAX_TRIP_FAILS: u8 = 3;

/// A run is only planned at this much health (hit points percent): a pack is dropped on death,
/// so the bot rests first.
const START_HP_PCT: u8 = 80;
/// A bot that goes back for a dropped pack waits for this much health first (it just died).
const RECOVER_HP_PCT: u8 = 60;
/// A dropped pack is used from this close (metres).
const PACK_CAST_M: f32 = 2.8;
/// The bot walks up to this close to the dropped pack (metres).
const PACK_STOP_M: f32 = 1.5;
/// A use of the pack doodad is repeated after this long (ms).
const PACK_CAST_GAP_MS: u64 = 2_500;
/// Most uses of the pack doodad in one recovery.
const MAX_PACK_CASTS: u8 = 4;
/// The skill that picks a dropped pack up (`DoodadFuncRecoverItem`).
const PACK_RECOVER_SKILL: u32 = 11_361;

/// How long to wait for a bench or NPC object to show up once the bot stands at its spawn.
const FIND_WAIT_MS: u64 = 8_000;
/// How long to wait for GM-given items and labor.
const SUPPLY_WAIT_MS: u64 = 6_000;
/// How long a craft may take (cast time 6 s) before it counts as failed.
const CRAFT_WAIT_MS: u64 = 22_000;
/// How long to wait for the answer to a sale.
const SELL_WAIT_MS: u64 = 8_000;
/// How long the goal waits for the payment before it ends and leaves the mail to the background.
const PAYMENT_WAIT_MS: u64 = 20_000;
/// Craft and sale attempts per run.
const MAX_RETRIES: u8 = 3;

// `ErrorMessageType` ids the sale can fail with (server `economy/error_message.rs`).
const ERR_INVALID: i16 = -1;
const ERR_INVALID_TARGET: i16 = 18;
const ERR_NOT_ENOUGH_LABOR: i16 = 29;
const ERR_TOO_FAR_AWAY: i16 = 154;
const ERR_STORE_NO_GOODS: i16 = 361;
const ERR_CANT_SELL_HERE: i16 = 512;

/// What a name is looked up in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// An item template.
    Item,
    /// An NPC template.
    Npc,
    /// A doodad template.
    Doodad,
}

/// What the state machine needs from the bot. [`Live`] wraps a [`Ctx`]; the tests use a fake.
pub trait Env {
    /// The bot clock in milliseconds.
    fn now_ms(&self) -> u64;
    /// The bot's position.
    fn pos(&self) -> [f32; 3];
    /// What the bot knows.
    fn state(&self) -> &BotState;
    /// What the bot knows, writable (ledger notes).
    fn state_mut(&mut self) -> &mut BotState;
    /// What the movement controller is doing.
    fn motion(&self) -> MotionStatus;
    /// Whether GM commands may be used.
    fn gm_allowed(&self) -> bool;
    /// Whether a trip of `dist` metres would be a GM `/move`.
    fn would_teleport(&self, dist: f32) -> bool;
    /// A recipe.
    fn craft(&self, id: u32) -> Option<&Craft>;
    /// Whether `item` is a trade pack some NPC buys.
    fn is_pack(&self, item: u32) -> bool;
    /// Whether `template` is the doodad a dropped trade pack turns into.
    fn is_pack_doodad(&self, _template: u32) -> bool {
        false
    }
    /// The use skill of a doodad template (0 when unknown).
    fn doodad_use_skill(&self, _template: u32) -> u32 {
        0
    }
    /// The lowest character level any pack craft needs (the craft skill's level requirement;
    /// selling needs none).
    fn min_pack_level(&self) -> u8;
    /// An English name for the log and the web UI.
    fn name(&self, kind: Named, id: u32) -> String;
    /// Plans a fresh run (a pack to craft, a bench, a buyer).
    fn plan_new(&mut self) -> Option<Plan>;
    /// Plans the walk to a buyer for the pack `item` the bot already carries.
    fn plan_carry(&mut self, item: u32) -> Option<Plan>;
    /// Queues a packet.
    fn send(&mut self, out: Out);
    /// Sends a GM command; `false` when GM commands are off.
    fn gm(&mut self, line: &str) -> bool;
    /// Whether a far trip to (x, y) would go by portal (a gate or a recall beats the walk), so
    /// that it is asked for in one piece instead of in walking legs.
    fn portal_trip(&mut self, _x: f32, _y: f32) -> bool {
        false
    }
    /// Starts a trip to (x, y), stopping `radius` metres short (a GM `/move` when long).
    fn travel(&mut self, x: f32, y: f32, radius: f32);
    /// How long (metres) a leg of a walk is: the straight line cannot do much more than
    /// [`LEG_M`] in the 90 s a trip may take, a route planned on the navigation mesh can.
    fn leg_m(&self) -> f32 {
        LEG_M
    }
    /// Stops walking.
    fn stop(&mut self);
    /// Turns towards (x, y).
    fn face(&mut self, x: f32, y: f32);
    /// A notable event for the web UI and the log.
    fn say(&self, text: &str);
}

/// [`Env`] around a tick's [`Ctx`].
pub struct Live<'a, 'c>(pub &'a mut Ctx<'c>);

impl Env for Live<'_, '_> {
    fn now_ms(&self) -> u64 {
        self.0.now_ms
    }

    fn pos(&self) -> [f32; 3] {
        self.0.state.me.pos
    }

    fn state(&self) -> &BotState {
        &*self.0.state
    }

    fn state_mut(&mut self) -> &mut BotState {
        &mut *self.0.state
    }

    fn motion(&self) -> MotionStatus {
        self.0.motion.status()
    }

    fn gm_allowed(&self) -> bool {
        self.0.gm_allowed()
    }

    fn would_teleport(&self, dist: f32) -> bool {
        self.0.would_teleport(dist)
    }

    fn craft(&self, id: u32) -> Option<&Craft> {
        self.0.world.craft(id)
    }

    fn is_pack(&self, item: u32) -> bool {
        !self.0.world.specialty_buyers(item).is_empty()
    }

    fn is_pack_doodad(&self, template: u32) -> bool {
        self.0.world.is_pack_doodad(template)
    }

    fn doodad_use_skill(&self, template: u32) -> u32 {
        self.0.world.doodad_use_skill(template)
    }

    fn min_pack_level(&self) -> u8 {
        self.0.world.min_pack_craft_level()
    }

    fn name(&self, kind: Named, id: u32) -> String {
        let w = self.0.world;
        let found = match kind {
            Named::Item => w.item_name(id).map(str::to_owned),
            Named::Npc => w.npc(id).map(|n| n.name.to_string()),
            Named::Doodad => w.doodad(id).map(|d| d.name.to_string()),
        };
        found.unwrap_or_else(|| format!("#{id}"))
    }

    fn plan_new(&mut self) -> Option<Plan> {
        let ctx = &mut *self.0;
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let gm = ctx.cfg.bots.gm_bootstrap;
        let teleport = gm && ctx.cfg.bots.long_travel_teleport_m > 0.0;
        let labor = i32::from(ctx.state.me.labor);
        let st = &*ctx.state;
        let (level, faction) = (st.me.level, st.me.faction);
        let world = ctx.world;
        let craft_ok = |c: &Craft| {
            if st.econ.skipped(SkipKind::Craft, c.id, now) {
                return false;
            }
            if level < c.req_level && !gm {
                return false;
            }
            if gm {
                // The GM gives the materials: keep the amounts sane (bag space).
                return c.materials.iter().map(|m| m.1).sum::<u32>() <= MAX_GM_MATERIALS;
            }
            labor >= i32::from(c.labor) + SELL_LABOR
                && c.materials
                    .iter()
                    .all(|&(item, n)| st.item_count(item) >= i64::from(n))
        };
        let bench_ok =
            |_tpl: u32, p: [f32; 2]| !st.econ.skipped(SkipKind::Bench, spot_key(p[0], p[1]), now);
        let buyer_ok = |npc: u32| !st.econ.skipped(SkipKind::Buyer, npc, now);
        // Walking: the shortest budget that has a route; `/move` makes distances irrelevant.
        let budgets: &[f32] = if teleport {
            &[f32::INFINITY]
        } else {
            &WALK_BUDGETS_M
        };
        for &budget in budgets {
            let q = Query {
                from: [me[0], me[1]],
                max_route_m: budget,
                radius_m: budget.min(TELEPORT_RADIUS_M),
                craft_ok: &craft_ok,
                bench_ok: &bench_ok,
                buyer_ok: &buyer_ok,
            };
            let cands: Vec<Plan> = plan::candidates(world, &q)
                .into_iter()
                .filter(|p| {
                    // a GM bot is levelled to the craft in Supply: judge the route at the level
                    // the bot will have for the run, not at its current one
                    let run_level = level.max(world.craft(p.craft).map_or(0, |c| c.req_level));
                    !route_risky(world, faction, run_level, [me[0], me[1]], p)
                })
                .collect();
            if let Some(p) = plan::pick(&cands, ctx.rng) {
                return Some(p);
            }
        }
        None
    }

    fn plan_carry(&mut self, item: u32) -> Option<Plan> {
        let ctx = &mut *self.0;
        let now = ctx.now_ms;
        let me = ctx.state.me.pos;
        let st = &*ctx.state;
        let buyer_ok = |npc: u32| !st.econ.skipped(SkipKind::Buyer, npc, now);
        plan::carry_only(ctx.world, item, [me[0], me[1]], &buyer_ok)
    }

    fn send(&mut self, out: Out) {
        self.0.send(out);
    }

    fn gm(&mut self, line: &str) -> bool {
        self.0.gm(line)
    }

    fn portal_trip(&mut self, x: f32, y: f32) -> bool {
        crate::bot::travel::has_route(self.0, x, y)
    }

    fn travel(&mut self, x: f32, y: f32, radius: f32) {
        self.0.travel(x, y, radius, Pace::Run);
    }

    fn leg_m(&self) -> f32 {
        self.0.motion.leg_m()
    }

    fn stop(&mut self) {
        self.0.motion.stop();
    }

    fn face(&mut self, x: f32, y: f32) {
        self.0.motion.face(x, y);
    }

    fn say(&self, text: &str) {
        self.0.log.say(text);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    /// Choose the route.
    #[default]
    Plan,
    /// Walk back to the spot the bot died at and pick the dropped pack up.
    Recover,
    /// Get the materials and labor.
    Supply,
    /// Walk to the workbench.
    ToBench,
    /// At the bench: start the craft.
    Craft,
    /// Wait for the pack.
    Crafting,
    /// Carry the pack to the buyer.
    ToBuyer,
    /// At the buyer: start the sale.
    Sell,
    /// Wait for the sale to go through.
    Selling,
    /// Wait for the payment mail.
    Payment,
}

/// What a walk tick came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    Going,
    There,
    Stuck,
}

/// What the bot lacks for the run.
#[derive(Debug, Clone)]
struct Shortfall {
    /// `(item, count)` of missing materials.
    materials: SmallVec<[(u32, u32); 4]>,
    /// Labor missing for crafting plus selling (positive = missing).
    labor: i32,
    /// The character level the craft needs when the character is below it, else 0.
    level: u8,
}

impl Shortfall {
    fn none(&self) -> bool {
        self.materials.is_empty() && self.labor <= 0 && self.level == 0
    }

    /// Why the run cannot go on without a GM.
    fn reason(&self) -> &'static str {
        if self.level > 0 {
            LEVEL_TOO_LOW
        } else if self.labor > 0 {
            "not enough labor"
        } else {
            "not enough materials"
        }
    }
}

/// Why a run fails when the character is too low for the craft skill.
const LEVEL_TOO_LOW: &str = "trade packs need a higher character level";

/// The trade pack among the worn `equipment` (a carried pack sits in the back slot), if any.
pub fn carried_pack(equipment: &[EquipSlot], is_pack: impl Fn(u32) -> bool) -> Option<u32> {
    equipment.iter().map(|e| e.template).find(|&t| is_pack(t))
}

/// Why a trade run cannot succeed for this bot right now, `None` when it can be tried. The brain
/// asks this before it picks the goal, so a bot that is too low a level (or has no materials and
/// no GM help) does not pick it over and over just to fail it.
///
/// A bot that carries a pack can always try (selling is all it needs), and with `gm_bootstrap` the
/// GM supplies the rest. Without it the character needs the level of the lowest pack recipe and,
/// for some recipe at its level, the labor and the materials in the bag.
pub fn unavailable(ctx: &Ctx<'_>) -> Option<&'static str> {
    if pack_busy(ctx) || ctx.gm_allowed() {
        return None;
    }
    let (level, labor) = (ctx.state.me.level, i32::from(ctx.state.me.labor));
    if level < ctx.world.min_pack_craft_level() {
        return Some(LEVEL_TOO_LOW);
    }
    let st = &*ctx.state;
    let possible = ctx.world.pack_crafts().any(|c| {
        level >= c.req_level
            && labor >= i32::from(c.labor) + SELL_LABOR
            && c.materials
                .iter()
                .all(|&(item, n)| st.item_count(item) >= i64::from(n))
    });
    (!possible).then_some("no trade pack can be crafted from the bag")
}

/// The trade pack the bot carries, if any.
pub fn carried_pack_of(ctx: &Ctx<'_>) -> Option<u32> {
    carried_pack_in(ctx.world, &ctx.state.equipment)
}

/// [`carried_pack_of`] for code that has the world and the equipment but no [`Ctx`] (the bot
/// task's command path).
pub fn carried_pack_in(world: &aa_world::World, equipment: &[EquipSlot]) -> Option<u32> {
    carried_pack(equipment, |t| !world.specialty_buyers(t).is_empty())
}

/// Whether the bot must not start anything but its trade run: it carries a pack, or one it
/// dropped at a death spot waits to be picked up.
pub fn pack_busy(ctx: &Ctx<'_>) -> bool {
    // (a note past its deadline no longer holds the bot: the recovery writes it off when the run
    // next looks at it, but nothing else may wait for that)
    carried_pack_of(ctx).is_some()
        || ctx
            .state
            .econ
            .lost_pack()
            .is_some_and(|lost| ctx.now_ms < lost.until_ms)
}

/// The goals that are not drawn while [`pack_busy`]: every goal but the trade run. A carrying
/// bot does not leave the run for an event, a quest, a duel, another fight or a rest in the open;
/// it only defends itself and sells the pack.
pub fn pack_block_mask() -> GoalMask {
    let mut mask = 0;
    for g in GoalKind::ALL {
        if g != GoalKind::TradeRun {
            mask |= 1 << g.index();
        }
    }
    mask
}

/// Whether a planned run crosses an area that is deadly for the bot or a zone owned by a hostile
/// alliance (its guards attack on sight). Such a run is not crafted; another budget or route may
/// avoid the area.
pub fn route_risky(
    world: &aa_world::World,
    faction: u32,
    level: u8,
    from: [f32; 2],
    plan: &Plan,
) -> bool {
    let deadly = |a: [f32; 2], b: [f32; 2]| {
        progress::deadly(world, faction, level, (a[0], a[1]), (b[0], b[1]))
    };
    let mut hostile = |x: f32, y: f32| {
        world.zone_info_at(x, y).is_some_and(|z| {
            z.faction != 0 && world.relation(faction, z.faction) == Relation::Hostile
        })
    };
    deadly(from, plan.bench)
        || deadly(plan.bench, plan.buyer)
        || plan::crosses_hostile_zone(from, plan.bench, &mut hostile)
        || plan::crosses_hostile_zone(plan.bench, plan.buyer, &mut hostile)
}

/// Craft, carry and sell trade packs. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct TradeRun {
    phase: Phase,
    plan: Option<Plan>,
    /// The one-line description for the web UI.
    text: Box<str>,
    end_ms: u64,
    phase_ms: u64,
    /// When the last request went out (0 = none yet in this phase).
    sent_ms: u64,
    /// Do nothing before this bot clock (backoff).
    until_ms: u64,
    /// Failed trips in a row.
    fails: u8,
    /// The run reported its end (done or failed): a `stop` without it is an interruption.
    finished: bool,
    /// Attempts in this phase.
    tries: u8,
    /// Craft and sale attempts in this run.
    retries: u8,
    /// Stand closer than usual (the last attempt was out of range).
    close: bool,
}

impl TradeRun {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        *self = TradeRun {
            text: "Trade run: planning a route".into(),
            end_ms: ctx.now_ms + RUN_TIMEOUT_MS,
            phase_ms: ctx.now_ms,
            ..TradeRun::default()
        };
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let status = self.step(&mut Live(ctx));
        self.finished |= status != GoalStatus::Running;
        if matches!(self.phase, Phase::ToBench | Phase::ToBuyer | Phase::Recover) {
            // On the way: the combat controller only fights back against close attackers.
            ctx.combat.note_travelling(ctx.now_ms);
        }
        status
    }

    /// Called when the goal ends. A run that is cut short while the bot carries a pack is noted
    /// in the ledger: the goal that takes over (a rest) can give the run back afterwards, see
    /// [`carried_pack_of`].
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        // (a death dropped the pack: the run is recovered by the next trade run, not by a rest)
        if !self.finished && !ctx.state.me.dead && carried_pack_of(ctx).is_some() {
            ctx.state.econ.note_interrupted_run(ctx.now_ms);
        }
    }

    /// Appends what the bot is doing.
    pub fn describe(&self, out: &mut String) {
        out.push_str(&self.text);
    }

    // ---- the state machine --------------------------------------------------------------

    /// One step of the run against `env`.
    pub fn step(&mut self, env: &mut dyn Env) -> GoalStatus {
        if env.now_ms() >= self.end_ms {
            return self.fail(env, FailReason::Timeout, "the run took too long");
        }
        match self.phase {
            Phase::Plan => self.do_plan(env),
            Phase::Recover => self.do_recover(env),
            Phase::Supply => self.do_supply(env),
            Phase::ToBench => self.do_to_bench(env),
            Phase::Craft => self.do_craft(env),
            Phase::Crafting => self.do_crafting(env),
            Phase::ToBuyer => self.do_to_buyer(env),
            Phase::Sell => self.do_sell(env),
            Phase::Selling => self.do_selling(env),
            Phase::Payment => self.do_payment(env),
        }
    }

    /// Leaves the run: the reason goes to the web UI's event log.
    fn fail(&mut self, env: &mut dyn Env, why: FailReason, text: &'static str) -> GoalStatus {
        env.say(&format!("trade run failed: {text}"));
        let why = match why {
            FailReason::Other(_) => FailReason::Other(text),
            other => other,
        };
        GoalStatus::Failed(why)
    }

    /// Moves to `phase` and renders the new description.
    fn enter(&mut self, env: &dyn Env, phase: Phase) {
        self.phase = phase;
        self.phase_ms = env.now_ms();
        self.sent_ms = 0;
        self.tries = 0;
        self.fails = 0;
        self.text = self.headline(env).into();
    }

    fn headline(&self, env: &dyn Env) -> String {
        if matches!(self.phase, Phase::Recover) {
            return match env.state().econ.lost_pack() {
                Some(lost) => format!(
                    "Trade run: going back for the dropped pack ({})",
                    length(dist2d(env.pos(), lost.pos))
                ),
                None => "Trade run: going back for the dropped pack".into(),
            };
        }
        let Some(plan) = self.plan else {
            return "Trade run: planning a route".into();
        };
        let item = env.name(Named::Item, plan.item);
        let buyer = env.name(Named::Npc, plan.buyer_tpl);
        match self.phase {
            Phase::Plan => "Trade run: planning a route".into(),
            Phase::Recover => "Trade run: going back for the dropped pack".into(),
            Phase::Supply => format!("Trade run: getting materials for {item}"),
            Phase::ToBench => format!(
                "Trade run: going to {} to craft {item} ({})",
                env.name(Named::Doodad, plan.bench_tpl),
                length(plan.to_bench_m)
            ),
            Phase::Craft | Phase::Crafting => format!("Trade run: crafting {item}"),
            Phase::ToBuyer => {
                format!(
                    "Trade run: carrying {item} to {buyer} ({})",
                    length(plan.carry_m)
                )
            }
            Phase::Sell | Phase::Selling => format!("Trade run: selling {item} to {buyer}"),
            Phase::Payment => format!("Trade run: sold {item}, waiting for the payment"),
        }
    }

    fn do_plan(&mut self, env: &mut dyn Env) -> GoalStatus {
        let carried = carried_pack(&env.state().equipment, |t| env.is_pack(t));
        if let Some(lost) = env.state().econ.lost_pack() {
            if carried == Some(lost.item) {
                // (the pack is in the back slot again: recovered, or a relog in between)
                env.state_mut().econ.clear_lost_pack();
                env.say(&format!(
                    "trade run: picked the dropped pack back up ({})",
                    env.name(Named::Item, lost.item)
                ));
            } else if carried.is_some() {
                env.state_mut().econ.clear_lost_pack();
                env.state_mut().econ.note_pack_lost();
                env.say("trade run: pack lost (a newer pack replaced the dropped one)");
            } else if !env.is_pack(lost.item) {
                // (the back slot held a glider, not a trade pack)
                env.state_mut().econ.clear_lost_pack();
            } else {
                if env.state().me.hp_pct() < RECOVER_HP_PCT {
                    env.stop();
                    let text = "Trade run: resting before going back for the dropped pack";
                    if self.text.as_ref() != text {
                        env.say("trade run: resting first, the bot just died");
                        self.text = text.into();
                    }
                    return GoalStatus::Running;
                }
                self.until_ms = 0;
                self.enter(env, Phase::Recover);
                return GoalStatus::Running;
            }
        }
        if carried.is_none() && env.state().me.hp_pct() < START_HP_PCT {
            env.stop();
            let text = "Trade run: resting before the run";
            if self.text.as_ref() != text {
                env.say(&format!(
                    "trade run: resting first ({}% hit points)",
                    env.state().me.hp_pct()
                ));
                self.text = text.into();
            }
            return GoalStatus::Running;
        }
        let need = env.min_pack_level();
        if carried.is_none() && env.state().me.level < need && !env.gm_allowed() {
            env.say(&format!(
                "trade run: crafting packs needs character level {need}"
            ));
            return self.fail(env, FailReason::Other(""), LEVEL_TOO_LOW);
        }
        let plan = match carried {
            Some(item) => env.plan_carry(item),
            None => env.plan_new(),
        };
        let Some(plan) = plan else {
            return self.fail(env, FailReason::NoTarget, "no route within reach");
        };
        self.plan = Some(plan);
        self.retries = 0;
        self.close = false;
        let item = env.name(Named::Item, plan.item);
        if plan.craft == 0 {
            env.say(&format!(
                "trade run: carrying {item} to {} ({})",
                env.name(Named::Npc, plan.buyer_tpl),
                length(plan.carry_m)
            ));
            self.enter(env, Phase::ToBuyer);
        } else {
            env.say(&format!(
                "trade run: craft {} ({item}) at {} ({}), sell to {} ({})",
                plan.craft,
                env.name(Named::Doodad, plan.bench_tpl),
                length(plan.to_bench_m),
                env.name(Named::Npc, plan.buyer_tpl),
                length(plan.carry_m)
            ));
            self.enter(env, Phase::Supply);
        }
        GoalStatus::Running
    }

    /// Goes back to the spot the bot died at and picks the dropped pack up. The decision is
    /// [`ledger::recovery_step`]; this only carries it out.
    fn do_recover(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(lost) = env.state().econ.lost_pack() else {
            self.enter(env, Phase::Plan);
            return GoalStatus::Running;
        };
        // the pack is in the back slot again: the run goes on
        if let Some(item) = carried_pack(&env.state().equipment, |t| env.is_pack(t)) {
            env.state_mut().econ.clear_lost_pack();
            env.say(&format!(
                "trade run: picked the dropped pack back up ({})",
                env.name(Named::Item, item)
            ));
            self.enter(env, Phase::Plan);
            return GoalStatus::Running;
        }
        let pack = find_pack_doodad(env, lost.pos);
        let waited_ms = if self.sent_ms == 0 {
            0
        } else {
            env.now_ms().saturating_sub(self.sent_ms)
        };
        match ledger::recovery_step(
            &lost,
            env.now_ms(),
            env.state().place.instance_id(),
            env.pos(),
            pack.map(|(obj, _, _, d)| (obj, d)),
            waited_ms,
        ) {
            ledger::Recovery::Gone(why) => {
                env.state_mut().econ.clear_lost_pack();
                env.state_mut().econ.note_pack_lost();
                env.say(&format!(
                    "trade run: pack lost ({}; {})",
                    env.name(Named::Item, lost.item),
                    why.word()
                ));
                let reason = match why {
                    ledger::PackGone::Expired => FailReason::Timeout,
                    _ => FailReason::NoTarget,
                };
                self.fail(env, reason, "the dropped pack is gone")
            }
            ledger::Recovery::Return => {
                self.sent_ms = 0;
                match self.walk(env, [lost.pos[0], lost.pos[1]], ledger::PACK_RETURN_M) {
                    Walk::Going => GoalStatus::Running,
                    Walk::Stuck => self.fail(
                        env,
                        FailReason::Stuck,
                        "could not get back to the dropped pack",
                    ),
                    Walk::There => {
                        self.sent_ms = env.now_ms();
                        GoalStatus::Running
                    }
                }
            }
            ledger::Recovery::Wait => {
                if self.sent_ms == 0 {
                    self.sent_ms = env.now_ms();
                }
                env.stop();
                GoalStatus::Running
            }
            ledger::Recovery::Pick(obj) => {
                let Some((_, template, pos, dist)) = pack else {
                    return GoalStatus::Running;
                };
                if self.sent_ms == 0 {
                    self.sent_ms = env.now_ms();
                }
                if dist > PACK_CAST_M {
                    return match self.walk(env, [pos[0], pos[1]], PACK_STOP_M) {
                        Walk::Going | Walk::There => GoalStatus::Running,
                        Walk::Stuck => {
                            self.fail(env, FailReason::Stuck, "could not reach the dropped pack")
                        }
                    };
                }
                if env.now_ms() < self.until_ms {
                    return GoalStatus::Running;
                }
                if self.tries >= MAX_PACK_CASTS {
                    return self.fail(
                        env,
                        FailReason::Timeout,
                        "could not pick up the dropped pack",
                    );
                }
                self.tries += 1;
                self.until_ms = env.now_ms() + PACK_CAST_GAP_MS;
                env.stop();
                env.face(pos[0], pos[1]);
                let skill = match env.doodad_use_skill(template) {
                    0 => PACK_RECOVER_SKILL,
                    s => s,
                };
                env.send(cs::start_skill_doodad(skill, env.state().me.obj, obj));
                GoalStatus::Running
            }
        }
    }

    /// What the bot lacks for crafting plus selling.
    fn shortfall(env: &dyn Env, craft: &Craft) -> Shortfall {
        let st = env.state();
        let mut materials = SmallVec::new();
        for &(item, n) in craft.materials.iter() {
            let have = st.item_count(item);
            if have < i64::from(n) {
                materials.push((item, u32::try_from(i64::from(n) - have).unwrap_or(n)));
            }
        }
        let need = i32::from(craft.labor) + SELL_LABOR + LABOR_MARGIN;
        Shortfall {
            materials,
            labor: need - i32::from(st.me.labor),
            level: if st.me.level < craft.req_level {
                craft.req_level
            } else {
                0
            },
        }
    }

    fn do_supply(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        let Some(craft) = env.craft(plan.craft) else {
            return self.fail(env, FailReason::NoTarget, "unknown craft");
        };
        let short = Self::shortfall(env, craft);
        if short.none() {
            self.enter(env, Phase::ToBench);
            return GoalStatus::Running;
        }
        if !env.gm_allowed() {
            return self.fail(env, FailReason::Other(""), short.reason());
        }
        let now = env.now_ms();
        if self.sent_ms != 0 && now.saturating_sub(self.sent_ms) < SUPPLY_WAIT_MS {
            return GoalStatus::Running;
        }
        if self.tries >= 2 {
            return self.fail(env, FailReason::Other(""), "the GM supply did not arrive");
        }
        self.tries += 1;
        self.sent_ms = now;
        if short.level > 0 {
            env.gm(&format!("/level {}", short.level));
        }
        for (item, mut n) in short.materials {
            while n > 0 {
                let chunk = n.min(GM_ITEM_CHUNK);
                env.gm(&format!("/item add self {item} {chunk}"));
                n -= chunk;
            }
        }
        if short.labor > 0 {
            env.gm(&format!("/labor {}", short.labor.min(2_000)));
        }
        GoalStatus::Running
    }

    fn do_to_bench(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        let found = find_bench(env.state(), &plan);
        let dest = found.map_or(plan.bench, |(_, p)| [p[0], p[1]]);
        let radius = if self.close {
            BENCH_CLOSE_M
        } else {
            BENCH_STOP_M
        };
        match self.walk(env, dest, radius) {
            Walk::Going => GoalStatus::Running,
            Walk::Stuck => {
                self.skip_bench(env, &plan);
                self.fail(env, FailReason::Stuck, "could not get to the workbench")
            }
            Walk::There => {
                if found.is_some() {
                    self.enter(env, Phase::Craft);
                    return GoalStatus::Running;
                }
                let now = env.now_ms();
                if self.sent_ms == 0 {
                    self.sent_ms = now;
                } else if now.saturating_sub(self.sent_ms) > FIND_WAIT_MS {
                    self.skip_bench(env, &plan);
                    return self.fail(env, FailReason::NoTarget, "the workbench is not there");
                }
                GoalStatus::Running
            }
        }
    }

    fn skip_bench(&self, env: &mut dyn Env, plan: &Plan) {
        let now = env.now_ms();
        env.state_mut().econ.skip(
            SkipKind::Bench,
            spot_key(plan.bench[0], plan.bench[1]),
            now,
            SKIP_MS,
        );
    }

    fn do_craft(&mut self, env: &mut dyn Env) -> GoalStatus {
        let now = env.now_ms();
        if now < self.until_ms {
            return GoalStatus::Running;
        }
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        let Some((obj, pos)) = find_bench(env.state(), &plan) else {
            self.enter(env, Phase::ToBench);
            return GoalStatus::Running;
        };
        if dist2d(env.pos(), pos) > BENCH_REACH_M {
            self.enter(env, Phase::ToBench);
            return GoalStatus::Running;
        }
        if let Some(craft) = env.craft(plan.craft) {
            let short = Self::shortfall(env, craft);
            if !short.none() {
                if !env.gm_allowed() {
                    return self.fail(env, FailReason::Other(""), short.reason());
                }
                self.enter(env, Phase::Supply);
                return GoalStatus::Running;
            }
        }
        if env.state().me.hp_pct() < START_HP_PCT {
            env.stop();
            let text = "Trade run: resting before crafting";
            if self.text.as_ref() != text {
                env.say(&format!(
                    "trade run: resting first ({}% hit points)",
                    env.state().me.hp_pct()
                ));
                self.text = text.into();
            }
            return GoalStatus::Running;
        }
        env.stop();
        env.face(pos[0], pos[1]);
        env.send(cs::execute_craft(plan.craft, obj, 1));
        self.phase = Phase::Crafting;
        self.sent_ms = now;
        self.text = self.headline(env).into();
        GoalStatus::Running
    }

    fn do_crafting(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        if carries(env.state(), plan.item) {
            env.say(&format!(
                "trade run: crafted {}",
                env.name(Named::Item, plan.item)
            ));
            self.retries = 0;
            self.enter(env, Phase::ToBuyer);
            return GoalStatus::Running;
        }
        let now = env.now_ms();
        let refused = env.state().econ.error_since(self.sent_ms);
        let silent = now.saturating_sub(self.sent_ms) > CRAFT_WAIT_MS;
        if refused.is_none() && !silent {
            return GoalStatus::Running;
        }
        if let Some((err, reason)) = refused {
            env.say(&format!(
                "trade run: the craft was refused (error {err}, {reason})"
            ));
        }
        self.craft_failed(env, &plan)
    }

    /// The craft did not produce the pack: find out why and retry, or give up.
    fn craft_failed(&mut self, env: &mut dyn Env, plan: &Plan) -> GoalStatus {
        let now = env.now_ms();
        if let Some(craft) = env.craft(plan.craft) {
            let short = Self::shortfall(env, craft);
            if !short.none() {
                // Spent or lost on the way (another craft, a GM command that did not land).
                if env.gm_allowed() && self.retries < MAX_RETRIES {
                    self.retries += 1;
                    self.enter(env, Phase::Supply);
                    return GoalStatus::Running;
                }
                return self.fail(env, FailReason::Other(""), short.reason());
            }
        }
        self.retries += 1;
        if self.retries >= MAX_RETRIES {
            env.state_mut()
                .econ
                .skip(SkipKind::Craft, plan.craft, now, SKIP_MS);
            self.skip_bench(env, plan);
            return self.fail(
                env,
                FailReason::Other(""),
                "the workbench refused the craft",
            );
        }
        // Back off, then approach closer and try again.
        self.until_ms = now + 3_000 * u64::from(self.retries);
        self.close = true;
        self.enter(env, Phase::ToBench);
        GoalStatus::Running
    }

    fn do_to_buyer(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        if !carries(env.state(), plan.item) {
            return self.fail(env, FailReason::Other(""), "the trade pack is gone");
        }
        let found = find_npc(env.state(), plan.buyer_tpl, plan.buyer);
        let dest = found.map_or(plan.buyer, |(_, p)| [p[0], p[1]]);
        let radius = if self.close { NPC_CLOSE_M } else { NPC_STOP_M };
        match self.walk(env, dest, radius) {
            Walk::Going => GoalStatus::Running,
            Walk::Stuck => {
                self.skip_buyer(env, &plan);
                self.fail(env, FailReason::Stuck, "could not get to the buyer")
            }
            Walk::There => {
                if found.is_some() {
                    self.enter(env, Phase::Sell);
                    return GoalStatus::Running;
                }
                let now = env.now_ms();
                if self.sent_ms == 0 {
                    self.sent_ms = now;
                } else if now.saturating_sub(self.sent_ms) > FIND_WAIT_MS {
                    self.skip_buyer(env, &plan);
                    return self.fail(env, FailReason::NoTarget, "the buyer is not there");
                }
                GoalStatus::Running
            }
        }
    }

    fn skip_buyer(&self, env: &mut dyn Env, plan: &Plan) {
        let now = env.now_ms();
        env.state_mut()
            .econ
            .skip(SkipKind::Buyer, plan.buyer_tpl, now, SKIP_MS);
    }

    fn do_sell(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        let now = env.now_ms();
        if now < self.until_ms {
            return GoalStatus::Running;
        }
        if !carries(env.state(), plan.item) {
            return self.fail(env, FailReason::Other(""), "the trade pack is gone");
        }
        let Some((obj, pos)) = find_npc(env.state(), plan.buyer_tpl, plan.buyer) else {
            self.enter(env, Phase::ToBuyer);
            return GoalStatus::Running;
        };
        if dist2d(env.pos(), pos) > NPC_REACH_M {
            self.enter(env, Phase::ToBuyer);
            return GoalStatus::Running;
        }
        let labor = i32::from(env.state().me.labor);
        if labor < SELL_LABOR {
            if !env.gm_allowed() {
                return self.fail(env, FailReason::Other(""), "not enough labor to sell");
            }
            if self.sent_ms != 0 && now.saturating_sub(self.sent_ms) < SUPPLY_WAIT_MS {
                return GoalStatus::Running;
            }
            if self.tries >= 2 {
                return self.fail(env, FailReason::Other(""), "the GM labor did not arrive");
            }
            self.tries += 1;
            self.sent_ms = now;
            env.gm(&format!("/labor {}", SELL_LABOR - labor + LABOR_MARGIN));
            return GoalStatus::Running;
        }
        env.stop();
        env.face(pos[0], pos[1]);
        env.send(cs::sell_backpack_goods(obj));
        self.phase = Phase::Selling;
        self.sent_ms = now;
        self.text = self.headline(env).into();
        GoalStatus::Running
    }

    fn do_selling(&mut self, env: &mut dyn Env) -> GoalStatus {
        let Some(plan) = self.plan else {
            return self.fail(env, FailReason::NoTarget, "no plan");
        };
        if !carries(env.state(), plan.item) {
            env.state_mut().econ.note_sale();
            env.say(&format!(
                "trade run: sold {} to {}, the payment is coming by mail",
                env.name(Named::Item, plan.item),
                env.name(Named::Npc, plan.buyer_tpl)
            ));
            self.enter(env, Phase::Payment);
            return GoalStatus::Running;
        }
        let now = env.now_ms();
        match env.state().econ.error_since(self.sent_ms) {
            Some((ERR_TOO_FAR_AWAY, _)) => {
                self.retries += 1;
                if self.retries >= MAX_RETRIES {
                    self.skip_buyer(env, &plan);
                    return self.fail(env, FailReason::Other(""), "the buyer is out of reach");
                }
                self.close = true;
                self.enter(env, Phase::ToBuyer);
                GoalStatus::Running
            }
            Some((ERR_NOT_ENOUGH_LABOR, _)) => {
                self.enter(env, Phase::Sell);
                GoalStatus::Running
            }
            Some((
                ERR_INVALID | ERR_INVALID_TARGET | ERR_STORE_NO_GOODS | ERR_CANT_SELL_HERE,
                _,
            )) => {
                self.skip_buyer(env, &plan);
                self.fail(env, FailReason::Other(""), "the buyer refused the pack")
            }
            Some(_) | None if now.saturating_sub(self.sent_ms) > SELL_WAIT_MS => {
                self.retries += 1;
                if self.retries >= MAX_RETRIES {
                    self.skip_buyer(env, &plan);
                    return self.fail(env, FailReason::Timeout, "the sale got no answer");
                }
                self.enter(env, Phase::Sell);
                GoalStatus::Running
            }
            _ => GoalStatus::Running,
        }
    }

    fn do_payment(&mut self, env: &mut dyn Env) -> GoalStatus {
        let waited = env.now_ms().saturating_sub(self.phase_ms);
        if env.state().econ.awaiting() == 0 {
            return GoalStatus::Done;
        }
        if waited > PAYMENT_WAIT_MS {
            env.say(
                "trade run: the payment mail is not here yet; it will be collected when it arrives",
            );
            return GoalStatus::Done;
        }
        GoalStatus::Running
    }

    // ---- walking ------------------------------------------------------------------------

    /// One walking tick towards `dest`, stopping `radius` metres short.
    fn walk(&mut self, env: &mut dyn Env, dest: [f32; 2], radius: f32) -> Walk {
        let me = env.pos();
        let d = dist2d(me, [dest[0], dest[1], 0.0]);
        // After a refusal for being out of range the bot really goes to the stop radius.
        let slack = if self.close {
            CLOSE_SLACK_M
        } else {
            ARRIVE_SLACK_M
        };
        if d <= radius + slack {
            env.stop();
            self.fails = 0;
            return Walk::There;
        }
        match env.motion() {
            MotionStatus::Moving => return Walk::Going,
            MotionStatus::Failed(_) => {
                self.fails += 1;
                if self.fails >= MAX_TRIP_FAILS {
                    return Walk::Stuck;
                }
            }
            MotionStatus::Idle | MotionStatus::Arrived => {}
        }
        // A new leg: the whole way when it is short or a `/move`, else a stretch of it.
        let leg = env.leg_m();
        if d <= leg + 20.0 || env.would_teleport(d) || env.portal_trip(dest[0], dest[1]) {
            env.travel(dest[0], dest[1], radius);
        } else {
            let k = leg / d;
            env.travel(
                me[0] + (dest[0] - me[0]) * k,
                me[1] + (dest[1] - me[1]) * k,
                1.0,
            );
        }
        Walk::Going
    }
}

/// Collects the payment of sold trade packs. Runs on every brain tick whatever the bot is doing:
/// the payment mail can come long after the goal ended.
pub fn collect_payments(ctx: &mut Ctx<'_>) {
    let paid = ctx.state.econ.tick(ctx.now_ms, ctx.out);
    if let Some(fault) = ctx.state.econ.take_fault() {
        ctx.find(crate::bot::watch::Finding::Payment { fault });
    }
    if let Some(p) = paid {
        let mut text = format!(
            "trade run: payment collected, {}",
            coins(i64::from(p.money))
        );
        if p.items > 0 {
            let _ = write!(text, " and {} item(s)", p.items);
        }
        let e = &ctx.state.econ;
        let _ = write!(
            text,
            " (mail {}; {} packs sold, {} lost, {} earned in all)",
            p.mail_id,
            e.packs_sold,
            e.packs_lost,
            coins(e.earned)
        );
        ctx.log.say(text);
    }
}

// ---- helpers ------------------------------------------------------------------------------

/// Whether the bot carries (has equipped) the pack `item`.
fn carries(st: &BotState, item: u32) -> bool {
    st.equipment.iter().any(|e| e.template == item)
}

/// The workbench object in view that belongs to the planned spawn: `(object id, position)`.
fn find_bench(st: &BotState, plan: &Plan) -> Option<(u32, [f32; 3])> {
    let spawn = [plan.bench[0], plan.bench[1], 0.0];
    st.doodads
        .iter()
        .filter(|(_, d)| d.template == plan.bench_tpl)
        .map(|(obj, d)| (*obj, d.pos, dist2d(d.pos, spawn)))
        .filter(|c| c.2 <= SPAWN_MATCH_M)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|c| (c.0, c.1))
}

/// The dropped-pack doodad in view near the death spot `at` (the one nearest to the spot):
/// `(object, template, position, distance to the bot)`. The distance is the bot's, not the
/// spot's: the doodad is in view long before the bot is within reach of it.
fn find_pack_doodad(env: &dyn Env, at: [f32; 3]) -> Option<(u32, u32, [f32; 3], f32)> {
    let me = env.pos();
    env.state()
        .doodads
        .iter()
        .filter(|(_, d)| env.is_pack_doodad(d.template))
        .map(|(&obj, d)| (obj, d.template, d.pos, dist2d(d.pos, at)))
        .filter(|c| c.3 <= ledger::PACK_NEAR_M)
        .min_by(|a, b| a.3.total_cmp(&b.3))
        .map(|(obj, template, pos, _)| (obj, template, pos, dist2d(pos, me)))
}

/// The living buyer NPC in view that belongs to the planned spawn: `(object id, position)`.
fn find_npc(st: &BotState, template: u32, spawn: [f32; 2]) -> Option<(u32, [f32; 3])> {
    let spawn = [spawn[0], spawn[1], 0.0];
    st.units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && u.id == template && !u.dead())
        .map(|(obj, u)| (*obj, u.pos, dist2d(u.pos, spawn)))
        .filter(|c| c.2 <= SPAWN_MATCH_M)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|c| (c.0, c.1))
}

/// "640 m" or "1.2 km".
fn length(m: f32) -> String {
    if m >= 1000.0 {
        format!("{:.1} km", m / 1000.0)
    } else {
        format!("{m:.0} m")
    }
}

/// Copper as "3g 30s 48c".
pub fn coins(copper: i64) -> String {
    let (g, s, c) = (copper / 10_000, copper / 100 % 100, copper % 100);
    match (g, s) {
        (0, 0) => format!("{c}c"),
        (0, _) => format!("{s}s {c}c"),
        _ => format!("{g}g {s}s {c}c"),
    }
}

#[cfg(test)]
mod tests;
