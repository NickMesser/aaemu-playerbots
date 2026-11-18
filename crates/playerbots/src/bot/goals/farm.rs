//! Farm: buy seeds, plant, wait, harvest, and grow what the server is short of.
//!
//! A bot that farms keeps plots on the land of its own houses and on the public farms (see
//! [`crate::bot::farm`]). The goal does one of two jobs per run:
//!
//! * **harvest** when a plot is due (ripe, or its growth time is over): walk to the crop, cast the
//!   harvest skill, wait for the crop to change, plant the seeds the harvest gave back on the
//!   same spot;
//! * **plant** otherwise, when the bot has room for plots: choose a crop ([`plan::choose`]: the
//!   items the operator or the population director wishes for first, then the scarce ones, else
//!   what pays), buy its seeds from the nearest vendor (or, with `bots.gm_bootstrap` and no
//!   vendor or money, mint them), walk to a free spot on its own land or a public farm and plant.
//!
//! # Wild farms
//!
//! With `[farm] wild`, a bot whose `outlaw` trait is at least `wild_outlaw` (or one started with
//! [`GoalArgs::wild`]) plants up to `wild_plots` crops on one small patch out in the open
//! ([`site::wild_patch`]: secluded, dry, flat, out of the farms, housing areas and war zones). A
//! seed there costs labor ([`plan::labor_cap`]; a GM bot tops it up). The plots go into the
//! journal's `wild` section, so the bot finds them again after a restart ([`restore_wild`]); a
//! crop that is gone when the bot comes back counts as stolen, and its patch is given up for
//! another one. A goal started with [`GoalArgs::item`] grows that seed (or a crop that gives that
//! item).
//!
//! With `[farm] steal` an outlaw also harvests a ripe crop of somebody else out in the wild when
//! no real player is near ([`steal_target`]): only the crops of the other bots of this process,
//! unless `steal_from_players` (never with `bots.gm_bootstrap`). The server lets anybody harvest
//! a crop outside the public farms and the houses; within a day of its planting the thief leaves
//! footprints the owner can report. What a theft gives is never the bot's own for the ledger, and
//! the stolen spot is not planted again.
//!
//! The state machine is [`machine`] (pure, tested against a scripted server); this file builds its
//! input from [`Ctx`], applies the acts (packets, movement, GM lines), and holds the scoring.
//!
//! # Honesty
//!
//! A harvest is the bot's own to sell only when the seeds were bought: the machine opens the
//! ledger's harvest window with the plot's flag ([`FarmMem::harvest_honest`]) for the length of
//! a harvest cast, and a plot is honest when the ledger counted the seed it planted as clean.
//! GM-minted seeds, and plots found again after a reconnect, are not. See
//! [`crate::bot::market::ledger`].
//!
//! # Scoring
//!
//! [`score`] is 0 unless `farm.enabled`, the bot is settled, healthy and in the main world, and
//! there is something to do: a due plot (three times the goal's [`weight`]), or room for plots, a
//! crop the bot can get seeds for and a place to plant it (the weight, up to two and a half times
//! that when the crop is wished for or scarce). The population director's `farm` bias and its
//! wish nudge apply on top (`brain::scores_now`).

pub mod machine;
mod stock;

#[cfg(test)]
mod tests;

use std::cell::RefCell;

use aa_proto::cs::{self, BuyItem, SHOP_MONEY};
use aa_world::{Crop, FarmKind, Vec3, World};
use pb_web::Activity;
use smallvec::SmallVec;

use crate::bot::cmd::GoalArgs;
use crate::bot::ctx::Ctx;
use crate::bot::farm::plan::{self, Env, Plan, Vendor};
use crate::bot::farm::site::{self, Site, WildEnv};
use crate::bot::farm::{FarmMem, Foreign, Plot, MAX_PLOTS, STOLEN_LAND, WILD_LAND};
use crate::bot::goals::housing::garden_radius;
use crate::bot::motion::Pace;
use crate::bot::state::{dist2d, BotState, UnitKind};
use crate::config::BehaviorCfg;
use crate::contest::plot_key;
use crate::hub::{unix_ms, Hub, Logger};
use crate::journal::WildPlot;
use crate::population::economy::ScarcityView;
use crate::roster::Temperament;

use self::machine::{Act, Input, Machine, Ops};
use super::{FailReason, GoalStatus};

/// A bot that has been online for less than this does not farm yet.
const SETTLE_MS: u64 = 90_000;
/// Hit points (percent) below which a bot does not set out.
const MIN_HP_PCT: u8 = 60;
/// A bot plants again at most this often (a harvest is not held back).
const RUN_COOLDOWN_MS: u64 = 5 * 60_000;
/// The score of a due plot, as a multiple of the goal's weight.
const HARVEST_BOOST: f32 = 3.0;
/// How much of the need of the best crop is added to the weight, per point (up to
/// [`NEED_CAP`]).
const NEED_GAIN: f32 = 0.5;
/// The most need that counts towards the weight.
const NEED_CAP: f32 = 3.0;

/// Buying seeds, planting and harvesting. See the module docs.
#[derive(Debug, Default)]
pub struct Farm {
    run: Option<Box<Machine>>,
    stock: Option<Box<stock::Job>>,
    /// What the goal was started with (`wild`: plant on a wild patch, `item`: the seed to grow).
    args: Option<Box<GoalArgs>>,
}

impl Farm {
    /// A fresh goal (the brain starts it with [`Goal::new`](super::Goal::new)).
    pub fn new() -> Farm {
        Farm::default()
    }

    /// A fresh goal started with `args` (`GoalArgs::wild`: plant on unprotected land, `item`: the
    /// seed to grow, or a crop that gives that item).
    pub fn with_args(args: Option<Box<GoalArgs>>) -> Farm {
        Farm {
            args,
            ..Self::default()
        }
    }

    /// The arguments it was started with.
    pub fn args(&self) -> Option<&GoalArgs> {
        self.args.as_deref()
    }

    /// Called once when the brain picks the goal: finds the wild plots of the journal again and
    /// decides between harvesting and planting.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        restore_wild(ctx);
        if let Some(job) = stock::choose(ctx) {
            self.stock = Some(Box::new(job));
            return;
        }
        self.run = choose_job(ctx, self.args.as_deref()).map(Box::new);
        match &self.run {
            Some(m) => ctx.log.say(format!(
                "goal: farm ({})",
                if m.plants() { "plant" } else { "harvest" }
            )),
            None => ctx.log.say("goal: farm (nothing to do)"),
        }
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if !ctx.cfg.farm.enabled {
            return GoalStatus::Failed(FailReason::Other("farming is switched off"));
        }
        if let Some(job) = self.stock.as_mut() {
            return job.tick(ctx);
        }
        let Some(machine) = self.run.as_mut() else {
            return GoalStatus::Failed(FailReason::NoTarget);
        };
        let husbandry = stock::enabled(ctx);
        let mut mem = ctx.state.market.take_farm();
        let harvested_before = mem.harvested;
        let mut acts = Vec::new();
        let status = {
            let world = ctx.world;
            let ops = RealOps {
                world,
                hub: ctx.hub,
                bot: ctx.persona.id,
                now_ms: ctx.now_ms,
                fallback_z: ctx.state.me.pos[2],
                rng: RefCell::new(&mut *ctx.rng),
                husbandry,
            };
            let st: &BotState = ctx.state;
            let count = |t: u32| st.item_count(t);
            let item = |t: u32| st.find_item(t).map(|(_, i)| (i.id, i64::from(i.count)));
            let clean = |t: u32| st.market.clean_count(t);
            let pos = st.me.pos;
            let merchant = |npc: u32| nearest_merchant(st, npc, pos);
            let io = Input {
                now_ms: ctx.now_ms,
                pos,
                me_obj: st.me.obj,
                gm: ctx.cfg.bots.gm_bootstrap,
                replant: ctx.cfg.farm.replant,
                money: st.me.money,
                labor: st.me.labor,
                motion: ctx.motion.status(),
                errors: st.errors.as_slice(),
                count: &count,
                item: &item,
                clean: &clean,
                merchant: &merchant,
                max_trip_m: ctx.cfg.farm.max_trip_m,
                ops: &ops,
            };
            machine.step(&io, &mut mem, &mut acts)
        };
        let last_crop = (mem.harvested != harvested_before).then_some(mem.last_crop);
        if acts.iter().any(|a| matches!(a, Act::Publish)) || mem.has_gone() {
            let patch = machine.plan().map(|p| p.site).filter(Site::is_wild);
            sync_wild(ctx, &mut mem, patch.map(|s| s.centre));
        }
        ctx.state.market.put_farm(mem);
        apply(ctx, acts);
        if let Some(doodad) = last_crop {
            log_produce(ctx, doodad);
        }
        status
    }

    /// Called when the goal ends, fails or is interrupted: the harvest window closes, the walk
    /// stops, the board is told what grows.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(stock) = self.stock.take() {
            stock.stop(ctx);
            return;
        }
        let mut mem = ctx.state.market.take_farm();
        if let Some(mut machine) = self.run.take() {
            let mut acts = Vec::new();
            machine.stop(&mut mem, &mut acts);
            mem.last_end_ms = ctx.now_ms.max(1);
            mem.drop_stolen();
            let patch = machine.plan().map(|p| p.site).filter(Site::is_wild);
            sync_wild(ctx, &mut mem, patch.map(|s| s.centre));
            ctx.state.market.put_farm(mem);
            apply(ctx, acts);
        } else {
            ctx.state.market.put_farm(mem);
        }
        ctx.motion.stop();
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]).
    pub fn owns_combat(&self) -> bool {
        false
    }

    /// The activity the web UI shows while the goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        if fighting {
            Activity::Fighting
        } else if moving {
            Activity::Traveling
        } else {
            Activity::Idle
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        out.push_str("farm");
        if let Some(job) = &self.stock {
            out.push_str(job.description());
        }
        if let Some(m) = &self.run {
            out.push_str(": ");
            out.push_str(m.note());
            if let Some(p) = m.plan() {
                out.push_str(&format!(" (seed {}, {})", p.seed, p.site.describe()));
            }
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`],
/// from its [`weight`] and the situation).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of a farm run for a bot with this temperament: a farmer is a homebody or a merchant
/// at heart. About a third of the quest weight for a neutral bot.
pub fn weight(temper: &Temperament) -> f32 {
    0.35 * (0.5 + 0.5 * temper.homebody + 0.5 * temper.merchant)
}

/// The score of the goal for this tick. 0 keeps the bot from picking it. Called whenever the brain
/// scores the goals, so the cheap checks come first.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    let cfg = &ctx.cfg.farm;
    let base = weight(&ctx.persona.temper);
    if !cfg.enabled
        || ctx.state.place.in_instance()
        || ctx.now_ms < SETTLE_MS
        || ctx.state.me.hp_pct() < MIN_HP_PCT
        || ctx.world.farm().crops().is_empty()
    {
        return 0.0;
    }
    let mem = ctx.state.market.farm();
    let now = ctx.now_ms;
    let pos = ctx.state.me.pos;
    if stock::choose(ctx).is_some()
        || mem.is_some_and(|m| m.plots.iter().any(|p| harvest_due(ctx, p, now, pos)))
    {
        return base * HARVEST_BOOST;
    }
    // a ripe wild plot of the journal the memory has not found again yet (after a restart)
    let now_unix = unix_ms() / 1000;
    if ctx.hub.journal.read(ctx.persona.id, |j| {
        j.wild.plots.iter().any(|p| {
            p.ripe_unix <= now_unix
                && dist2d(pos, p.pos) <= cfg.max_trip_m
                && mem.is_none_or(|m| m.at(p.pos[0], p.pos[1]).is_none())
        })
    }) {
        return base * HARVEST_BOOST;
    }
    if steal_target(ctx).is_some() {
        return base * HARVEST_BOOST;
    }
    if mem.is_some_and(|m| m.last_end_ms != 0 && now < m.last_end_ms + RUN_COOLDOWN_MS) {
        return 0.0;
    }
    let mut rng = fastrand::Rng::with_seed(u64::from(ctx.persona.id) ^ (now / 60_000));
    let who = Who::of(ctx, None);
    match plan_now(ctx.world, ctx.state, ctx.hub, ctx.cfg, &who, now, &mut rng) {
        Some((pick, _)) => base * (1.0 + NEED_GAIN * pick.need.min(NEED_CAP)),
        None => 0.0,
    }
}

/// Who plants, and with what wishes: the farm planner's view of the bot beyond its state.
struct Who {
    bot: u32,
    /// A wild patch may be used (`[farm] wild` and an outlaw, or the `wild` argument).
    wild: bool,
    /// Only a wild patch (the `wild` argument).
    wild_only: bool,
    /// The seed (or produce) the goal was started for.
    item: Option<u32>,
    /// This planting uses the portion of farm space assigned to the guild.
    guild_order: bool,
    livestock_order: Option<u32>,
    /// The bot keeps out of zones at war.
    avoid_war: bool,
}

impl Who {
    fn of(ctx: &Ctx<'_>, args: Option<&GoalArgs>) -> Who {
        let cfg = &ctx.cfg.farm;
        let asked = args.is_some_and(|a| a.wild);
        let outlaw = ctx.persona.profile.outlaw >= cfg.wild_outlaw;
        let order = crate::bot::guild::profession::farm_order(ctx);
        Who {
            bot: ctx.persona.id,
            wild: order.is_none() && cfg.wild && (asked || outlaw),
            wild_only: order.is_none() && cfg.wild && asked,
            item: order.or_else(|| args.and_then(|a| a.item)),
            guild_order: order.is_some(),
            livestock_order: order.filter(|_| stock::enabled(ctx)),
            avoid_war: crate::bot::war::avoiding(&ctx.cfg.events),
        }
    }
}

fn harvest_due(ctx: &Ctx<'_>, plot: &Plot, now: u64, pos: [f32; 3]) -> bool {
    plot.due(now)
        && dist2d(pos, plot.pos) <= ctx.cfg.farm.max_trip_m
        && !(stock::enabled(ctx) && ctx.world.farm().livestock_yields(plot.doodad).is_some())
}

/// Harvest if a plot is due and reachable, else plant if there is a plan.
fn choose_job(ctx: &mut Ctx<'_>, args: Option<&GoalArgs>) -> Option<Machine> {
    let cfg = &ctx.cfg.farm;
    let now = ctx.now_ms;
    let pos = ctx.state.me.pos;
    let mem = ctx.state.market.farm();
    let fetch = cfg.public_farms && mem.is_none_or(|m| !m.list_asked);
    if mem.is_some_and(|m| m.plots.iter().any(|p| harvest_due(ctx, p, now, pos))) {
        return Some(Machine::harvest(fetch, now));
    }
    if let Some(f) = steal_target(ctx) {
        let seed = ctx
            .world
            .farm()
            .crop_of_doodad(f.doodad)
            .map_or(0, |c| c.seed);
        ctx.state.market.farm_mut().plots.push(Plot {
            obj: f.obj,
            doodad: f.doodad,
            seed,
            pos: f.pos,
            phase: f.phase,
            ripe: true,
            check_ms: 0,
            planted_ms: 0,
            honest: false,
            land: STOLEN_LAND,
        });
        ctx.log.say(format!(
            "farm: helps itself to a ripe crop at ({:.0}, {:.0}) that is not its own",
            f.pos[0], f.pos[1]
        ));
        return Some(Machine::harvest(false, now));
    }
    let who = Who::of(ctx, args);
    let (_, plan) = plan_now(ctx.world, ctx.state, ctx.hub, ctx.cfg, &who, now, ctx.rng)?;
    ctx.log.say(format!(
        "farm: plan {:?} seed {} x{} (buy {}, mint {}) at {}",
        plan.why,
        plan.seed,
        plan.want,
        plan.buy,
        plan.mint,
        plan.site.describe()
    ));
    Some(Machine::plant(plan, fetch, now))
}

/// The best crop for the bot now and the plan to plant it.
fn plan_now(
    world: &World,
    state: &BotState,
    hub: &Hub,
    cfg: &crate::config::Config,
    who: &Who,
    now_ms: u64,
    rng: &mut fastrand::Rng,
) -> Option<(plan::Pick, Plan)> {
    let fcfg = &cfg.farm;
    let bot = who.bot;
    let mem = state.market.farm();
    let wild = if who.wild {
        wild_site(world, state, hub, cfg, who, rng)
    } else {
        None
    };
    let mut sites = if who.wild_only {
        Vec::new()
    } else {
        candidate_sites(world, state, mem, cfg, who.livestock_order.is_some())
    };
    // an outlaw plants out in the wild first, the rest where it may
    if let Some(w) = wild {
        sites.insert(0, w);
    }
    if sites.is_empty() {
        return None;
    }
    let mut wished = hub.pop.wished();
    if let Some(item) = who.item.filter(|_| who.guild_order) {
        let farm = world.farm();
        if let Some(crop) = farm.crop_of_seed(item) {
            let yields = who
                .livestock_order
                .and_then(|_| farm.livestock_yields(crop.doodad))
                .unwrap_or(&crop.produce);
            wished.extend(
                yields
                    .iter()
                    .map(|y| y.item)
                    .filter(|&product| product != item),
            );
        } else {
            wished.push(item);
        }
    }
    let env = RealEnv {
        world,
        state,
        hub,
        bot,
        gm: cfg.bots.gm_bootstrap,
        max_trip_m: fcfg.max_trip_m,
        wished,
        sites: &sites,
        mem,
        now_ms,
        livestock_order: who.livestock_order,
    };
    let crops = world.farm().crops();
    let pick = match who.item {
        // the goal was started for an item: that seed, or a crop that gives it
        Some(item) => crops
            .iter()
            .filter(|c| {
                c.seed == item
                    || env
                        .livestock_yields(c)
                        .unwrap_or(&c.produce)
                        .iter()
                        .any(|y| y.item == item)
            })
            .filter_map(|c| plan::evaluate(c, &env, fcfg.growth_rate))
            .max_by(|a, b| a.score.total_cmp(&b.score))?,
        None => plan::choose(crops, &env, fcfg.growth_rate, rng)?,
    };
    let crop = world.farm().crop_of_seed(pick.seed)?;
    let gm = cfg.bots.gm_bootstrap;
    let plan = sites.iter().filter(|s| takes(crop, s)).find_map(|&site| {
        let room = if site.is_wild() {
            site::wild_room(mem, fcfg.max_plots, fcfg.wild_plots)
        } else {
            let limit = if who.guild_order {
                fcfg.max_plots.div_ceil(2)
            } else {
                fcfg.max_plots
            };
            site::room(mem, site.land, limit)
        };
        let plan = plan::make_plan(&pick, site, room, state.me.money, gm)?;
        if site.is_wild() {
            plan::labor_cap(plan, state.me.labor, seed_labor(world, pick.seed), gm)
        } else {
            Some(plan)
        }
    })?;
    Some((pick, plan))
}

/// The labor planting one `seed` out in the wild costs: its use skill's `consume_lp`.
fn seed_labor(world: &World, seed: u32) -> i16 {
    world
        .skill(world.item_use_skill(seed))
        .map_or(0, |s| i16::try_from(s.labor).unwrap_or(i16::MAX))
}

/// The bot's wild patch: the one in the journal while it has room, else a new secluded one
/// within reach ([`site::wild_patch`]). `None` when the bot has no room for wild plots or no spot
/// will do.
fn wild_site(
    world: &World,
    state: &BotState,
    hub: &Hub,
    cfg: &crate::config::Config,
    who: &Who,
    rng: &mut fastrand::Rng,
) -> Option<Site> {
    let fcfg = &cfg.farm;
    let mem = state.market.farm();
    if site::wild_room(mem, fcfg.max_plots, fcfg.wild_plots) == 0 {
        return None;
    }
    let pos = state.me.pos;
    let (patch, bad) = hub
        .journal
        .read(who.bot, |j| (j.wild.patch, j.wild.bad_patches.clone()));
    if let Some(c) = patch.filter(|c| dist2d(pos, [c[0], c[1], 0.0]) <= fcfg.max_trip_m) {
        return Some(Site::wild(c));
    }
    let env = WildWorld {
        world,
        state,
        hub,
        avoid_war: who.avoid_war,
        bad: &bad,
    };
    site::wild_patch(&env, pos, rng)
}

/// Whether `site` takes `crop`: a garden and a wild patch take any crop that grows on land, a
/// farm the crops of its kind.
fn takes(crop: &Crop, site: &Site) -> bool {
    if crop.group == 5 && site.is_wild() {
        return false;
    }
    match site.kind() {
        None => true,
        Some(kind) => crop.allowed_in(kind),
    }
}

/// The places the bot may plant on now: its gardens and the public farms within reach, with room
/// left, nearest first.
fn candidate_sites(
    world: &World,
    state: &BotState,
    mem: Option<&FarmMem>,
    cfg: &crate::config::Config,
    include_ranch: bool,
) -> Vec<Site> {
    let fcfg = &cfg.farm;
    let mut all: Vec<Site> = Vec::new();
    if fcfg.own_land {
        all.extend(site::gardens(&state.houses, |template| {
            world.housing_design(template).map_or(4.0, garden_radius)
        }));
    }
    if fcfg.public_farms {
        let mut kinds = FarmKind::Farm.bit() | FarmKind::Nursery.bit();
        if include_ranch {
            kinds |= FarmKind::Ranch.bit();
        }
        all.extend(site::farms(world.farm().areas(), kinds));
    }
    all.retain(|s| site::room(mem, s.land, fcfg.max_plots) > 0);
    site::nearest(all, state.me.pos, fcfg.max_trip_m)
}

/// The ripe crop of somebody else in view an outlaw may take (`[farm] steal`, see the module
/// docs): out in the wild, of another bot of this process (or, with `steal_from_players` and
/// without `bots.gm_bootstrap`, of anybody), not watched by a real player. The nearest.
pub fn steal_target(ctx: &Ctx<'_>) -> Option<Foreign> {
    let cfg = &ctx.cfg.farm;
    use crate::bot::guild::profession::{membership, Profession};
    let thief = matches!(membership(ctx), Some((_, Profession::Thief, false)));
    if !cfg.steal
        || !thief && ctx.persona.profile.outlaw < cfg.wild_outlaw
        || thief && ctx.state.team().is_some_and(|team| team.in_team())
        || ctx.real_player_near
        || crate::bot::guild::profession::farm_order(ctx).is_some()
    {
        return None;
    }
    let players = cfg.steal_from_players && !ctx.cfg.bots.gm_bootstrap;
    let mem = ctx.state.market.farm()?;
    let (world, me) = (ctx.world, &ctx.state.me);
    let war = crate::bot::war::avoiding(&ctx.cfg.events);
    mem.foreign
        .iter()
        .filter(|f| {
            let [x, y, _] = f.pos;
            f.owner != me.char_id
                && (players || ctx.hub.is_bot_char(f.owner))
                && world
                    .farm()
                    .crop_of_doodad(f.doodad)
                    .is_some_and(|c| c.is_ripe(f.phase))
                && mem.at(x, y).is_none()
                && dist2d(me.pos, f.pos) <= cfg.max_trip_m
                && world.housing_area_at(x, y).is_none()
                && world.farm().area_at(x, y).is_none()
                && !(war && crate::bot::war::at_war(world, ctx.state, x, y))
        })
        .min_by(|a, b| dist2d(me.pos, a.pos).total_cmp(&dist2d(me.pos, b.pos)))
        .copied()
}

/// Puts the wild plots of the journal (planted before a restart, or by an earlier session) back
/// into the farm memory, so the harvest finds them; forgets the ones that withered long ago.
fn restore_wild(ctx: &mut Ctx<'_>) {
    let (id, now_unix, now) = (ctx.persona.id, unix_ms() / 1000, ctx.now_ms);
    let journal = &ctx.hub.journal;
    if journal.read(id, |j| j.wild.plots.is_empty()) {
        return;
    }
    let plots = journal.update(id, |j| {
        j.wild.prune(now_unix);
        j.wild.plots.clone()
    });
    let mem = ctx.state.market.farm_mut();
    let mut back = 0;
    for wp in plots {
        if mem.plots.len() >= MAX_PLOTS || mem.at(wp.pos[0], wp.pos[1]).is_some() {
            continue;
        }
        let left_ms = wp.ripe_unix.saturating_sub(now_unix).saturating_mul(1000);
        mem.plots.push(Plot {
            obj: 0,
            doodad: wp.doodad,
            seed: wp.seed,
            pos: wp.pos,
            phase: 0,
            ripe: false,
            check_ms: now.saturating_add(left_ms).max(1),
            planted_ms: 0,
            honest: wp.honest,
            land: WILD_LAND,
        });
        back += 1;
    }
    if back > 0 {
        ctx.log.say(format!(
            "farm: remembers {back} wild plot(s) from the journal"
        ));
    }
}

/// Writes the wild plots to the journal: the ones planted in this session, the ones harvested
/// (forgotten) and the ones gone when the bot came back (stolen: the patch is given up).
/// `patch`: the middle of the wild patch the run plants on.
fn sync_wild(ctx: &Ctx<'_>, mem: &mut FarmMem, patch: Option<[f32; 2]>) {
    let gone = mem.take_gone();
    for g in gone.iter().filter(|g| g.land == STOLEN_LAND && g.harvested) {
        ctx.log.say(format!(
            "farm: took the crop at ({:.0}, {:.0}); it is not the bot's own to sell",
            g.pos[0], g.pos[1]
        ));
    }
    let now_unix = unix_ms() / 1000;
    let (now, rate) = (ctx.now_ms, ctx.cfg.farm.growth_rate);
    let world = ctx.world;
    let fresh: SmallVec<[WildPlot; 4]> = mem
        .plots
        .iter()
        .filter(|p| p.land == WILD_LAND && p.obj != 0 && p.planted_ms != 0)
        .map(|p| {
            let planted = now_unix.saturating_sub(now.saturating_sub(p.planted_ms) / 1000);
            let grow_s = world
                .farm()
                .crop_of_doodad(p.doodad)
                .map_or(0, |c| c.grow_ms_at(rate) / 1000);
            WildPlot {
                pos: p.pos,
                seed: p.seed,
                doodad: p.doodad,
                planted_unix: planted,
                ripe_unix: planted + grow_s,
                honest: p.honest,
            }
        })
        .collect();
    if fresh.is_empty() && gone.iter().all(|g| g.land != WILD_LAND) {
        return;
    }
    let mut stolen: SmallVec<[[f32; 3]; 2]> = SmallVec::new();
    ctx.hub.journal.update(ctx.persona.id, |j| {
        for g in gone.iter().filter(|g| g.land == WILD_LAND) {
            if g.harvested {
                j.wild.harvested_at(g.pos);
            } else if j.wild.stolen_at(g.pos) {
                stolen.push(g.pos);
            }
        }
        for f in &fresh {
            let known = j
                .wild
                .plots
                .iter()
                .any(|p| dist2d(p.pos, f.pos) < crate::bot::farm::SAME_SPOT_M);
            if !known {
                j.wild.add(f.clone());
            }
        }
        if let Some(c) = patch.filter(|_| !fresh.is_empty() && stolen.is_empty()) {
            j.wild.patch = Some(c);
        }
    });
    for p in stolen {
        say_stolen(ctx.log, p);
    }
}

fn say_stolen(log: &Logger, p: [f32; 3]) {
    log.say(format!(
        "farm: the wild crop at ({:.0}, {:.0}) is gone: somebody took it; the next patch goes \
         somewhere else",
        p[0], p[1]
    ));
}

/// The NPC spawns within `radius` of (x, y) that are townsfolk to a bot of `faction`: friendly to
/// it, or with a service (a merchant, a trainer) or a quest. The wildlife does not count.
pub fn townsfolk(world: &World, faction: u32, x: f32, y: f32, radius: f32) -> u32 {
    let mut n = 0u32;
    world.npc_spawns_within(Vec3::new(x, y, 0.0), radius, |t, _| {
        if world.npc(t).is_some_and(|npc| {
            npc.roles.0 != 0
                || npc.is_quest_giver
                || world.relation(faction, npc.faction_id) == aa_world::Relation::Friendly
        }) {
            n += 1;
        }
    });
    n
}

/// The world's side of [`site::wild_patch`].
struct WildWorld<'a> {
    world: &'a World,
    state: &'a BotState,
    hub: &'a Hub,
    avoid_war: bool,
    /// Patches where crops were stolen.
    bad: &'a [[f32; 2]],
}

impl WildEnv for WildWorld<'_> {
    fn ground(&self, x: f32, y: f32) -> Option<f32> {
        super::walkable(self.world, x, y).then(|| self.world.ground_z(x, y).unwrap_or(0.0))
    }

    fn protected(&self, x: f32, y: f32) -> bool {
        self.world.housing_area_at(x, y).is_some()
            || self.world.farm().area_at(x, y).is_some()
            || (self.avoid_war && crate::bot::war::at_war(self.world, self.state, x, y))
    }

    fn npcs(&self, x: f32, y: f32, radius: f32, cap: u32) -> u32 {
        townsfolk(self.world, self.state.me.faction, x, y, radius).min(cap)
    }

    fn quest_giver(&self, x: f32, y: f32, radius: f32) -> bool {
        let mut found = false;
        let world = self.world;
        world.npc_spawns_within(Vec3::new(x, y, 0.0), radius, |t, _| {
            found = found || world.npc(t).is_some_and(|n| n.is_quest_giver);
        });
        found
    }

    fn monster(&self, x: f32, y: f32, radius: f32) -> bool {
        let faction = self.state.me.faction;
        crate::bot::goals::quest::progress::worst_near(self.world, faction, x, y, radius, u8::MAX)
            > 0
    }

    fn town(&self, x: f32, y: f32, radius: f32) -> bool {
        let near = |p: Vec3| (p.x - x).powi(2) + (p.y - y).powi(2) <= radius * radius;
        let portals = &self.world.travel().portals;
        portals.recall_points().iter().any(|r| near(r.pos))
            || portals.respawn_points().iter().any(|r| near(r.pos))
    }

    fn player(&self, x: f32, y: f32, radius: f32) -> bool {
        self.state.units.values().any(|u| {
            u.kind == UnitKind::Character
                && dist2d(u.pos, [x, y, 0.0]) <= radius
                && !self.hub.is_bot_char(u.id)
        })
    }

    fn bad(&self, x: f32, y: f32) -> bool {
        self.bad
            .iter()
            .any(|b| dist2d([b[0], b[1], 0.0], [x, y, 0.0]) <= site::WILD_BAD_M)
    }
}

/// The nearest merchant NPC of `template` that is in view and alive: `(object id, position)`.
fn nearest_merchant(state: &BotState, template: u32, me: [f32; 3]) -> Option<(u32, [f32; 3])> {
    state
        .units
        .iter()
        .filter(|(_, u)| u.kind == UnitKind::Npc && u.id == template && !u.dead())
        .map(|(&obj, u)| (obj, u.pos, u.dist2d(me)))
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(obj, pos, _)| (obj, pos))
}

/// Logs what the bot holds of the produce of the crop it just harvested, and how much of it the
/// ledger counts as its own (the operator's evidence that the honest window worked).
fn log_produce(ctx: &mut Ctx<'_>, doodad: u32) {
    let Some(crop) = ctx.world.farm().crop_of_doodad(doodad) else {
        return;
    };
    for y in crop.produce.iter().take(4) {
        let held = ctx.state.item_count(y.item);
        let clean = ctx.state.market.clean_count(y.item);
        let name = ctx.world.item_name(y.item).unwrap_or("?");
        ctx.log.say(format!(
            "farm: holds {held} of {name} ({}), the ledger counts {clean} as its own",
            y.item
        ));
    }
}

/// Carries out what the machine asked for.
fn apply(ctx: &mut Ctx<'_>, acts: Vec<Act>) {
    let mut publish = false;
    for act in acts {
        match act {
            Act::Say(text) => ctx.log.say(text),
            Act::Gm(line) => {
                ctx.gm(&line);
            }
            Act::Travel { x, y, radius } => ctx.travel(x, y, radius, Pace::Run),
            Act::Face { x, y } => ctx.motion.face(x, y),
            Act::StopMotion => {
                if ctx.motion.is_moving() {
                    ctx.motion.stop();
                }
            }
            Act::RequestList => ctx.send(cs::request_common_farm_list()),
            Act::Buy { npc, item, count } => ctx.send(cs::buy_items(
                npc,
                &[BuyItem {
                    item,
                    grade: 0,
                    count,
                    currency: SHOP_MONEY,
                }],
            )),
            Act::Plant {
                doodad,
                pos,
                z_rot,
                item_id,
            } => ctx.send(cs::create_doodad(
                doodad,
                (pos[0], pos[1], pos[2]),
                z_rot,
                1.0,
                item_id,
            )),
            Act::Harvest { skill, obj } => {
                let me = ctx.state.me.obj;
                ctx.send(cs::start_skill_doodad(skill, me, obj));
            }
            Act::Publish => publish = true,
        }
    }
    if publish {
        crate::bot::farm::publish(ctx.hub, ctx.persona.id, ctx.state, ctx.world);
    }
}

/// What the planner reads about the bot and the world.
struct RealEnv<'a> {
    world: &'a World,
    state: &'a BotState,
    hub: &'a Hub,
    bot: u32,
    gm: bool,
    max_trip_m: f32,
    wished: Vec<u32>,
    sites: &'a [Site],
    mem: Option<&'a FarmMem>,
    now_ms: u64,
    livestock_order: Option<u32>,
}

impl Env for RealEnv<'_> {
    fn livestock_yields(&self, crop: &Crop) -> Option<&[aa_world::Yield]> {
        let order = self.livestock_order?;
        let yields = self.world.farm().livestock_yields(crop.doodad)?;
        (crop.seed == order || yields.iter().any(|y| y.item == order)).then_some(yields)
    }

    fn have(&self, seed: u32) -> i64 {
        self.state.item_count(seed)
    }

    fn price(&self, item: u32) -> u32 {
        self.world.item_facts(item).map_or(0, |f| f.price)
    }

    fn value(&self, item: u32) -> f32 {
        self.world.item_facts(item).map_or(0.0, |f| {
            let sold = f.refund as f32;
            if f.tradable() {
                sold.max(f.price as f32 * 0.5)
            } else {
                sold
            }
        })
    }

    fn scarcity(&self, item: u32) -> f32 {
        self.hub.pop.scarcity(item)
    }

    fn wished(&self, item: u32) -> bool {
        self.wished.contains(&item)
    }

    fn growing(&self, item: u32) -> f32 {
        let _ = self.bot;
        self.hub.farms.units(item)
    }

    fn vendor(&self, seed: u32) -> Option<Vendor> {
        let pos = self.state.me.pos;
        let from = aa_world::Vec3::new(pos[0], pos[1], pos[2]);
        self.world
            .farm()
            .vendors_of(seed)
            .iter()
            .filter_map(|&npc| {
                let at = self.world.nearest_npc_spawn(npc, from)?;
                let dist = at.dist2d(from);
                (dist <= self.max_trip_m).then_some(Vendor {
                    npc,
                    pos: [at.x, at.y, at.z],
                    dist,
                })
            })
            .min_by(|a, b| a.dist.total_cmp(&b.dist))
    }

    fn place_for(&self, crop: &Crop) -> bool {
        self.sites.iter().any(|s| takes(crop, s))
    }

    fn bad_seed(&self, seed: u32) -> bool {
        self.mem.is_some_and(|m| m.seed_is_bad(seed, self.now_ms))
    }

    fn money(&self) -> i64 {
        self.state.me.money
    }

    fn gm(&self) -> bool {
        self.gm
    }
}

/// The machine's view of the world: the terrain, the farm areas, the skills.
struct RealOps<'a> {
    world: &'a World,
    hub: &'a Hub,
    bot: u32,
    now_ms: u64,
    fallback_z: f32,
    rng: RefCell<&'a mut fastrand::Rng>,
    husbandry: bool,
}

impl Ops for RealOps<'_> {
    fn managed_stock(&self, doodad: u32) -> bool {
        self.husbandry && self.world.farm().livestock_yields(doodad).is_some()
    }

    fn spot(&self, mem: &FarmMem, site: &Site, radius_mm: u16) -> Option<[f32; 3]> {
        let spacing = site::spacing_m(radius_mm);
        let (world, hub, bot, now) = (self.world, self.hub, self.bot, self.now_ms);
        let fits = |x: f32, y: f32| {
            super::walkable(world, x, y)
                && !mem.spot_is_bad(x, y, now)
                && !mem
                    .plots
                    .iter()
                    .any(|p| dist2d(p.pos, [x, y, 0.0]) < spacing)
                && hub.contest.others(plot_key(x, y), bot) == 0
        };
        let mut rng = self.rng.borrow_mut();
        let [x, y] = site::pick_spot(site, world.farm().areas(), &mut rng, &fits)?;
        hub.contest.claim(plot_key(x, y), bot);
        Some([x, y, world.ground_z(x, y).unwrap_or(self.fallback_z)])
    }

    fn harvest_skill(&self, doodad: u32, phase: u32) -> Option<u32> {
        self.world
            .farm()
            .crop_of_doodad(doodad)?
            .harvest_skill(phase)
    }

    fn cast_ms(&self, skill: u32) -> u64 {
        self.world
            .skill(skill)
            .map_or(0, |s| u64::from(s.casting_time))
    }

    fn radius_mm(&self, doodad: u32) -> u16 {
        self.world
            .farm()
            .crop_of_doodad(doodad)
            .map_or(500, |c| c.radius_mm)
    }

    fn plant_labor(&self, seed: u32) -> i16 {
        seed_labor(self.world, seed)
    }
}
