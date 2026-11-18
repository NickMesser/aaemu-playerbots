//! The farm goal's state machine. A pure function of its state, an [`Input`] (what the bot knows
//! this tick) and the bot's [`FarmMem`], returning [`Act`]s (what the bot should do), so a whole
//! run is tested against a scripted server without a network or a [`Ctx`](crate::bot::ctx::Ctx).
//!
//! A run is one of two jobs:
//!
//! * **plant** ([`Machine::plant`]): `Fetch` (ask the server for the crops the bot has on the
//!   public farms, once per session), `Shop` (walk to the seed vendor and buy) or `Mint` (a GM
//!   command, only when there is no vendor or no money), then `Sow` (draw a free spot, walk next
//!   to it, `CSCreateDoodad`, wait for the crop to appear);
//! * **reap** ([`Machine::harvest`]): `Reap` (walk to the nearest ripe plot, start the harvest skill,
//!   wait for the crop to change, then plant the seeds the harvest gave back on the same spot).
//!
//! The memory ([`FarmMem`]) follows the server's events outside the machine; the machine reads it
//! (which plots exist, which are ripe) and writes the plots it plants and drops. The harvest
//! window ([`FarmMem::harvest_honest`]) is open exactly while a harvest cast is under way.

use smallvec::SmallVec;

use crate::bot::farm::plan::Plan;
use crate::bot::farm::site::Site;
use crate::bot::farm::{refusal, FarmMem, Plot, SAME_SPOT_M, STOLEN_LAND, WILD_LAND};
use crate::bot::motion::{MotionStatus, MoveFailure};
use crate::bot::state::{dist2d, StateError};

use super::super::{FailReason, GoalStatus};

/// A run (all phases) ends after this long.
pub const RUN_LIMIT_MS: u64 = 25 * 60_000;
/// How long to wait for the server's list of the crops on the public farms.
const FETCH_WAIT_MS: u64 = 2_500;
/// Wait for the goods after a purchase.
const BUY_WAIT_MS: u64 = 4_000;
/// Purchases tried before the vendor is given up.
const MAX_BUYS: u8 = 3;
/// The merchant must be within 3 m (the server's `SHOP_RANGE`); the bot stops this close.
const SHOP_STOP_M: f32 = 2.0;
/// The bot buys when it is at most this far from the merchant.
const SHOP_REACH_M: f32 = 2.6;
/// How long a vendor may be out of view once the bot stands at its spawn.
const VENDOR_VIEW_MS: u64 = 10_000;
/// Wait for GM-minted goods to show up in the bag.
const MINT_WAIT_MS: u64 = 10_000;
/// Wait for a crop to appear after `CSCreateDoodad`.
const PLANT_WAIT_MS: u64 = 6_000;
/// The bot stops this far from the spot it plants on (the server allows 30 m; standing close
/// puts the crop in view at once and the bot beside it, not on it).
const STAND_M: f32 = 3.0;
/// Pause after facing the spot, before the seed is planted.
const FACE_WAIT_MS: u64 = 500;
/// Refusals of spots a run puts up with before it stops.
const MAX_REFUSALS: u8 = 6;
/// The bot stops this far from a crop it harvests.
const REAP_STOP_M: f32 = 2.0;
/// The harvest cast is sent when the bot is at most this far from the crop.
const REAP_REACH_M: f32 = 3.5;
/// A crop that is not in view when the bot stands next to its place is gone after this long.
const VIEW_WAIT_MS: u64 = 8_000;
/// Harvest casts sent for one crop.
const MAX_CASTS: u8 = 3;
/// Wait after the cast time for the crop to change.
const REAP_EXTRA_MS: u64 = 5_000;
/// Wait for a harvested crop to leave the world before its place is planted again.
const GONE_WAIT_MS: u64 = 4_000;
/// Pause between two casts, on top of the cast time.
const CAST_GAP_MS: u64 = 1_500;
/// Gap between two walk commands towards the same spot.
const GO_GAP_MS: u64 = 1_000;
/// A trip the motion controller cuts off at its time limit is continued, at most this many legs.
const MAX_LEGS: u8 = 6;
/// A leg must have brought the bot at least this much closer to count as progress.
const LEG_MIN_GAIN_M: f32 = 30.0;
/// A plot that was not ripe when visited is looked at again after this long.
const REVISIT_MS: u64 = 3 * 60_000;
/// A plot that could not be reached is tried again after this long.
const UNREACHABLE_MS: u64 = 10 * 60_000;
/// Labor points the bot asks for with `/labor` when a harvest needs more than it has.
const LABOR_TOP_UP: i16 = 60;
/// Visited plots remembered per run.
const MAX_VISITED: usize = 12;

/// The labor a harvest skill costs (the server's `consume_lp`; the bot only needs a floor).
pub fn labor_of(skill: u32) -> i16 {
    match skill {
        13980 => 1,
        13981 => 2,
        13982 => 3,
        13984 => 5,
        16774 => 10,
        20486 => 20,
        _ => 5,
    }
}

/// What the machine asks the glue to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Act {
    /// A line for the bot's log.
    Say(String),
    /// A GM command line.
    Gm(String),
    /// Walk to (x, y), stopping `radius` metres short.
    Travel { x: f32, y: f32, radius: f32 },
    /// Turn towards (x, y).
    Face { x: f32, y: f32 },
    /// Stop walking.
    StopMotion,
    /// `CSRequestCommonFarmList`.
    RequestList,
    /// `CSBuyItems` of `count` of `item` from the merchant `npc`.
    Buy { npc: u32, item: u32, count: i32 },
    /// `CSCreateDoodad` of `doodad` with the bag item `item_id` at `pos`.
    Plant {
        doodad: u32,
        pos: [f32; 3],
        z_rot: f32,
        item_id: u64,
    },
    /// `CSStartSkill` of the harvest `skill` on the crop `obj`.
    Harvest { skill: u32, obj: u32 },
    /// The plots changed: publish what is growing.
    Publish,
}

/// What the world offers the machine: spots, harvest skills.
pub trait Ops {
    fn managed_stock(&self, _doodad: u32) -> bool {
        false
    }

    /// A free spot (x, y, z) on `site` for a crop of `radius_mm`, claimed for this bot; `None`
    /// when the place has none.
    fn spot(&self, mem: &FarmMem, site: &Site, radius_mm: u16) -> Option<[f32; 3]>;
    /// The skill that harvests a crop of `doodad` in `phase`.
    fn harvest_skill(&self, doodad: u32, phase: u32) -> Option<u32>;
    /// The cast time of a skill in milliseconds.
    fn cast_ms(&self, skill: u32) -> u64;
    /// The crop's planting radius for a seed.
    fn radius_mm(&self, doodad: u32) -> u16;
    /// The labor planting one `seed` out in the wild costs (its use skill's).
    fn plant_labor(&self, seed: u32) -> i16;
}

/// What [`Machine::labor`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Labor {
    /// The bot has the points.
    Enough,
    /// A GM top-up was asked for; wait for it.
    Wait,
    /// Not enough, and no top-up to wait for.
    Short,
}

/// What the bot knows this tick.
pub struct Input<'a> {
    /// The bot clock in milliseconds.
    pub now_ms: u64,
    /// The bot's position.
    pub pos: [f32; 3],
    /// The bot's object id.
    pub me_obj: u32,
    /// GM commands are allowed.
    pub gm: bool,
    /// Harvested seeds are planted again on the spot.
    pub replant: bool,
    /// The bot's money in copper.
    pub money: i64,
    /// The bot's labor points.
    pub labor: i16,
    /// What the movement controller is doing.
    pub motion: MotionStatus,
    /// Recent refusals, newest last.
    pub errors: &'a [(u64, StateError)],
    /// How many of an item the bag holds.
    pub count: &'a dyn Fn(u32) -> i64,
    /// The bag item of a template: `(instance id, count)`.
    pub item: &'a dyn Fn(u32) -> Option<(u64, i64)>,
    /// How many of an item the ledger counts as the bot's own.
    pub clean: &'a dyn Fn(u32) -> u32,
    /// The merchant NPC of a template that is in view: `(object id, position)`.
    pub merchant: &'a dyn Fn(u32) -> Option<(u32, [f32; 3])>,
    /// The farthest the bot goes to a plot (metres).
    pub max_trip_m: f32,
    /// The world.
    pub ops: &'a dyn Ops,
}

/// Which job a run does.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Job {
    Plant(Plan),
    Reap,
}

/// The phase of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Ask for the crops on the public farms.
    Fetch,
    /// Walk to the vendor and buy seeds.
    Shop,
    /// Mint seeds with a GM command.
    Mint,
    /// Plant.
    Sow,
    /// Harvest.
    Reap,
}

/// Walking to a point in legs.
#[derive(Debug, Clone, Copy, Default)]
struct Walker {
    spot: Option<(f32, f32)>,
    legs: u8,
    leg_from: f32,
    go_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    Arrived,
    Moving,
    Failed,
}

impl Walker {
    /// Walks to (x, y), stopping `radius` metres short. The command is re-issued when something
    /// else took the controller over; a long way is walked in legs as long as every leg gets
    /// closer.
    fn walk(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, x: f32, y: f32, radius: f32) -> Walk {
        let here = dist2d(io.pos, [x, y, 0.0]);
        if here <= radius + 0.6 {
            acts.push(Act::StopMotion);
            self.spot = None;
            return Walk::Arrived;
        }
        let same = self
            .spot
            .is_some_and(|s| dist2d([s.0, s.1, 0.0], [x, y, 0.0]) < 2.0);
        match io.motion {
            MotionStatus::Moving if same => return Walk::Moving,
            MotionStatus::Failed(MoveFailure::Timeout)
                if same && self.legs < MAX_LEGS && self.leg_from - here >= LEG_MIN_GAIN_M =>
            {
                self.legs += 1;
                self.leg_from = here;
                self.go_ms = 0;
            }
            MotionStatus::Failed(_) if same => {
                self.spot = None;
                return Walk::Failed;
            }
            _ => {}
        }
        if same && io.now_ms.saturating_sub(self.go_ms) < GO_GAP_MS {
            return Walk::Moving;
        }
        if !same {
            self.legs = 0;
            self.leg_from = here;
        }
        acts.push(Act::Travel { x, y, radius });
        self.spot = Some((x, y));
        self.go_ms = io.now_ms;
        Walk::Moving
    }
}

/// Planting state.
#[derive(Debug, Clone, Copy)]
struct Sow {
    seed: u32,
    doodad: u32,
    radius_mm: u16,
    /// The place to draw spots from; `None` when `spot` is fixed (replanting).
    site: Option<Site>,
    land: u8,
    /// Seeds still to plant.
    left: u8,
    /// The spot being planted.
    spot: Option<[f32; 3]>,
    /// The spot is not to be changed (a harvested plot's place).
    fixed: bool,
    /// When `CSCreateDoodad` was sent, 0 before.
    sent_ms: u64,
    /// When the bot faced the spot, 0 before.
    faced_ms: u64,
    refusals: u8,
    /// Planted in this phase.
    done: u8,
}

/// Harvesting state.
#[derive(Debug, Clone, Copy, Default)]
struct Reap {
    /// The plot being harvested (its place).
    at: Option<[f32; 3]>,
    /// The crop's object id.
    obj: u32,
    /// The skill sent.
    skill: u32,
    /// When the bot stood next to the place and the crop was not in view, 0 before.
    unseen_ms: u64,
    /// When the harvest cast was last sent, 0 before.
    sent_ms: u64,
    casts: u8,
    /// The harvest worked; waiting for the crop to leave.
    gone_ms: u64,
    /// The seed and doodad of the harvested crop (for the replant).
    seed: u32,
    doodad: u32,
    land: u8,
    honest: bool,
}

/// One run. See the module docs.
#[derive(Debug)]
pub struct Machine {
    job: Job,
    phase: Phase,
    began_ms: u64,
    phase_ms: u64,
    walker: Walker,
    // shopping
    buys: u8,
    buy_sent_ms: u64,
    bag_before: i64,
    shop_target: i64,
    vendor_seen_ms: u64,
    // minting
    mint_sent: bool,
    // planting
    sow: Option<Sow>,
    // harvesting
    reap: Reap,
    visited: SmallVec<[[f32; 2]; 4]>,
    labor_asked: bool,
    labor_ms: u64,
    /// Seeds planted in this run.
    pub planted: u8,
    /// Crops harvested in this run.
    pub harvested: u8,
    /// A text for the web UI and the log: what the run is doing.
    note: &'static str,
}

impl Machine {
    /// A run that plants `plan`. `fetch`: ask for the public farm crops first.
    pub fn plant(plan: Plan, fetch: bool, now_ms: u64) -> Machine {
        let mut m = Machine::new(Job::Plant(plan), now_ms);
        m.phase = if fetch {
            Phase::Fetch
        } else {
            Machine::after_fetch(&plan)
        };
        m.sow = Some(Sow::of(&plan));
        m
    }

    /// A run that harvests the plots that are due. `fetch`: ask for the public farm crops first.
    pub fn harvest(fetch: bool, now_ms: u64) -> Machine {
        let mut m = Machine::new(Job::Reap, now_ms);
        m.phase = if fetch { Phase::Fetch } else { Phase::Reap };
        m
    }

    fn new(job: Job, now_ms: u64) -> Machine {
        Machine {
            job,
            phase: Phase::Fetch,
            began_ms: now_ms,
            phase_ms: now_ms,
            walker: Walker::default(),
            buys: 0,
            buy_sent_ms: 0,
            bag_before: 0,
            shop_target: 0,
            vendor_seen_ms: 0,
            mint_sent: false,
            sow: None,
            reap: Reap::default(),
            visited: SmallVec::new(),
            labor_asked: false,
            labor_ms: 0,
            planted: 0,
            harvested: 0,
            note: "starting",
        }
    }

    fn after_fetch(plan: &Plan) -> Phase {
        if plan.mint > 0 {
            Phase::Mint
        } else if plan.buy > 0 {
            Phase::Shop
        } else {
            Phase::Sow
        }
    }

    /// The phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// What the run is doing, in a few words.
    pub fn note(&self) -> &'static str {
        self.note
    }

    /// Whether the run plants (as opposed to harvests).
    pub fn plants(&self) -> bool {
        matches!(self.job, Job::Plant(_))
    }

    /// The plan of a planting run.
    pub fn plan(&self) -> Option<&Plan> {
        match &self.job {
            Job::Plant(p) => Some(p),
            Job::Reap => None,
        }
    }

    /// Whether the bot has `need` labor points; a GM bot asks for a top-up once per run and
    /// waits a few seconds for it.
    fn labor(&mut self, io: &Input<'_>, acts: &mut Vec<Act>, need: i16) -> Labor {
        if io.labor >= need {
            return Labor::Enough;
        }
        if io.gm && !self.labor_asked {
            self.labor_asked = true;
            self.labor_ms = io.now_ms;
            acts.push(Act::Gm(format!("/labor {}", LABOR_TOP_UP.max(need))));
            return Labor::Wait;
        }
        if io.gm && io.now_ms.saturating_sub(self.labor_ms) < 8_000 {
            return Labor::Wait;
        }
        Labor::Short
    }

    fn enter(&mut self, phase: Phase, io: &Input<'_>) {
        self.phase = phase;
        self.phase_ms = io.now_ms;
        self.walker = Walker::default();
    }

    /// The run is over: closes the harvest window.
    fn finish(&mut self, mem: &mut FarmMem, status: GoalStatus) -> GoalStatus {
        mem.harvest_honest = None;
        status
    }

    /// Called when the goal stops, whatever the reason.
    pub fn stop(&mut self, mem: &mut FarmMem, acts: &mut Vec<Act>) {
        mem.harvest_honest = None;
        acts.push(Act::StopMotion);
        acts.push(Act::Publish);
    }

    /// One tick.
    pub fn step(&mut self, io: &Input<'_>, mem: &mut FarmMem, acts: &mut Vec<Act>) -> GoalStatus {
        if io.now_ms.saturating_sub(self.began_ms) > RUN_LIMIT_MS {
            return self.finish(mem, GoalStatus::Failed(FailReason::Timeout));
        }
        let status = match self.phase {
            Phase::Fetch => self.fetch(io, mem, acts),
            Phase::Shop => self.shop(io, mem, acts),
            Phase::Mint => self.mint(io, acts),
            Phase::Sow => self.sow(io, mem, acts),
            Phase::Reap => self.reap(io, mem, acts),
        };
        if status != GoalStatus::Running {
            mem.harvest_honest = None;
        }
        status
    }

    // ---- fetch --------------------------------------------------------------------------

    fn fetch(&mut self, io: &Input<'_>, mem: &mut FarmMem, acts: &mut Vec<Act>) -> GoalStatus {
        self.note = "asking for the crops on the public farms";
        if !mem.list_asked {
            mem.list_asked = true;
            acts.push(Act::RequestList);
            self.phase_ms = io.now_ms;
            return GoalStatus::Running;
        }
        if io.now_ms.saturating_sub(self.phase_ms) < FETCH_WAIT_MS {
            return GoalStatus::Running;
        }
        let next = match &self.job {
            Job::Plant(p) => Machine::after_fetch(p),
            Job::Reap => Phase::Reap,
        };
        self.enter(next, io);
        GoalStatus::Running
    }

    // ---- shopping -----------------------------------------------------------------------

    fn shop(&mut self, io: &Input<'_>, mem: &mut FarmMem, acts: &mut Vec<Act>) -> GoalStatus {
        let Job::Plant(plan) = self.job else {
            return GoalStatus::Done;
        };
        self.note = "buying seeds";
        let have = (io.count)(plan.seed);
        if self.buys == 0 && self.shop_target == 0 {
            self.shop_target = have + i64::from(plan.buy);
        }
        let Some(vendor) = plan.vendor else {
            return self.fall_back_to_mint(io, mem, acts, "no vendor");
        };
        if have >= self.shop_target {
            self.enter(Phase::Sow, io);
            return GoalStatus::Running;
        }
        // something arrived since the last purchase: plant with what the bag holds now
        if self.buy_sent_ms != 0 && have > self.bag_before {
            if let Some(sow) = self.sow.as_mut() {
                sow.left = sow.left.min(u8::try_from(have).unwrap_or(u8::MAX));
            }
            self.enter(Phase::Sow, io);
            return GoalStatus::Running;
        }
        let missing = self.shop_target - have;
        if io.money < i64::from(plan.price.max(1)) {
            return self.fall_back_to_mint(io, mem, acts, "not enough money");
        }
        match (io.merchant)(vendor.npc) {
            Some((npc_obj, pos)) => {
                self.vendor_seen_ms = io.now_ms;
                let d = dist2d(io.pos, pos);
                if d > SHOP_REACH_M {
                    return match self.walker.walk(io, acts, pos[0], pos[1], SHOP_STOP_M) {
                        Walk::Failed => self.fall_back_to_mint(io, mem, acts, "vendor unreachable"),
                        _ => GoalStatus::Running,
                    };
                }
                acts.push(Act::StopMotion);
                acts.push(Act::Face {
                    x: pos[0],
                    y: pos[1],
                });
                if self.buy_sent_ms == 0
                    || io.now_ms.saturating_sub(self.buy_sent_ms) >= BUY_WAIT_MS
                {
                    if self.buys >= MAX_BUYS {
                        return self.fall_back_to_mint(io, mem, acts, "the vendor sold nothing");
                    }
                    let affordable = io.money / i64::from(plan.price.max(1));
                    let count = missing.min(affordable).clamp(1, 255);
                    acts.push(Act::Buy {
                        npc: npc_obj,
                        item: plan.seed,
                        count: i32::try_from(count).unwrap_or(1),
                    });
                    acts.push(Act::Say(format!(
                        "farm: buying {count} seed {} for {} copper each",
                        plan.seed, plan.price
                    )));
                    self.buys += 1;
                    self.buy_sent_ms = io.now_ms;
                    self.bag_before = have;
                }
                GoalStatus::Running
            }
            None => {
                // not in view: go to a spawn of the vendor and look
                let d = dist2d(io.pos, vendor.pos);
                if d <= SHOP_REACH_M + 4.0 {
                    if self.vendor_seen_ms == 0 {
                        self.vendor_seen_ms = io.now_ms;
                    }
                    if io.now_ms.saturating_sub(self.vendor_seen_ms) > VENDOR_VIEW_MS {
                        return self.fall_back_to_mint(io, mem, acts, "the vendor is not there");
                    }
                    acts.push(Act::StopMotion);
                    return GoalStatus::Running;
                }
                self.vendor_seen_ms = 0;
                match self
                    .walker
                    .walk(io, acts, vendor.pos[0], vendor.pos[1], SHOP_STOP_M + 2.0)
                {
                    Walk::Failed => self.fall_back_to_mint(io, mem, acts, "vendor unreachable"),
                    _ => GoalStatus::Running,
                }
            }
        }
    }

    /// The vendor did not work out: mint the seeds when a GM may, else give up.
    fn fall_back_to_mint(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        why: &'static str,
    ) -> GoalStatus {
        let Job::Plant(plan) = &mut self.job else {
            return GoalStatus::Done;
        };
        if io.gm && plan.mint == 0 {
            plan.mint = plan.want.clamp(1, 3);
            acts.push(Act::Say(format!("farm: {why}; minting seeds instead")));
            self.mint_sent = false;
            self.enter(Phase::Mint, io);
            return GoalStatus::Running;
        }
        self.finish(mem, GoalStatus::Failed(FailReason::Other(why)))
    }

    // ---- minting ------------------------------------------------------------------------

    fn mint(&mut self, io: &Input<'_>, acts: &mut Vec<Act>) -> GoalStatus {
        let Job::Plant(plan) = self.job else {
            return GoalStatus::Done;
        };
        self.note = "minting seeds";
        if !io.gm {
            return GoalStatus::Failed(FailReason::Other("seeds need a GM"));
        }
        let have = (io.count)(plan.seed);
        if !self.mint_sent {
            self.mint_sent = true;
            self.bag_before = have;
            self.phase_ms = io.now_ms;
            acts.push(Act::Gm(format!(
                "/item add self {} {}",
                plan.seed, plan.mint
            )));
            return GoalStatus::Running;
        }
        if have > self.bag_before {
            if let Some(sow) = self.sow.as_mut() {
                sow.left = sow.left.min(u8::try_from(have).unwrap_or(u8::MAX));
            }
            self.enter(Phase::Sow, io);
            return GoalStatus::Running;
        }
        if io.now_ms.saturating_sub(self.phase_ms) > MINT_WAIT_MS {
            return GoalStatus::Failed(FailReason::Other("the minted seeds did not arrive"));
        }
        GoalStatus::Running
    }

    // ---- planting -----------------------------------------------------------------------

    fn sow(&mut self, io: &Input<'_>, mem: &mut FarmMem, acts: &mut Vec<Act>) -> GoalStatus {
        let Some(mut sow) = self.sow else {
            return GoalStatus::Done;
        };
        let status = self.sow_step(io, mem, acts, &mut sow);
        self.sow = Some(sow);
        status
    }

    fn sow_step(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        sow: &mut Sow,
    ) -> GoalStatus {
        self.note = "planting";
        if sow.sent_ms != 0 {
            return self.sow_wait(io, mem, acts, sow);
        }
        if sow.left == 0 {
            return self.sow_end(io, mem, acts, sow, None);
        }
        let Some((item_id, have)) = (io.item)(sow.seed).filter(|(_, n)| *n > 0) else {
            // the bag has no seed: late news of a purchase, or none at all
            if io.now_ms.saturating_sub(self.phase_ms) < 3_000 {
                return GoalStatus::Running;
            }
            return self.sow_end(io, mem, acts, sow, Some(FailReason::Other("no seeds")));
        };
        let _ = have;
        let spot = match sow.spot {
            Some(s) => s,
            None => {
                let Some(site) = sow.site else {
                    return self.sow_end(io, mem, acts, sow, Some(FailReason::NoTarget));
                };
                match io.ops.spot(mem, &site, sow.radius_mm) {
                    Some(s) => {
                        sow.spot = Some(s);
                        s
                    }
                    None => {
                        return self.sow_end(
                            io,
                            mem,
                            acts,
                            sow,
                            Some(FailReason::Other("no free spot")),
                        )
                    }
                }
            }
        };
        match self.walker.walk(io, acts, spot[0], spot[1], STAND_M) {
            Walk::Failed => {
                mem.mark_bad_spot(spot[0], spot[1], io.now_ms);
                sow.spot = None;
                sow.refusals += 1;
                if sow.fixed || sow.refusals >= MAX_REFUSALS {
                    return self.sow_end(io, mem, acts, sow, Some(FailReason::Stuck));
                }
                GoalStatus::Running
            }
            Walk::Moving => GoalStatus::Running,
            Walk::Arrived => {
                if sow.faced_ms == 0 {
                    acts.push(Act::Face {
                        x: spot[0],
                        y: spot[1],
                    });
                    sow.faced_ms = io.now_ms;
                    return GoalStatus::Running;
                }
                if io.now_ms.saturating_sub(sow.faced_ms) < FACE_WAIT_MS {
                    return GoalStatus::Running;
                }
                // a seed out in the wild costs the labor of its use skill
                if sow.land == WILD_LAND {
                    let need = io.ops.plant_labor(sow.seed);
                    match self.labor(io, acts, need) {
                        Labor::Enough => {}
                        Labor::Wait => return GoalStatus::Running,
                        Labor::Short => {
                            acts.push(Act::Say(format!(
                                "farm: {} labor points are not enough to plant out in the wild \
                                 (needs {need})",
                                io.labor
                            )));
                            return self.sow_end(
                                io,
                                mem,
                                acts,
                                sow,
                                Some(FailReason::Other("not enough labor")),
                            );
                        }
                    }
                }
                let honest = (io.clean)(sow.seed) >= 1;
                acts.push(Act::Plant {
                    doodad: sow.doodad,
                    pos: spot,
                    z_rot: 0.0,
                    item_id,
                });
                acts.push(Act::Say(format!(
                    "farm: planting doodad {} at ({:.0}, {:.0}){}",
                    sow.doodad,
                    spot[0],
                    spot[1],
                    if honest { "" } else { " (not clean)" }
                )));
                sow.sent_ms = io.now_ms;
                sow.faced_ms = 0;
                // the plot is noted at once; the crop's own announcement gives it its object id
                mem.plots
                    .retain(|p| !(p.obj == 0 && dist2d(p.pos, spot) < SAME_SPOT_M));
                mem.plant(Plot {
                    obj: 0,
                    doodad: sow.doodad,
                    seed: sow.seed,
                    pos: spot,
                    phase: 0,
                    ripe: false,
                    check_ms: 0,
                    planted_ms: io.now_ms,
                    honest,
                    land: sow.land,
                });
                GoalStatus::Running
            }
        }
    }

    /// Waits for the crop to appear or the server to refuse.
    fn sow_wait(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        sow: &mut Sow,
    ) -> GoalStatus {
        let Some(spot) = sow.spot else {
            sow.sent_ms = 0;
            return GoalStatus::Running;
        };
        let known = mem
            .plots
            .iter()
            .position(|p| dist2d(p.pos, spot) < SAME_SPOT_M && p.planted_ms == sow.sent_ms);
        // a crop that has an object id is the one that was planted
        if let Some(i) = known {
            if mem.plots[i].obj != 0 {
                let p = &mut mem.plots[i];
                let crop_ms = self.plan().map_or(0, |pl| pl.grow_ms);
                p.check_ms = if crop_ms > 0 {
                    sow.sent_ms + crop_ms + 3_000
                } else {
                    sow.sent_ms + 5 * 60_000
                };
                self.planted += 1;
                sow.done += 1;
                sow.left = sow.left.saturating_sub(1);
                sow.sent_ms = 0;
                sow.spot = None;
                acts.push(Act::Publish);
                return GoalStatus::Running;
            }
        }
        // the server said no
        let refused = io.errors.iter().rev().find_map(|(at, e)| match e {
            StateError::Msg(code) if *at >= sow.sent_ms => Some(*code),
            _ => None,
        });
        if let Some(code) = refused {
            if let Some(i) = known {
                mem.drop_plot(i);
            }
            sow.sent_ms = 0;
            return match code {
                refusal::FARM_COUNT | refusal::FARM_TYPE => {
                    acts.push(Act::Say(format!(
                        "farm: the server says the farm is full ({code})"
                    )));
                    self.sow_end(io, mem, acts, sow, Some(FailReason::Other("farm is full")))
                }
                refusal::NO_LABOR => self.sow_end(
                    io,
                    mem,
                    acts,
                    sow,
                    Some(FailReason::Other("not enough labor")),
                ),
                refusal::AREA
                | refusal::NO_PERM
                | refusal::TOO_CLOSE_DOODAD
                | refusal::TOO_CLOSE_UNIT
                | refusal::TOO_FAR => {
                    acts.push(Act::Say(format!(
                        "farm: spot ({:.0}, {:.0}) refused ({code})",
                        spot[0], spot[1]
                    )));
                    self.spot_refused(io, mem, acts, sow, spot)
                }
                other => {
                    acts.push(Act::Say(format!(
                        "farm: seed {} refused ({other})",
                        sow.seed
                    )));
                    mem.mark_bad_seed(sow.seed, io.now_ms);
                    self.sow_end(io, mem, acts, sow, Some(FailReason::Other("seed refused")))
                }
            };
        }
        if io.now_ms.saturating_sub(sow.sent_ms) > PLANT_WAIT_MS {
            if let Some(i) = known {
                mem.drop_plot(i);
            }
            sow.sent_ms = 0;
            acts.push(Act::Say("farm: the server planted nothing".into()));
            return self.spot_refused(io, mem, acts, sow, spot);
        }
        GoalStatus::Running
    }

    fn spot_refused(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        sow: &mut Sow,
        spot: [f32; 3],
    ) -> GoalStatus {
        mem.mark_bad_spot(spot[0], spot[1], io.now_ms);
        sow.refusals += 1;
        sow.spot = None;
        if sow.fixed || sow.refusals >= MAX_REFUSALS {
            return self.sow_end(
                io,
                mem,
                acts,
                sow,
                Some(FailReason::Other("no spot accepted")),
            );
        }
        GoalStatus::Running
    }

    /// Planting ends: a planting run is over (done when it planted something), a replant goes back
    /// to harvesting.
    fn sow_end(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        sow: &mut Sow,
        failed: Option<FailReason>,
    ) -> GoalStatus {
        acts.push(Act::StopMotion);
        acts.push(Act::Publish);
        if matches!(self.job, Job::Reap) {
            self.sow = None;
            self.reap = Reap::default();
            self.enter(Phase::Reap, io);
            return GoalStatus::Running;
        }
        let status = match failed {
            Some(reason) if sow.done == 0 => GoalStatus::Failed(reason),
            _ => GoalStatus::Done,
        };
        self.finish(mem, status)
    }

    // ---- harvesting ---------------------------------------------------------------------

    fn next_target(&self, io: &Input<'_>, mem: &FarmMem) -> Option<Plot> {
        mem.plots
            .iter()
            .filter(|p| p.due(io.now_ms))
            .filter(|p| !io.ops.managed_stock(p.doodad))
            .filter(|p| {
                !self
                    .visited
                    .iter()
                    .any(|v| dist2d(p.pos, [v[0], v[1], 0.0]) < SAME_SPOT_M)
            })
            .filter(|p| dist2d(io.pos, p.pos) <= io.max_trip_m)
            .min_by(|a, b| {
                dist2d(io.pos, a.pos)
                    .total_cmp(&dist2d(io.pos, b.pos))
                    .then(a.pos[0].total_cmp(&b.pos[0]))
            })
            .copied()
    }

    fn visit(&mut self, pos: [f32; 3]) {
        if self.visited.len() >= MAX_VISITED {
            self.visited.remove(0);
        }
        self.visited.push([pos[0], pos[1]]);
    }

    fn reap(&mut self, io: &Input<'_>, mem: &mut FarmMem, acts: &mut Vec<Act>) -> GoalStatus {
        self.note = "harvesting";
        let Some(at) = self.reap.at else {
            let Some(p) = self.next_target(io, mem) else {
                acts.push(Act::Publish);
                return self.finish(mem, GoalStatus::Done);
            };
            self.reap = Reap {
                at: Some(p.pos),
                obj: p.obj,
                seed: p.seed,
                doodad: p.doodad,
                land: p.land,
                honest: p.honest,
                ..Reap::default()
            };
            self.walker = Walker::default();
            return GoalStatus::Running;
        };
        // the plot as the memory has it now
        let idx = mem
            .plots
            .iter()
            .position(|p| dist2d(p.pos, at) < SAME_SPOT_M && p.doodad == self.reap.doodad);
        if self.reap.sent_ms != 0 {
            return self.reap_wait(io, mem, acts, idx);
        }
        let Some(i) = idx else {
            // the plot is gone from the memory (vanished in view)
            self.visit(at);
            self.reap = Reap::default();
            return GoalStatus::Running;
        };
        let plot = mem.plots[i];
        match self.walker.walk(io, acts, at[0], at[1], REAP_STOP_M) {
            Walk::Failed => {
                mem.plots[i].check_ms = io.now_ms + UNREACHABLE_MS;
                mem.plots[i].ripe = false;
                acts.push(Act::Say(format!(
                    "farm: cannot reach the crop at ({:.0}, {:.0})",
                    at[0], at[1]
                )));
                self.visit(at);
                self.reap = Reap::default();
                GoalStatus::Running
            }
            Walk::Moving => GoalStatus::Running,
            Walk::Arrived => {
                if plot.obj == 0 {
                    if self.reap.unseen_ms == 0 {
                        self.reap.unseen_ms = io.now_ms;
                    }
                    if io.now_ms.saturating_sub(self.reap.unseen_ms) > VIEW_WAIT_MS {
                        // the bot stands at the place and sees no crop: it is gone
                        acts.push(Act::Say(format!(
                            "farm: no crop at ({:.0}, {:.0}) any more{}",
                            at[0],
                            at[1],
                            if plot.land == WILD_LAND {
                                " (somebody took it?)"
                            } else {
                                ""
                            }
                        )));
                        mem.lose_plot(i);
                        acts.push(Act::Publish);
                        self.reap = Reap::default();
                    }
                    return GoalStatus::Running;
                }
                if !plot.ripe {
                    // not ripe yet: look again later
                    mem.plots[i].check_ms = io.now_ms + REVISIT_MS;
                    self.visit(at);
                    self.reap = Reap::default();
                    return GoalStatus::Running;
                }
                self.reap.obj = plot.obj;
                self.reap.honest = plot.honest;
                self.cast(io, mem, acts, i)
            }
        }
    }

    /// Starts the harvest cast on plot `i`.
    fn cast(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        i: usize,
    ) -> GoalStatus {
        let plot = mem.plots[i];
        let Some(skill) = io.ops.harvest_skill(plot.doodad, plot.phase) else {
            mem.plots[i].check_ms = io.now_ms + UNREACHABLE_MS;
            mem.plots[i].ripe = false;
            self.visit(plot.pos);
            self.reap = Reap::default();
            return GoalStatus::Running;
        };
        let need = labor_of(skill);
        match self.labor(io, acts, need) {
            Labor::Enough => {}
            Labor::Wait => return GoalStatus::Running,
            Labor::Short => {
                acts.push(Act::Say(format!(
                    "farm: {} labor points are not enough for the harvest (needs {need})",
                    io.labor
                )));
                return self.finish(
                    mem,
                    GoalStatus::Failed(FailReason::Other("not enough labor")),
                );
            }
        }
        acts.push(Act::StopMotion);
        acts.push(Act::Face {
            x: plot.pos[0],
            y: plot.pos[1],
        });
        if self.reap.casts >= MAX_CASTS {
            mem.plots[i].check_ms = io.now_ms + REVISIT_MS;
            acts.push(Act::Say(format!(
                "farm: the harvest of the crop at ({:.0}, {:.0}) does not start",
                plot.pos[0], plot.pos[1]
            )));
            self.visit(plot.pos);
            self.reap = Reap::default();
            return GoalStatus::Running;
        }
        if dist2d(io.pos, plot.pos) > REAP_REACH_M {
            self.walker = Walker::default();
            return GoalStatus::Running;
        }
        mem.harvest_honest = Some(plot.honest);
        acts.push(Act::Harvest {
            skill,
            obj: plot.obj,
        });
        acts.push(Act::Say(format!(
            "farm: harvesting doodad {} (skill {skill}){}",
            plot.doodad,
            if plot.honest { "" } else { " (not clean)" }
        )));
        self.reap.skill = skill;
        self.reap.sent_ms = io.now_ms;
        self.reap.casts += 1;
        GoalStatus::Running
    }

    /// Waits for the harvested crop to change, then for it to leave, then replants.
    fn reap_wait(
        &mut self,
        io: &Input<'_>,
        mem: &mut FarmMem,
        acts: &mut Vec<Act>,
        idx: Option<usize>,
    ) -> GoalStatus {
        let at = self.reap.at.unwrap_or(io.pos);
        // 1. the harvest worked: the crop is not ripe any more or has left the world
        if self.reap.gone_ms == 0 {
            let worked = match idx {
                None => true,
                Some(i) => !mem.plots[i].ripe,
            };
            if worked {
                mem.expect_removal(self.reap.obj);
                self.reap.gone_ms = io.now_ms;
                self.harvested += 1;
                mem.harvested += 1;
                mem.last_crop = self.reap.doodad;
                mem.harvest_honest = None;
                acts.push(Act::Say(format!(
                    "farm: harvested the crop at ({:.0}, {:.0})",
                    at[0], at[1]
                )));
                return GoalStatus::Running;
            }
            let cast = io.ops.cast_ms(self.reap.skill);
            let waited = io.now_ms.saturating_sub(self.reap.sent_ms);
            if waited > cast + REAP_EXTRA_MS {
                // no change: cast again (or give the crop up after MAX_CASTS)
                self.reap.sent_ms = 0;
                mem.harvest_honest = None;
            } else if waited > cast + CAST_GAP_MS {
                // a refusal of the skill ends the wait early
                let refused = io.errors.iter().rev().any(|(t, e)| {
                    *t >= self.reap.sent_ms
                        && matches!(e, StateError::Skill { skill, result } if *skill == self.reap.skill && *result != 0)
                });
                if refused {
                    self.reap.sent_ms = 0;
                    mem.harvest_honest = None;
                }
            }
            return GoalStatus::Running;
        }
        // 2. the crop leaves the world; then its place is planted again
        if let Some(i) = idx {
            if io.now_ms.saturating_sub(self.reap.gone_ms) < GONE_WAIT_MS {
                return GoalStatus::Running;
            }
            // it is still there (a crop that grows again): the memory keeps it
            mem.plots[i].ripe = false;
            if mem.plots[i].check_ms <= io.now_ms {
                mem.plots[i].check_ms = io.now_ms + REVISIT_MS;
            }
        }
        acts.push(Act::Publish);
        let r = self.reap;
        self.visit(at);
        self.reap = Reap::default();
        // (a stolen crop's place is not the bot's to plant again)
        if idx.is_none() && io.replant && r.land != STOLEN_LAND {
            if let Some((_, have)) = (io.item)(r.seed).filter(|(_, n)| *n > 0) {
                let _ = have;
                self.sow = Some(Sow {
                    seed: r.seed,
                    doodad: r.doodad,
                    radius_mm: io.ops.radius_mm(r.doodad),
                    site: None,
                    land: r.land,
                    left: 1,
                    spot: Some(at),
                    fixed: true,
                    sent_ms: 0,
                    faced_ms: 0,
                    refusals: 0,
                    done: 0,
                });
                self.enter(Phase::Sow, io);
            }
        }
        GoalStatus::Running
    }
}

impl Sow {
    fn of(plan: &Plan) -> Sow {
        Sow {
            seed: plan.seed,
            doodad: plan.doodad,
            radius_mm: plan.radius_mm,
            site: Some(plan.site),
            land: plan.site.land,
            left: plan.want,
            spot: None,
            fixed: false,
            sent_ms: 0,
            faced_ms: 0,
            refusals: 0,
            done: 0,
        }
    }
}
