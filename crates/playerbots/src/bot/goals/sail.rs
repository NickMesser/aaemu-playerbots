//! Sail: summon a boat, sail it across the sea and come ashore.
//!
//! A bot gets a boat summon scroll (the cheapest boat's, added with the GM `/item add self` line
//! when GM commands are allowed; without them a bot sails only when its bag already holds one),
//! picks a trip on the sea map (a crossing of 1 to 3 km to another beach, or a loop out to sea and
//! back; see [`trip`]), walks to the launch beach, faces the water and casts the scroll's skill on
//! itself. The server puts the boat on the water ahead; the bot swims to it, binds the driver's seat,
//! probes which way the wheel turns, sails the planned route with a heading controller
//! ([`crate::bot::vehicle`]), stops at the landing, leaves the boat, despawns it and swims ashore.
//! Lakes and rivers are out of scope: the map only knows the sea.
//!
//! The state machine is [`machine`] (pure: an `Input` in, `Act`s out, tested against a scripted
//! server); this file connects it to the bot: it builds the input from [`Ctx`], applies the acts
//! (packets, movement, GM lines, issue reports) and holds the scoring. Whatever ends the goal (done,
//! failed, interrupted by a fight, a rest or the operator) passes through [`Sail::stop`], which
//! leaves the seat and despawns the boat, so no boat is left behind (the server also removes a boat
//! whose owner logs out or changes the instance).
//!
//! Boats are small and turn slowly (a rowboat 4.35 degrees per second), so the goal asks the bot task
//! for fast ticks while the bot is on a boat (`VehicleState::fast_tick`).
//!
//! Which boat is `[sail] ship` ([`crate::config::ShipChoice`]): the rowboat by default, or a
//! clipper, cutter, junk, yawl or schooner ([`aa_world::travel::ShipClass`]). The bigger hulls
//! launch from beaches with room and deep water, sail on the deep sea mask with a wide margin and
//! rounded corners, slow down for the corners and for the berth, and berth out at sea.

mod machine;
mod piracy;
pub mod trip;

#[cfg(test)]
mod tests;

use std::cell::RefCell;

use aa_proto::cs;
use aa_world::travel::{ShipClass, ShipFamily};
use aa_world::water::{find_shore, Disc, ShoreSpot, WaterMap, NAV_FLOOR_M};
use aa_world::World;
use pb_web::Activity;

use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, Pace};
use crate::bot::state::BotState;
use crate::config::{BehaviorCfg, ShipChoice};
use crate::roster::Temperament;

use self::machine::{Act, Input, Machine, Ops, Pick, MIN_SPEED};
use self::trip::{Plan, PlanError, TripCfg};
use super::{FailReason, GoalStatus};

/// A bot sails at most once in this long (bot clock milliseconds).
const COOLDOWN_MS: u64 = 30 * 60_000;
/// A bot that has been online for less than this does not sail yet.
const SETTLE_MS: u64 = 3 * 60_000;
/// Hit points (percent) below which a bot does not set out.
const MIN_HP_PCT: u8 = 60;
/// A beach is "near" when the terrain dips under the sea at one of these distances.
const COAST_PROBES_M: [f32; 3] = [100.0, 250.0, 450.0];

/// Summoning a boat and sailing it. See the module docs.
#[derive(Debug, Default)]
pub struct Sail {
    trip: Option<Box<Machine>>,
    trade_destination: Option<(f32, f32)>,
    patrol: Option<Box<piracy::Patrol>>,
}

impl Sail {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        self.trade_destination = None;
        let armed = naval_ready(ctx);
        let sailor = ctx.state.vehicle.sailor_mut();
        sailor.active = true;
        if armed {
            sailor.naval = Some(Box::default());
            self.patrol = Some(Box::default());
        }
        self.trip = Some(Box::new(Machine::new(ctx.now_ms, ctx.state.me.pos)));
    }

    pub(super) fn start_trade_passage(&mut self, ctx: &mut Ctx<'_>, buyer: [f32; 2]) {
        self.start(ctx);
        self.trade_destination = Some((buyer[0], buyer[1]));
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(machine) = self.trip.as_mut() else {
            return GoalStatus::Failed(FailReason::Other("not started"));
        };
        if !ctx.cfg.behavior.sailing {
            return GoalStatus::Failed(FailReason::Other("sailing is switched off"));
        }
        if machine.underway() && self.patrol.as_mut().is_some_and(|patrol| patrol.tick(ctx)) {
            return GoalStatus::Running;
        }
        let world = ctx.world;
        let Some(pick) = pick_for(ctx) else {
            return GoalStatus::Failed(FailReason::Other("no boat scroll in the data"));
        };
        let (Some(map), Some(shore_map)) = (world.water_for(&pick.class), world.water()) else {
            return GoalStatus::Failed(FailReason::Other("no sea map"));
        };
        let mut acts = Vec::new();
        let status = {
            let ops = RealOps {
                world,
                map,
                shore_map,
                class: pick.class,
                faction: ctx.state.me.faction,
                trade_destination: self.trade_destination,
                rng: RefCell::new(&mut *ctx.rng),
            };
            let io = input(
                ctx.state,
                ctx.now_ms,
                ctx.cfg.bots.gm_bootstrap && self.trade_destination.is_none(),
                ctx.motion.status(),
                &pick,
                &ops,
            );
            machine.step(&io, &mut acts)
        };
        apply(ctx, acts);
        status
    }

    /// Called when the goal ends, fails or is interrupted: leaves the seat and despawns the boat.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if let Some(mut patrol) = self.patrol.take() {
            patrol.stop(ctx);
            ctx.state.vehicle.sailor_mut().naval = None;
        }
        if let Some(mut machine) = self.trip.take() {
            let world = ctx.world;
            if let Some((pick, map, shore_map)) = pick_for(ctx)
                .and_then(|pick| Some((pick, world.water_for(&pick.class)?, world.water()?)))
            {
                let mut acts = Vec::new();
                {
                    let ops = RealOps {
                        world,
                        map,
                        shore_map,
                        class: pick.class,
                        faction: ctx.state.me.faction,
                        trade_destination: self.trade_destination,
                        rng: RefCell::new(&mut *ctx.rng),
                    };
                    let io = input(
                        ctx.state,
                        ctx.now_ms,
                        ctx.cfg.bots.gm_bootstrap,
                        ctx.motion.status(),
                        &pick,
                        &ops,
                    );
                    machine.stop(&io, &mut acts);
                }
                apply(ctx, acts);
            }
            let now = ctx.now_ms;
            let sailor = ctx.state.vehicle.sailor_mut();
            sailor.active = false;
            sailor.last_trip_end_ms = now.max(1);
            sailor.trips = sailor.trips.saturating_add(u16::from(machine.sailed));
        }
        ctx.motion.stop();
    }

    /// Whether the goal drives the combat controller itself (see [`super::Goal::owns_combat`]):
    /// on the boat it must not be paused by a fight.
    pub fn owns_combat(&self) -> bool {
        self.trip.as_ref().is_some_and(|m| m.owns_combat())
    }

    /// The activity the web UI shows while the goal runs.
    pub fn activity(&self, moving: bool, fighting: bool) -> Activity {
        if fighting {
            Activity::Fighting
        } else if moving || self.owns_combat() {
            Activity::Traveling
        } else {
            Activity::Idle
        }
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        out.push_str("sail");
        if let Some(m) = &self.trip {
            out.push_str(": ");
            m.describe(out);
        }
    }
}

/// The goal's weight before the situation is looked at: 0 (the goal scores itself, see [`score`],
/// from the temperament and the situation).
pub fn base_weight(_behavior: &BehaviorCfg, _temper: &Temperament) -> f32 {
    0.0
}

/// The weight of a sailing trip for a bot with this temperament: explorer and merchant types like
/// the sea, homebodies do not. About a fifth of the quest weight for a neutral bot.
pub fn weight(temper: &Temperament) -> f32 {
    0.2 * (0.3 + 0.7 * (1.0 - temper.homebody) + 0.7 * temper.merchant)
}

/// The score of the goal for this tick: 0 unless sailing is on, the bot is settled, healthy, in the
/// main world, off its cooldown, near a coast and able to get a scroll; the temperament's weight
/// otherwise. Cheap (a few terrain probes): the brain asks whenever it scores the goals.
pub fn score(ctx: &Ctx<'_>, _base: f32) -> f32 {
    if !ctx.cfg.behavior.sailing
        || ctx.state.place.in_instance()
        || ctx.now_ms < SETTLE_MS
        || ctx.state.me.hp_pct() < MIN_HP_PCT
    {
        return 0.0;
    }
    if let Some(s) = ctx.state.vehicle.sailor() {
        if s.last_trip_end_ms != 0 && ctx.now_ms < s.last_trip_end_ms + COOLDOWN_MS {
            return 0.0;
        }
    }
    let world = ctx.world;
    let Some(pick) = pick_for(ctx) else {
        return 0.0;
    };
    if !(ctx.gm_allowed() || ctx.state.item_count(pick.item) > 0) {
        return 0.0;
    }
    if !coast_near(world, ctx.state.me.pos) {
        return 0.0;
    }
    weight(&ctx.persona.temper)
}

/// Whether the terrain dips under the sea within a few hundred metres of `pos` (a coast, or the
/// bot is in the water).
fn coast_near(world: &World, pos: [f32; 3]) -> bool {
    if world.baked().terrain.is_none() {
        return false;
    }
    COAST_PROBES_M.iter().any(|&r| {
        (0..12).any(|i| {
            let a = i as f32 * std::f32::consts::TAU / 12.0;
            world
                .ground_z(pos[0] + a.cos() * r, pos[1] + a.sin() * r)
                .is_some_and(|z| z > 0.0 && z < NAV_FLOOR_M)
        })
    })
}

/// The scroll and boat of this bot ([`pick_scroll`] with the bot's config, bag and temperament).
fn pick_for(ctx: &Ctx<'_>) -> Option<Pick> {
    pick_scroll(
        ctx.world,
        ctx.cfg.sail.ship,
        ctx.state.me.char_id,
        ctx.persona.temper.merchant,
        |item| ctx.state.item_count(item),
    )
}

pub(super) fn owned_trade_ship(ctx: &Ctx<'_>) -> Option<ShipClass> {
    if !ctx.cfg.behavior.sailing || ctx.state.place.in_instance() {
        return None;
    }
    let pick = pick_for(ctx)?;
    (ctx.state.item_count(pick.item) > 0).then_some(pick.class)
}

pub fn naval_ready(ctx: &Ctx<'_>) -> bool {
    ctx.cfg.behavior.sailing
        && piracy::enabled(ctx)
        && pick_for(ctx).is_some_and(|pick| {
            ctx.state.item_count(pick.item) > 0
                && ctx
                    .world
                    .travel()
                    .boats
                    .slave_template(pick.class.slave)
                    .is_some_and(|hull| hull.cannon_points().next().is_some())
        })
}

pub(super) fn plan_trade_passage(
    ctx: &Ctx<'_>,
    ship: ShipClass,
    from: [f32; 2],
    to: [f32; 2],
) -> Option<Plan> {
    let world = ctx.world;
    let from = (from[0], from[1]);
    let to = (to[0], to[1]);
    let ground = |x, y| world.ground_z(x, y);
    let avoid = danger(world, world.water()?, ctx.state.me.faction, from, Some(to));
    trip::plan_passage(
        world.water_for(&ship)?,
        &ground,
        from,
        to,
        &TripCfg::for_ship(ship),
        &avoid,
    )
    .ok()
}

/// The boat family a bot sails. A fixed choice is that family. `auto` is the first family the bag
/// holds a scroll of (`holds`), else a boat of the bot's own: a roll seeded by its character id
/// (rowboat 40, clipper 30, cutter 12, junk 6, schooner 12; a merchant (`merchant` over 0.6) takes
/// the schooner for the rowboat's share).
pub fn family_of(
    choice: ShipChoice,
    char_id: u32,
    merchant: f32,
    holds: impl Fn(ShipFamily) -> bool,
) -> ShipFamily {
    if let Some(f) = choice.family() {
        return f;
    }
    if let Some(f) = ShipFamily::ALL.into_iter().find(|&f| holds(f)) {
        return f;
    }
    let mut rng = fastrand::Rng::with_seed(u64::from(char_id) ^ 0x5a11_0b0a);
    let (rowboat, schooner) = if merchant > 0.6 { (10, 42) } else { (40, 12) };
    let roll = rng.u32(0..100);
    let table = [
        (rowboat, ShipFamily::Rowboat),
        (30, ShipFamily::Clipper),
        (12, ShipFamily::Cutter),
        (6, ShipFamily::Junk),
        (schooner, ShipFamily::Schooner),
    ];
    let total: u32 = table.iter().map(|t| t.0).sum();
    let mut at = roll * total / 100;
    for (w, f) in table {
        if at < w {
            return f;
        }
        at -= w;
    }
    ShipFamily::Rowboat
}

/// The scroll to use: of the family the bot sails ([`family_of`]), one the bag holds, else the
/// cheapest scroll of the family; with the class of the boat it summons.
fn pick_scroll(
    world: &World,
    choice: ShipChoice,
    char_id: u32,
    merchant: f32,
    bag_count: impl Fn(u32) -> i64,
) -> Option<Pick> {
    let boats = &world.travel().boats;
    let holds = |f: ShipFamily| {
        boats
            .scrolls()
            .iter()
            .any(|s| f.slaves().contains(&s.slave) && bag_count(s.item) > 0)
    };
    let family = family_of(choice, char_id, merchant, holds);
    let scroll = boats.scroll_for(family, &bag_count)?;
    let boat = boats.slave_template(scroll.slave)?;
    (boat.has_driver_seat() && boat.top_speed() >= MIN_SPEED).then(|| Pick {
        item: scroll.item,
        skill: scroll.skill.id,
        cast_ms: scroll.skill.cast_ms,
        class: boat.class(),
    })
}

/// What the bot knows this tick, for the state machine.
fn input<'a>(
    st: &'a BotState,
    now_ms: u64,
    gm: bool,
    motion: MotionStatus,
    pick: &Pick,
    ops: &'a dyn Ops,
) -> Input<'a> {
    let sailor = st.vehicle.sailor();
    let boat_unit = sailor
        .and_then(|s| s.boat.as_ref())
        .and_then(|b| st.units.get(&b.obj))
        .map(|u| u.pos);
    Input {
        now_ms,
        char_id: st.me.char_id,
        pos: st.me.pos,
        gm,
        pick: *pick,
        held: st.find_item(pick.item).map(|(_, i)| (i.id, i.template)),
        sailor,
        boat_unit,
        errors: st.errors.as_slice(),
        motion,
        ops,
    }
}

/// Carries out what the machine asked for.
fn apply(ctx: &mut Ctx<'_>, acts: Vec<Act>) {
    let me = ctx.state.me.obj;
    for act in acts {
        match act {
            Act::Say(text) => ctx.log.say(text),
            Act::Gm(line) => {
                ctx.gm(&line);
            }
            Act::Travel { x, y, radius } => ctx.travel(x, y, radius, Pace::Run),
            Act::Face { x, y } => ctx.motion.face(x, y),
            Act::StopMotion => ctx.motion.stop(),
            Act::Sync(pos) => ctx.motion.teleported(pos),
            Act::Travelling => ctx.combat.note_travelling(ctx.now_ms),
            Act::Pin => {
                if let Some(pin) = ctx.motion.pin(ctx.now_ms, false) {
                    ctx.send(pin);
                }
            }
            Act::Summon {
                skill,
                item,
                template,
            } => ctx.send(cs::start_skill_item(skill, me, item, template, me)),
            Act::Bind(tl) => ctx.send(cs::bind_slave(tl)),
            Act::Discard(tl) => ctx.send(cs::discard_slave(tl)),
            Act::Despawn(obj) => ctx.send(cs::despawn_slave(obj)),
            Act::Helm { ship, cmd } => ctx.send(cs::ship_request(
                ship,
                ctx.now_ms as u32,
                cmd.throttle,
                cmd.steering,
            )),
            Act::Report(report) => ctx.report_issue(*report),
            Act::Find(finding) => ctx.find(finding),
        }
    }
}

/// The machine's view of the world: the sea map, the terrain and the monsters.
struct RealOps<'a> {
    world: &'a World,
    /// The sea mask of the boat's class (the deep one for the large hulls).
    map: &'a WaterMap,
    /// The standard sea mask: the swimmer's beaches and the monsters.
    shore_map: &'a WaterMap,
    class: ShipClass,
    faction: u32,
    trade_destination: Option<(f32, f32)>,
    rng: RefCell<&'a mut fastrand::Rng>,
}

fn danger(
    world: &World,
    map: &WaterMap,
    faction: u32,
    from: (f32, f32),
    destination: Option<(f32, f32)>,
) -> Vec<Disc> {
    let (center, radius) = destination.map_or((from, 5_000.0), |to| {
        let center = ((from.0 + to.0) * 0.5, (from.1 + to.1) * 0.5);
        let radius = (to.0 - center.0).hypot(to.1 - center.1) + 2_000.0;
        (center, radius)
    });
    trip::danger_zones(world, map, faction, center, radius)
}

impl Ops for RealOps<'_> {
    fn danger(&self, from: (f32, f32)) -> Vec<Disc> {
        danger(
            self.world,
            self.shore_map,
            self.faction,
            from,
            self.trade_destination,
        )
    }

    fn plan(&self, from: (f32, f32), avoid: &[Disc]) -> Result<Plan, PlanError> {
        let ground = |x: f32, y: f32| self.world.ground_z(x, y);
        let cfg = TripCfg::for_ship(self.class);
        if let Some(to) = self.trade_destination {
            return trip::plan_passage(self.map, &ground, from, to, &cfg, avoid);
        }
        trip::plan_trip(
            self.map,
            &ground,
            &mut self.rng.borrow_mut(),
            from,
            &cfg,
            avoid,
        )
    }

    fn route(&self, start: (f32, f32), plan: &Plan, avoid: &[Disc]) -> Option<Vec<(f32, f32)>> {
        trip::sail_route(self.map, start, plan, avoid, &self.class)
    }

    fn shore_near(&self, from: (f32, f32), max_r: f32) -> Option<ShoreSpot> {
        let ground = |x: f32, y: f32| self.world.ground_z(x, y);
        find_shore(self.shore_map, &ground, from, max_r, &[])
    }

    fn ground(&self, x: f32, y: f32) -> Option<f32> {
        self.world.ground_z(x, y)
    }
}
