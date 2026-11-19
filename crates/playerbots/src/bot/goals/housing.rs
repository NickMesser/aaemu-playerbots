//! The housing goal: get a house, and later go home.
//!
//! # Flow
//!
//! ```text
//!  Init ─ owns a house? ─► Build (unfinished, GM) ─► Home (walk there, stay a while)
//!    └──► Plan ─► Spots ─► Travel ─► Quote ─► Provision ─► Create ─► Wait ─► Built ─► Build ─► Done
//!                  ▲                                          │ refused (area, overlap, ...)
//!                  └──────────────── next spot / next area ◄──┘
//! ```
//!
//! * **Plan** picks a design (the design item already in the bag, else with `bots.gm_bootstrap`
//!   the Scarecrow Garden, then the cottage) and ranks the housing areas that allow its category
//!   by distance from the bot ([`spots::rank_areas`]).
//! * **Spots** samples candidate positions inside an area ([`spots::sample_spots`], which repeats
//!   the server's footprint rule), away from the NPCs and doodads in view.
//! * **Quote** sends `CSConstructHouseTax` and waits for `SCConstructHouseTax`; **Provision** gives
//!   the bot the design item and the tax certificates (one per 10 000 copper of the quoted total,
//!   rounded up, plus a margin) with `/item add self`; **Create** sends `CSCreateHouse`.
//! * A refusal (`SCErrorMsg` with a housing code) marks the spot bad and tries the next one, then
//!   the next area, up to [`MAX_TRIES`] spots.
//! * **Build** targets the house and uses the GM command `/build <n>` until the house is finished
//!   (the batch size shrinks when the server does not advance).
//!
//! Without `gm_bootstrap` the goal only works when the bag already holds a design item and the
//! tax certificates (otherwise it fails with a visible reason); a bot that owns a house goes home
//! and rests there.

mod log;
pub mod spots;

use std::fmt::Write as _;

use aa_proto::cs::{self, CreateHouse};
use aa_world::HousingDesign;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;

use crate::bot::ctx::Ctx;
use crate::bot::motion::{MotionStatus, Pace, SWIM_THRESHOLD};
use crate::bot::state::{dist2d, HouseBrief, Item, StateError};

use super::{jitter_ms, FailReason, GoalStatus};

pub use log::{HouseLog, Quote};
use spots::{cells_for_radius, rank_areas, sample_spots, side, SampleOpts};

/// Tax certificate item and its bound twin.
pub const TAX_CERTIFICATE: u32 = 31_891;
const BOUND_TAX_CERTIFICATE: u32 = 31_892;
/// Copper one tax certificate pays.
const CERTIFICATE_COPPER: i64 = 10_000;
/// Certificates minted beyond the quote.
const CERT_MARGIN: i64 = 2;
/// The designs a bot builds first (Scarecrow Garden, then the Nuian cottage).
pub const PREFERRED: [u32; 2] = [267, 172];
/// Spots the server may refuse before the goal gives up.
pub const MAX_TRIES: u8 = 8;
/// Housing areas looked at per plan.
const AREAS: usize = 6;
/// Trips longer than this are not walked when the bot may not teleport.
const MAX_WALK_M: f32 = 450.0;
/// The whole goal (not the stay at home) takes at most this long.
const TOTAL_MS: u64 = 15 * 60_000;
/// The server answers `CSConstructHouseTax` and `CSCreateHouse` within this.
const ANSWER_MS: u64 = 8_000;
/// Items created with `/item add` show in the bag within this.
const MINT_WAIT_MS: u64 = 7_000;
/// The finishing of the house takes at most this long.
const BUILD_MAX_MS: u64 = 150_000;
/// Pause between two `/build` commands.
const BUILD_EVERY_MS: u64 = 2_200;
/// Largest slope (metres across the footprint) a candidate may have.
const MAX_SLOPE_M: f32 = 4.0;
/// Stay at home this long (min, max in ms).
const HOME_STAY_MS: (u64, u64) = (20_000, 45_000);

/// `SCErrorMsg` ids of the housing placement checks and payments (`economy/error_message.rs`).
mod err {
    pub const BAG_INVALID_ITEM: i16 = 181;
    pub const HOUSE_CANNOT_CREATE_LACK_MONEY: i16 = 120;
    pub const INVALID_TAXATION: i16 = 473;
    pub const MAIL_NOT_ENOUGH_MONEY_TO_PAY_TAXES: i16 = 481;
    pub const HOUSE_CANNOT_CREATE_CONSTRUCT_TAX_ABNORMAL: i16 = 607;
    /// The first and last id of the `HOUSE_CANNOT_LOCATE_*` family.
    pub const LOCATE: std::ops::RangeInclusive<i16> = 112..=118;
    pub const LOCATE_NOT_DOMINATED_ZONE: i16 = 340;
    pub const LOCATE_NEAR_OTHERS_GUARD_HOUSE: i16 = 341;
    pub const LOCATE_INVALID_CATEGORY_AREA: i16 = 580;
    pub const INVALID_HOUSE_INFO: i16 = 614;
    pub const OWN_MORE_HOUSELESS_CONDITION: i16 = 615;
    pub const OWN_MORE_EXISTING_CATEGORY_CONDITION: i16 = 616;
    pub const MAX_CONSTRUCT_COUNT: i16 = 766;
}

/// What a refused `CSCreateHouse` means for the next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The spot is no good: try another one.
    BadSpot,
    /// The bot lacks the design item, the certificates or the money: provision again.
    Payment,
    /// The account may not build this kind of house at all.
    Rules,
}

/// Classifies a `SCErrorMsg` id as an answer to `CSCreateHouse`; `None` for unrelated messages.
pub fn classify_error(code: i16) -> Option<Refusal> {
    use err::*;
    match code {
        c if LOCATE.contains(&c) => Some(Refusal::BadSpot),
        LOCATE_NOT_DOMINATED_ZONE
        | LOCATE_NEAR_OTHERS_GUARD_HOUSE
        | LOCATE_INVALID_CATEGORY_AREA => Some(Refusal::BadSpot),
        INVALID_HOUSE_INFO | MAX_CONSTRUCT_COUNT => Some(Refusal::BadSpot),
        OWN_MORE_HOUSELESS_CONDITION | OWN_MORE_EXISTING_CATEGORY_CONDITION => Some(Refusal::Rules),
        BAG_INVALID_ITEM
        | HOUSE_CANNOT_CREATE_LACK_MONEY
        | INVALID_TAXATION
        | MAIL_NOT_ENOUGH_MONEY_TO_PAY_TAXES
        | HOUSE_CANNOT_CREATE_CONSTRUCT_TAX_ABNORMAL => Some(Refusal::Payment),
        _ => None,
    }
}

/// How the server answered a `CSCreateHouse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Nothing yet.
    Waiting,
    /// A new house appeared in the bot's list.
    Built,
    /// An error message about the placement arrived after the request.
    Refused(i16, Refusal),
    /// Neither a house nor an error within [`ANSWER_MS`].
    Silent,
}

/// Reads the answer to a `CSCreateHouse` sent at `sent_ms` from the bot's knowledge: the number of
/// houses now and before, and the recent error messages.
pub fn read_answer(
    houses: usize,
    before: usize,
    errors: &[(u64, StateError)],
    sent_ms: u64,
    now: u64,
) -> Answer {
    if houses > before {
        return Answer::Built;
    }
    let refused = errors.iter().find_map(|(t, e)| match e {
        StateError::Msg(code) if *t >= sent_ms => classify_error(*code).map(|r| (*code, r)),
        _ => None,
    });
    if let Some((code, why)) = refused {
        return Answer::Refused(code, why);
    }
    if now.saturating_sub(sent_ms) > ANSWER_MS {
        Answer::Silent
    } else {
        Answer::Waiting
    }
}

/// Garden radius of a design in metres: `housings.garden_radius` from the data (4.0 for the
/// Scarecrow Garden, 7.5 for the Nuian cottage). A design whose data carries none is guessed from
/// its category.
pub fn garden_radius(d: &HousingDesign) -> f32 {
    if d.garden_radius > 0.0 {
        return d.garden_radius;
    }
    match d.category {
        16 => 4.0,
        1 => 7.5,
        _ => 9.0,
    }
}

/// The design to build: a design item already in the bag first (standard designs before special
/// ones), else, with GM help, the first of [`PREFERRED`] (the cottage first when `cottage_first`:
/// every third bot builds one, so the map shows more than gardens), else the cheapest standard
/// design.
pub fn pick_design<'a>(
    designs: &'a [HousingDesign],
    bag: &FxHashMap<u16, Item>,
    gm: bool,
    cottage_first: bool,
) -> Option<&'a HousingDesign> {
    let held = |d: &&HousingDesign| bag.values().any(|i| i.template == d.item && i.count > 0);
    if let Some(d) = designs
        .iter()
        .filter(held)
        .find(|d| d.standard)
        .or_else(|| designs.iter().find(held))
    {
        return Some(d);
    }
    if !gm {
        return None;
    }
    let mut order = PREFERRED;
    if cottage_first {
        order.reverse();
    }
    order
        .iter()
        .find_map(|id| designs.iter().find(|d| d.design == *id))
        .or_else(|| designs.iter().find(|d| d.standard))
}

/// Tax certificates a quote of `total` copper needs (the server rounds up).
pub fn certificates_for(total: i32) -> i64 {
    (i64::from(total.max(0)) + CERTIFICATE_COPPER - 1) / CERTIFICATE_COPPER
}

pub(crate) fn certificates_held(ctx: &Ctx<'_>) -> i64 {
    ctx.state.item_count(TAX_CERTIFICATE) + ctx.state.item_count(BOUND_TAX_CERTIFICATE)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Decide between building and going home.
    #[default]
    Init,
    /// Choose the design and the areas.
    Plan,
    /// Sample the next spot.
    Spots,
    /// Walk to the spot.
    Travel,
    /// Ask for the tax.
    Quote,
    /// Get the design item and the tax certificates.
    Provision,
    /// Send `CSCreateHouse` and wait for the answer.
    Create,
    /// Finish the construction with `/build`.
    Build,
    /// Walk to the house.
    Home,
    /// Stay at the house.
    Linger,
}

/// What the plan holds while the goal builds a house.
#[derive(Debug, Clone)]
struct Plan {
    design: u32,
    item: u32,
    cells: i32,
    name: Box<str>,
    /// Housing area ids still to try, nearest first.
    areas: Vec<u32>,
    /// Candidate spots of the current area, best first.
    spots: Vec<[f32; 2]>,
    /// Spots the server refused (and where the bot found a unit in the way).
    bad: SmallVec<[[f32; 2]; 8]>,
    /// The spot being tried.
    spot: [f32; 3],
    /// Copper the server quoted (`None` before the answer).
    total: Option<i32>,
    /// Bot clock of the last request to the server.
    sent_ms: u64,
    /// Bot clock of the last `/item add`.
    mint_ms: u64,
    mint_tries: u8,
    quote_tries: u8,
    pay_retries: u8,
    /// Number of houses the bot owned when `CSCreateHouse` went out.
    houses_before: usize,
}

/// Build and use a house. See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Housing {
    phase: Phase,
    started_ms: u64,
    phase_ms: u64,
    tries: u8,
    plan: Option<Box<Plan>>,
    /// Transport id of the house being finished or visited.
    tl: u16,
    build_n: u32,
    build_ms: u64,
    build_left: i32,
    build_started_ms: u64,
    stay_ms: u64,
    moved: bool,
    last_x: f32,
    last_y: f32,
}

impl Housing {
    /// Called once when the brain picks the goal.
    pub fn start(&mut self, ctx: &mut Ctx<'_>) {
        *self = Housing::default();
        self.started_ms = ctx.now_ms;
        self.phase_ms = ctx.now_ms;
    }

    /// One tick.
    pub fn tick(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        if self.phase != Phase::Linger
            && self.phase != Phase::Home
            && now.saturating_sub(self.started_ms) > TOTAL_MS
        {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        match self.phase {
            Phase::Init => self.init(ctx),
            Phase::Plan => self.plan(ctx),
            Phase::Spots => self.next_spot(ctx),
            Phase::Travel => self.travel(ctx),
            Phase::Quote => self.quote(ctx),
            Phase::Provision => self.provision(ctx),
            Phase::Create => self.create(ctx),
            Phase::Build => self.build(ctx),
            Phase::Home => self.home(ctx),
            Phase::Linger => {
                if now >= self.stay_ms {
                    GoalStatus::Done
                } else {
                    GoalStatus::Running
                }
            }
        }
    }

    /// Called when the goal ends.
    pub fn stop(&mut self, ctx: &mut Ctx<'_>) {
        if ctx.motion.is_moving() {
            ctx.motion.stop();
        }
        self.plan = None;
    }

    /// Appends a one-line description of what the goal is doing to `out`.
    pub fn describe(&self, out: &mut String) {
        match self.phase {
            Phase::Init | Phase::Plan => out.push_str("planning a house"),
            Phase::Spots => out.push_str("looking for a place to build"),
            Phase::Travel => {
                if let Some(p) = &self.plan {
                    let _ = write!(
                        out,
                        "walking to {:.0}, {:.0} to build a {}",
                        p.spot[0], p.spot[1], p.name
                    );
                } else {
                    out.push_str("walking to a building plot");
                }
            }
            Phase::Quote => out.push_str("asking for the building tax"),
            Phase::Provision => out.push_str("getting the design and tax certificates"),
            Phase::Create => {
                let _ = write!(out, "placing a house (try {})", self.tries + 1);
            }
            Phase::Build => {
                let _ = write!(out, "building the house ({} steps left)", self.build_left);
            }
            Phase::Home => out.push_str("going home"),
            Phase::Linger => out.push_str("resting at home"),
        }
    }

    // ---- phases --------------------------------------------------------------------------

    fn init(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        if let Some(h) = ctx.state.houses.first().copied() {
            self.tl = h.tl;
            if h.build_left > 0 && ctx.gm_allowed() {
                ctx.log.say(format!(
                    "housing: finishing the house ({} steps left)",
                    h.build_left
                ));
                self.enter_build(ctx.now_ms, h.build_left);
            } else {
                self.phase = Phase::Home;
            }
            return GoalStatus::Running;
        }
        self.phase = Phase::Plan;
        GoalStatus::Running
    }

    fn plan(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let gm = ctx.gm_allowed();
        let cottage_first = ctx.persona.id.is_multiple_of(3);
        let Some(d) = pick_design(
            ctx.world.housing_designs(),
            &ctx.state.bag,
            gm,
            cottage_first,
        ) else {
            ctx.log.say(
                "housing: needs bots.gm_bootstrap (or a design item and tax certificates in the bag)",
            );
            return GoalStatus::Failed(FailReason::Other(
                "housing needs gm_bootstrap or a design item",
            ));
        };
        let cells = cells_for_radius(garden_radius(d));
        let me = ctx.pos();
        let max_dist = if ctx.would_teleport(MAX_WALK_M + 1.0) {
            f32::INFINITY
        } else {
            MAX_WALK_M
        };
        let areas = rank_areas(
            &ctx.world.baked().housing_areas,
            |id| ctx.world.housing_area_allows(id, d.category),
            [me[0], me[1]],
            max_dist,
            side(cells) * 2.0,
            AREAS,
        );
        if areas.is_empty() {
            ctx.log.say(format!(
                "housing: no housing area for a {} within reach",
                d.name
            ));
            return GoalStatus::Failed(FailReason::Other("no housing area in reach"));
        }
        ctx.log.say(format!(
            "housing: planning a {} (design {}, category {}), {} candidate areas",
            d.name,
            d.design,
            d.category,
            areas.len()
        ));
        self.plan = Some(Box::new(Plan {
            design: d.design,
            item: d.item,
            cells,
            name: d.name.clone(),
            areas,
            spots: Vec::new(),
            bad: SmallVec::new(),
            spot: [0.0; 3],
            total: None,
            sent_ms: 0,
            mint_ms: 0,
            mint_tries: 0,
            quote_tries: 0,
            pay_retries: 0,
            houses_before: 0,
        }));
        self.phase = Phase::Spots;
        GoalStatus::Running
    }

    fn next_spot(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(plan) = self.plan.as_deref_mut() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        // refill from the next area when the current one is used up
        if plan.spots.is_empty() {
            if plan.areas.is_empty() {
                return GoalStatus::Failed(FailReason::Other("no free spot in any housing area"));
            }
            let id = plan.areas.remove(0);
            let Some(poly) = ctx.world.baked().housing_areas.iter().find(|p| p.id == id) else {
                return GoalStatus::Running;
            };
            let me = ctx.state.me.pos;
            let mut avoid: SmallVec<[[f32; 2]; 32]> = SmallVec::new();
            avoid.extend(plan.bad.iter().copied());
            avoid.extend(
                ctx.state
                    .units
                    .values()
                    .filter(|u| dist2d(u.pos, me) < 1500.0)
                    .take(150)
                    .map(|u| [u.pos[0], u.pos[1]]),
            );
            avoid.extend(
                ctx.state
                    .doodads
                    .values()
                    .filter(|d| dist2d(d.pos, me) < 1500.0)
                    .take(150)
                    .map(|d| [d.pos[0], d.pos[1]]),
            );
            let opts = SampleOpts {
                cells: plan.cells,
                ..SampleOpts::default()
            };
            let world = ctx.world;
            let half = side(plan.cells) * 0.5;
            let mut found = sample_spots(&poly.points, [me[0], me[1]], &opts, &avoid, ctx.rng);
            found.retain(|s| flat(world, s[0], s[1], half));
            ctx.log.debug(format!(
                "housing: area {id} has {} usable spots",
                found.len()
            ));
            plan.spots = found;
            return GoalStatus::Running;
        }
        let s = plan.spots.remove(0);
        let z = ctx
            .world
            .ground_z(s[0], s[1])
            .unwrap_or(ctx.state.me.pos[2]);
        plan.spot = [s[0], s[1], z];
        plan.total = None;
        plan.quote_tries = 0;
        self.moved = false;
        self.phase = Phase::Travel;
        self.phase_ms = ctx.now_ms;
        GoalStatus::Running
    }

    fn travel(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let Some(plan) = self.plan.as_deref() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        let spot = plan.spot;
        if !self.moved {
            self.moved = true;
            ctx.travel(spot[0], spot[1], 1.5, Pace::Run);
            return GoalStatus::Running;
        }
        match ctx.motion.status() {
            MotionStatus::Moving => GoalStatus::Running,
            MotionStatus::Failed(why) => {
                ctx.log
                    .debug(format!("housing: could not reach the spot ({why:?})"));
                self.reject_spot(ctx.now_ms)
            }
            MotionStatus::Arrived | MotionStatus::Idle => {
                if dist2d(ctx.state.me.pos, spot) > 8.0 {
                    // /move did not happen or was refused: walk the rest
                    self.moved = false;
                    return GoalStatus::Running;
                }
                self.phase = Phase::Quote;
                self.phase_ms = ctx.now_ms;
                GoalStatus::Running
            }
        }
    }

    fn quote(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(plan) = self.plan.as_deref_mut() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        if plan.sent_ms == 0 || plan.sent_ms < self.phase_ms {
            ctx.send(cs::construct_house_tax(
                plan.design,
                (plan.spot[0], plan.spot[1], plan.spot[2]),
            ));
            plan.sent_ms = now;
            plan.quote_tries += 1;
            return GoalStatus::Running;
        }
        if let Some(q) = ctx.state.house_log.quote {
            if q.design == plan.design && q.at_ms >= plan.sent_ms {
                plan.total = Some(q.total);
                ctx.log.say(format!(
                    "housing: tax quote {} copper for the {} at {:.0}, {:.0}",
                    q.total, plan.name, plan.spot[0], plan.spot[1]
                ));
                self.phase = Phase::Provision;
                self.phase_ms = now;
                return GoalStatus::Running;
            }
        }
        if now.saturating_sub(plan.sent_ms) > ANSWER_MS {
            if plan.quote_tries >= 2 {
                return GoalStatus::Failed(FailReason::Other("the server gave no tax quote"));
            }
            plan.sent_ms = 0;
        }
        GoalStatus::Running
    }

    fn provision(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let gm = ctx.gm_allowed();
        let Some(plan) = self.plan.as_deref_mut() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        let total = plan.total.unwrap_or(0);
        let need = certificates_for(total);
        let have = certificates_held(ctx);
        let have_item = ctx.state.find_item(plan.item).is_some();
        if have >= need && have_item {
            self.phase = Phase::Create;
            self.phase_ms = now;
            plan.sent_ms = 0;
            return GoalStatus::Running;
        }
        if !gm {
            ctx.log.say(format!(
                "housing: needs {need} tax certificates and the design item, has {have}",
            ));
            return GoalStatus::Failed(FailReason::Other(
                "housing: missing design item or tax certificates",
            ));
        }
        if plan.mint_ms != 0 && now.saturating_sub(plan.mint_ms) < MINT_WAIT_MS {
            return GoalStatus::Running; // wait for the items to arrive
        }
        if plan.mint_tries >= 3 {
            return GoalStatus::Failed(FailReason::Other(
                "could not get the design or the tax certificates",
            ));
        }
        plan.mint_tries += 1;
        plan.mint_ms = now;
        if !have_item {
            ctx.gm(&format!("/item add self {} 1", plan.item));
        }
        if have < need {
            let n = (need - have + CERT_MARGIN).clamp(1, 1000);
            ctx.gm(&format!("/item add self {TAX_CERTIFICATE} {n}"));
        }
        ctx.log.say(format!(
            "housing: asked for the design item and {} tax certificates",
            (need - have + CERT_MARGIN).max(0)
        ));
        GoalStatus::Running
    }

    fn create(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(plan) = self.plan.as_deref_mut() else {
            return GoalStatus::Failed(FailReason::Other("no plan"));
        };
        if plan.sent_ms == 0 {
            let Some((_, item)) = ctx.state.find_item(plan.item) else {
                self.phase = Phase::Provision;
                return GoalStatus::Running;
            };
            plan.houses_before = ctx.state.houses.len();
            ctx.out.push(cs::create_house(&CreateHouse {
                design_id: plan.design,
                pos: (plan.spot[0], plan.spot[1], plan.spot[2]),
                z_rot: 0.0,
                item_id: item.id,
                money: 0,
                ht: 0,
                auto_use_aa_point: false,
            }));
            plan.sent_ms = now;
            ctx.log.say(format!(
                "housing: placing the {} at {:.0}, {:.0}, {:.0} (try {})",
                plan.name,
                plan.spot[0],
                plan.spot[1],
                plan.spot[2],
                self.tries + 1
            ));
            return GoalStatus::Running;
        }
        match read_answer(
            ctx.state.houses.len(),
            plan.houses_before,
            &ctx.state.errors,
            plan.sent_ms,
            now,
        ) {
            Answer::Waiting => GoalStatus::Running,
            Answer::Built => {
                let Some(h) = ctx.state.houses.last().copied() else {
                    return GoalStatus::Running;
                };
                self.tl = h.tl;
                ctx.log.say(format!(
                    "housing: built the {} (house {}) at {:.0}, {:.0}, {:.0}",
                    plan.name, h.id, h.pos[0], h.pos[1], h.pos[2]
                ));
                sync_marker(ctx);
                if ctx.gm_allowed() && h.build_left > 0 {
                    self.enter_build(now, h.build_left);
                } else {
                    self.phase = Phase::Linger;
                    self.stay_ms = now + 3_000;
                }
                GoalStatus::Running
            }
            answer => {
                match answer {
                    Answer::Refused(code, why) => ctx.log.say(format!(
                        "housing: the server refused the spot (error {code}, {why:?})"
                    )),
                    _ => ctx.log.say("housing: no answer to the placement"),
                }
                self.on_refusal(answer, now)
            }
        }
    }

    /// What a refusal (or silence) means for the plan: another spot, a new payment, or the end.
    fn on_refusal(&mut self, answer: Answer, now: u64) -> GoalStatus {
        match answer {
            Answer::Refused(_, Refusal::Rules) => GoalStatus::Failed(FailReason::Other(
                "the account may not build this kind of house",
            )),
            Answer::Refused(_, Refusal::Payment) => {
                let Some(plan) = self.plan.as_deref_mut() else {
                    return GoalStatus::Failed(FailReason::Other("no plan"));
                };
                plan.pay_retries += 1;
                if plan.pay_retries > 2 {
                    return GoalStatus::Failed(FailReason::Other("the server refused the payment"));
                }
                plan.mint_ms = 0;
                plan.mint_tries = 0;
                plan.sent_ms = 0;
                // a fresh quote on the same spot, then provide for it again
                self.phase = Phase::Quote;
                self.phase_ms = now;
                GoalStatus::Running
            }
            _ => self.reject_spot(now),
        }
    }

    /// Marks the current spot bad and goes on with the next candidate.
    fn reject_spot(&mut self, now: u64) -> GoalStatus {
        self.tries += 1;
        if let Some(plan) = self.plan.as_deref_mut() {
            plan.bad.push([plan.spot[0], plan.spot[1]]);
            if plan.bad.len() > 8 {
                plan.bad.remove(0);
            }
            plan.sent_ms = 0;
        }
        if self.tries >= MAX_TRIES {
            return GoalStatus::Failed(FailReason::Other(
                "the server refused every spot that was tried",
            ));
        }
        self.phase = Phase::Spots;
        self.phase_ms = now;
        GoalStatus::Running
    }

    fn enter_build(&mut self, now: u64, left: i32) {
        self.phase = Phase::Build;
        self.phase_ms = now;
        self.build_started_ms = now;
        self.build_left = left;
        self.build_n = (left.max(1) as u32).min(20);
        self.build_ms = 0;
    }

    fn build(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(h) = ctx.state.houses.iter().find(|h| h.tl == self.tl).copied() else {
            return GoalStatus::Done;
        };
        if h.build_left <= 0 {
            ctx.log.say("housing: the house is finished");
            self.phase = Phase::Linger;
            self.stay_ms = now + jitter_ms(ctx.rng, 3_000, 8_000);
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.build_started_ms) > BUILD_MAX_MS {
            ctx.log.say(format!(
                "housing: stopped building, {} steps left",
                h.build_left
            ));
            return GoalStatus::Done;
        }
        let Some(obj) = ctx.state.house_log.obj_of(self.tl) else {
            return GoalStatus::Running;
        };
        // (the GM build command works on the targeted house: `gm_build` targets it right
        // before the command)
        if self.build_ms != 0 && now.saturating_sub(self.build_ms) < BUILD_EVERY_MS {
            return GoalStatus::Running;
        }
        // Did the last batch advance? If not, the batch was too big for the current step.
        if self.build_ms != 0 && h.build_left >= self.build_left {
            self.build_n = (self.build_n / 2).max(1);
        } else if self.build_ms != 0 {
            self.build_n = (self.build_n * 2).min(20);
        }
        self.build_left = h.build_left;
        let n = self.build_n.min(h.build_left.max(1) as u32);
        ctx.gm_build(obj, n);
        self.build_ms = now;
        GoalStatus::Running
    }

    fn home(&mut self, ctx: &mut Ctx<'_>) -> GoalStatus {
        let now = ctx.now_ms;
        let Some(h) = ctx.state.houses.iter().find(|h| h.tl == self.tl).copied() else {
            return GoalStatus::Done;
        };
        self.last_x = h.pos[0];
        self.last_y = h.pos[1];
        if dist2d(ctx.state.me.pos, h.pos) <= 10.0 {
            ctx.motion.stop();
            self.phase = Phase::Linger;
            self.stay_ms = now + jitter_ms(ctx.rng, HOME_STAY_MS.0, HOME_STAY_MS.1);
            ctx.log.say("housing: at home");
            return GoalStatus::Running;
        }
        if now.saturating_sub(self.started_ms) > 4 * 60_000 {
            return GoalStatus::Failed(FailReason::Timeout);
        }
        if !self.moved {
            self.moved = true;
            ctx.travel(h.pos[0], h.pos[1], 6.0, Pace::Run);
            return GoalStatus::Running;
        }
        match ctx.motion.status() {
            MotionStatus::Moving => GoalStatus::Running,
            MotionStatus::Failed(_) => GoalStatus::Failed(FailReason::Stuck),
            MotionStatus::Arrived | MotionStatus::Idle => {
                if dist2d(ctx.state.me.pos, h.pos) > 10.0 {
                    self.moved = false;
                }
                GoalStatus::Running
            }
        }
    }
}

/// Whether the ground around (x, y) is land and level enough for a footprint of half-width
/// `half`. Without baked terrain every spot passes.
fn flat(world: &aa_world::World, x: f32, y: f32, half: f32) -> bool {
    let Some(zc) = world.ground_z(x, y) else {
        return world.baked().terrain.is_none();
    };
    if zc < SWIM_THRESHOLD + 1.0 {
        return false;
    }
    [(half, half), (-half, half), (half, -half), (-half, -half)]
        .iter()
        .all(|(dx, dy)| {
            world
                .ground_z(x + dx, y + dy)
                .is_some_and(|z| (z - zc).abs() <= MAX_SLOPE_M && z >= SWIM_THRESHOLD + 1.0)
        })
}

/// Why housing cannot succeed for this bot right now, `None` when it can be tried: a bot with a
/// house can go home; one with `gm_bootstrap` gets the design and the certificates from the GM;
/// any other needs a design item in the bag already.
pub fn unavailable(ctx: &Ctx<'_>) -> Option<&'static str> {
    if !ctx.state.houses.is_empty() || ctx.gm_allowed() {
        return None;
    }
    pick_design(ctx.world.housing_designs(), &ctx.state.bag, false, false)
        .is_none()
        .then_some("housing needs gm_bootstrap or a design item")
}

/// Keeps the hub's map marker in step with the bot's first house. Cheap; the brain calls it on
/// every tick.
pub fn sync_marker(ctx: &mut Ctx<'_>) {
    let want = ctx.state.houses.first().copied();
    let have = ctx.state.house_log.marker;
    match (want, have) {
        (Some(h), m) if h.id != m => {
            let label = marker_label(ctx, &h);
            ctx.hub.set_house(ctx.persona.id, h.pos[0], h.pos[1], label);
            ctx.state.house_log.marker = h.id;
        }
        (None, m) if m != 0 => {
            ctx.hub.clear_house(ctx.persona.id);
            ctx.state.house_log.marker = 0;
        }
        _ => {}
    }
}

fn marker_label(ctx: &Ctx<'_>, h: &HouseBrief) -> String {
    let design = ctx
        .world
        .housing_designs()
        .iter()
        .find(|d| d.design == h.template)
        .map_or("house", |d| &*d.name);
    format!("{}'s {design}", ctx.persona.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn design(design: u32, item: u32, category: u32, tax: u32, standard: bool) -> HousingDesign {
        HousingDesign {
            design,
            item,
            category,
            tax,
            standard,
            steps: 1,
            garden_radius: match design {
                267 => 4.0,
                172 => 7.5,
                _ => 0.0,
            },
            name: format!("design {design}").into(),
        }
    }

    fn designs() -> Vec<HousingDesign> {
        vec![
            design(267, 15596, 16, 50_000, true),
            design(172, 21166, 1, 100_000, true),
            design(500, 50_000, 3, 0, false),
        ]
    }

    fn bag(templates: &[u32]) -> FxHashMap<u16, Item> {
        templates
            .iter()
            .enumerate()
            .map(|(i, t)| {
                (
                    i as u16,
                    Item {
                        id: i as u64 + 1,
                        template: *t,
                        count: 1,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_design_follows_the_bag_then_the_preference() {
        let d = designs();
        // nothing held, no GM: no design
        assert_eq!(
            pick_design(&d, &bag(&[]), false, false).map(|x| x.design),
            None
        );
        // GM: the scarecrow garden first
        assert_eq!(
            pick_design(&d, &bag(&[]), true, false).map(|x| x.design),
            Some(267)
        );
        // every third bot starts with the cottage
        assert_eq!(
            pick_design(&d, &bag(&[]), true, true).map(|x| x.design),
            Some(172)
        );
        // a design item in the bag wins, GM or not
        assert_eq!(
            pick_design(&d, &bag(&[21166]), false, false).map(|x| x.design),
            Some(172)
        );
        assert_eq!(
            pick_design(&d, &bag(&[21166]), true, false).map(|x| x.design),
            Some(172)
        );
        // a standard design is preferred over a special one
        assert_eq!(
            pick_design(&d, &bag(&[50_000, 15596]), true, false).map(|x| x.design),
            Some(267)
        );
        assert_eq!(
            pick_design(&d, &bag(&[50_000]), false, false).map(|x| x.design),
            Some(500)
        );
        // an empty stack does not count
        let mut b = bag(&[15596]);
        b.get_mut(&0).unwrap().count = 0;
        assert_eq!(pick_design(&d, &b, false, false).map(|x| x.design), None);
        // preferred designs missing: the cheapest standard one
        let only = vec![design(9, 90, 1, 10, true)];
        assert_eq!(
            pick_design(&only, &bag(&[]), true, false).map(|x| x.design),
            Some(9)
        );
    }

    #[test]
    fn certificates_round_up_to_ten_thousand_copper() {
        assert_eq!(certificates_for(0), 0);
        assert_eq!(certificates_for(-5), 0);
        assert_eq!(certificates_for(1), 1);
        assert_eq!(certificates_for(10_000), 1);
        assert_eq!(certificates_for(10_001), 2);
        assert_eq!(certificates_for(150_000), 15);
    }

    #[test]
    fn error_codes_map_to_the_next_step() {
        assert_eq!(classify_error(112), Some(Refusal::BadSpot));
        assert_eq!(classify_error(113), Some(Refusal::BadSpot));
        assert_eq!(classify_error(114), Some(Refusal::BadSpot));
        assert_eq!(classify_error(580), Some(Refusal::BadSpot));
        assert_eq!(classify_error(614), Some(Refusal::BadSpot));
        assert_eq!(classify_error(615), Some(Refusal::Rules));
        assert_eq!(classify_error(616), Some(Refusal::Rules));
        assert_eq!(classify_error(181), Some(Refusal::Payment));
        assert_eq!(classify_error(481), Some(Refusal::Payment));
        assert_eq!(classify_error(120), Some(Refusal::Payment));
        // unrelated messages (trade, loot, ...) are not answers to the placement
        assert_eq!(classify_error(251), None);
        assert_eq!(classify_error(1), None);
    }

    fn plan_at(spot: [f32; 3]) -> Plan {
        Plan {
            design: 267,
            item: 15_596,
            cells: 2,
            name: "Scarecrow Garden".into(),
            areas: vec![7],
            spots: vec![[1.0, 1.0]],
            bad: SmallVec::new(),
            spot,
            total: Some(150_000),
            sent_ms: 5_000,
            mint_ms: 0,
            mint_tries: 0,
            quote_tries: 1,
            pay_retries: 0,
            houses_before: 0,
        }
    }

    fn goal_with_plan() -> Housing {
        Housing {
            phase: Phase::Create,
            plan: Some(Box::new(plan_at([10.0, 20.0, 30.0]))),
            ..Housing::default()
        }
    }

    #[test]
    fn the_answer_is_a_new_house_an_error_or_silence() {
        let err = |t: u64, code: i16| (t, StateError::Msg(code));
        // nothing yet
        assert_eq!(read_answer(0, 0, &[], 5_000, 6_000), Answer::Waiting);
        // a new house
        assert_eq!(read_answer(1, 0, &[], 5_000, 6_000), Answer::Built);
        // a placement error after the request
        assert_eq!(
            read_answer(0, 0, &[err(5_500, 113)], 5_000, 6_000),
            Answer::Refused(113, Refusal::BadSpot)
        );
        // errors from before the request and unrelated ones do not count
        assert_eq!(
            read_answer(0, 0, &[err(4_000, 113), err(5_500, 251)], 5_000, 6_000),
            Answer::Waiting
        );
        // silence after the wait
        assert_eq!(
            read_answer(0, 0, &[], 5_000, 5_000 + ANSWER_MS + 1),
            Answer::Silent
        );
        // a house wins over an old error
        assert_eq!(
            read_answer(2, 1, &[err(5_500, 112)], 5_000, 6_000),
            Answer::Built
        );
    }

    #[test]
    fn a_refused_spot_goes_to_the_next_candidate_and_the_tries_are_limited() {
        let mut g = goal_with_plan();
        let st = g.on_refusal(Answer::Refused(113, Refusal::BadSpot), 7_000);
        assert_eq!(st, GoalStatus::Running);
        assert_eq!(g.phase, Phase::Spots);
        assert_eq!(g.tries, 1);
        let plan = g.plan.as_ref().unwrap();
        assert_eq!(plan.bad.as_slice(), [[10.0, 20.0]]);
        assert_eq!(plan.sent_ms, 0);
        // silence counts like a refusal
        let st = g.on_refusal(Answer::Silent, 8_000);
        assert_eq!(st, GoalStatus::Running);
        assert_eq!(g.tries, 2);
        // the bad list stays bounded
        for _ in 0..20 {
            g.tries = 0;
            g.on_refusal(Answer::Refused(114, Refusal::BadSpot), 9_000);
        }
        assert!(g.plan.as_ref().unwrap().bad.len() <= 9);
        // and after MAX_TRIES refusals the goal gives up
        g.tries = MAX_TRIES - 1;
        let st = g.on_refusal(Answer::Refused(112, Refusal::BadSpot), 10_000);
        assert!(matches!(st, GoalStatus::Failed(FailReason::Other(_))));
    }

    #[test]
    fn a_refused_payment_asks_for_a_fresh_quote_twice_then_gives_up() {
        let mut g = goal_with_plan();
        for n in 1..=2 {
            let st = g.on_refusal(Answer::Refused(481, Refusal::Payment), 7_000);
            assert_eq!(st, GoalStatus::Running, "retry {n}");
            assert_eq!(g.phase, Phase::Quote);
            let p = g.plan.as_ref().unwrap();
            assert_eq!((p.sent_ms, p.mint_tries), (0, 0));
        }
        let st = g.on_refusal(Answer::Refused(481, Refusal::Payment), 7_000);
        assert!(matches!(st, GoalStatus::Failed(FailReason::Other(_))));
    }

    #[test]
    fn rules_refusals_end_the_goal() {
        let mut g = goal_with_plan();
        let st = g.on_refusal(Answer::Refused(615, Refusal::Rules), 7_000);
        assert!(matches!(st, GoalStatus::Failed(FailReason::Other(_))));
    }

    #[test]
    fn footprints_of_the_known_designs() {
        let d = designs();
        assert_eq!(cells_for_radius(garden_radius(&d[0])), 2);
        assert_eq!(cells_for_radius(garden_radius(&d[1])), 4);
    }

    #[test]
    fn the_footprint_comes_from_the_data_and_is_guessed_without_it() {
        let mut d = design(900, 1, 16, 0, true);
        d.garden_radius = 5.5;
        assert_eq!(garden_radius(&d), 5.5);
        d.garden_radius = 0.0;
        assert_eq!(garden_radius(&d), 4.0, "garden category");
        d.category = 1;
        assert_eq!(garden_radius(&d), 7.5, "house category");
        d.category = 3;
        assert_eq!(garden_radius(&d), 9.0);
    }

    #[test]
    fn descriptions_follow_the_phase() {
        let mut h = Housing::default();
        let mut s = String::new();
        h.describe(&mut s);
        assert_eq!(s, "planning a house");
        h.phase = Phase::Build;
        h.build_left = 4;
        s.clear();
        h.describe(&mut s);
        assert_eq!(s, "building the house (4 steps left)");
        h.phase = Phase::Linger;
        s.clear();
        h.describe(&mut s);
        assert_eq!(s, "resting at home");
    }

    #[test]
    fn the_goal_stays_small() {
        assert!(
            std::mem::size_of::<Housing>() <= 120,
            "{}",
            std::mem::size_of::<Housing>()
        );
    }
}
