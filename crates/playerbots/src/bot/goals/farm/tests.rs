//! The farm machine against a scripted server: a bag, a merchant, a field, crops that grow when
//! the server says so. No network, no [`Ctx`](crate::bot::ctx::Ctx).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use aa_proto::sc_world::{DoodadInfo, WorldEvent};
use aa_world::{Crop, FarmKind, Yield};
use smallvec::smallvec;

use super::machine::{Act, Input, Machine, Ops, Phase};
use super::{FailReason, GoalStatus};
use crate::bot::farm::plan::{Plan, Vendor, Why};
use crate::bot::farm::site::Site;
use crate::bot::farm::{refusal, CropBook, FarmMem, Plot, Watcher, OWN_LAND, WILD_LAND};
use crate::bot::motion::MotionStatus;
use crate::bot::state::{dist2d, StateError};

const ME: u32 = 77;
const ME_OBJ: u32 = 9_000;
const SEED: u32 = 15659;
const DOODAD: u32 = 2259;
const GROWING: u32 = 4456;
const RIPE: u32 = 4457;
const POTATO: u32 = 7992;
const VENDOR_NPC: u32 = 31_001;
const VENDOR_OBJ: u32 = 5_001;
const SKILL: u32 = 13980;
const TICK_MS: u64 = 250;

struct Book(Vec<Crop>);

impl CropBook for Book {
    fn by_doodad(&self, doodad: u32) -> Option<&Crop> {
        self.0.iter().find(|c| c.doodad == doodad)
    }
}

fn potato() -> Crop {
    Crop {
        seed: SEED,
        doodad: DOODAD,
        group: 12,
        grow_ms: 600_000,
        radius_mm: 500,
        restrict_zone: 0,
        ripe: vec![(RIPE, SKILL)].into(),
        produce: vec![
            Yield {
                item: POTATO,
                min: 2,
                max: 4,
            },
            Yield {
                item: SEED,
                min: 1,
                max: 2,
            },
        ]
        .into(),
        kinds: FarmKind::Farm.bit(),
    }
}

struct FakeOps {
    spots: RefCell<VecDeque<[f32; 3]>>,
}

impl Ops for FakeOps {
    fn spot(&self, _: &FarmMem, _: &Site, _: u16) -> Option<[f32; 3]> {
        self.spots.borrow_mut().pop_front()
    }
    fn harvest_skill(&self, doodad: u32, phase: u32) -> Option<u32> {
        (doodad == DOODAD && phase == RIPE).then_some(SKILL)
    }
    fn cast_ms(&self, _: u32) -> u64 {
        4_000
    }
    fn radius_mm(&self, _: u32) -> u16 {
        500
    }
    fn plant_labor(&self, _: u32) -> i16 {
        10
    }
}

/// What the scripted server does with a `CSCreateDoodad`.
#[derive(Clone, Copy, PartialEq)]
enum Plant {
    Accept,
    Refuse(i16),
    Silent,
}

/// What the scripted server does with a `CSBuyItems`.
#[derive(Clone, Copy, PartialEq)]
enum Shop {
    Sells,
    Silent,
}

#[derive(Debug, Clone)]
enum Later {
    Created(DoodadInfo),
    Phase(u32, u32),
    Removed(u32),
    Bag(u32, i64, bool),
}

struct Rig {
    now: u64,
    pos: [f32; 3],
    gm: bool,
    replant: bool,
    money: i64,
    labor: i16,
    price: u32,
    bag: HashMap<u32, (u64, i64)>,
    clean: HashMap<u32, u32>,
    merchant: Option<[f32; 3]>,
    errors: Vec<(u64, StateError)>,
    motion: MotionStatus,
    mem: FarmMem,
    machine: Machine,
    ops: FakeOps,
    book: Book,
    plant: Plant,
    shop: Shop,
    // the harvest of the scripted server works (or not)
    harvest_works: bool,
    later: Vec<(u64, Later)>,
    next_obj: u32,
    acts: Vec<Act>,
    gm_lines: Vec<String>,
    planted_at: Vec<[f32; 3]>,
    publishes: u32,
    status: GoalStatus,
    /// Labor the scripted server takes for a seed it plants (a wild patch costs some).
    plant_cost: i16,
}

fn plan(want: u8, buy: u8, mint: u8, vendor: Option<Vendor>) -> Plan {
    Plan {
        seed: SEED,
        doodad: DOODAD,
        radius_mm: 500,
        grow_ms: 600_000,
        site: Site {
            land: OWN_LAND,
            index: 1,
            centre: [100.0, 100.0],
            radius: 8.0,
        },
        want,
        buy,
        vendor,
        price: 10,
        mint,
        why: Why::Profit,
    }
}

fn vendor_at(x: f32, y: f32) -> Vendor {
    Vendor {
        npc: VENDOR_NPC,
        pos: [x, y, 0.0],
        dist: 0.0,
    }
}

impl Rig {
    fn new(machine: Machine) -> Rig {
        Rig {
            now: 10_000,
            pos: [100.0, 100.0, 0.0],
            gm: false,
            replant: true,
            money: 1_000,
            labor: 50,
            price: 10,
            bag: HashMap::new(),
            clean: HashMap::new(),
            merchant: None,
            errors: Vec::new(),
            motion: MotionStatus::Idle,
            mem: FarmMem::default(),
            machine,
            ops: FakeOps {
                spots: RefCell::new(VecDeque::new()),
            },
            book: Book(vec![potato()]),
            plant: Plant::Accept,
            shop: Shop::Sells,
            harvest_works: true,
            later: Vec::new(),
            next_obj: 6_000,
            acts: Vec::new(),
            gm_lines: Vec::new(),
            planted_at: Vec::new(),
            publishes: 0,
            status: GoalStatus::Running,
            plant_cost: 0,
        }
    }

    fn with_spots(self, spots: &[[f32; 3]]) -> Rig {
        *self.ops.spots.borrow_mut() = spots.iter().copied().collect();
        self
    }

    fn with_seeds(mut self, n: i64, clean: u32) -> Rig {
        self.bag.insert(SEED, (500, n));
        self.clean.insert(SEED, clean);
        self
    }

    fn with_merchant(mut self, at: [f32; 3]) -> Rig {
        self.merchant = Some(at);
        self
    }

    fn watcher_land(&self) -> impl Fn(f32, f32) -> u8 {
        |_, _| OWN_LAND
    }

    fn deliver(&mut self) {
        let now = self.now;
        let due: Vec<Later> = self
            .later
            .iter()
            .filter(|(at, _)| *at <= now)
            .map(|(_, l)| l.clone())
            .collect();
        self.later.retain(|(at, _)| *at > now);
        for l in due {
            match l {
                Later::Created(d) => {
                    let ev = WorldEvent::DoodadsCreated(smallvec![d]);
                    self.observe(&ev);
                }
                Later::Phase(obj, phase) => self.observe(&WorldEvent::DoodadPhase { obj, phase }),
                Later::Removed(obj) => self.observe(&WorldEvent::DoodadsRemoved(smallvec![obj])),
                Later::Bag(item, n, clean) => {
                    let e = self.bag.entry(item).or_insert((700 + u64::from(item), 0));
                    e.1 += n;
                    // the ledger counts a harvest as the bot's own only inside an honest window
                    let honest = self.mem.harvest_honest == Some(true);
                    if clean && honest {
                        *self.clean.entry(item).or_insert(0) += n as u32;
                    }
                }
            }
        }
    }

    fn observe(&mut self, ev: &WorldEvent) {
        let land = self.watcher_land();
        let w = Watcher {
            char_id: ME,
            pos: self.pos,
            now_ms: self.now,
            book: &self.book,
            land: &land,
        };
        self.mem.observe(&w, ev);
    }

    /// One tick of the machine, then the scripted server reacts to what it asked for.
    fn tick(&mut self) -> GoalStatus {
        self.now += TICK_MS;
        self.deliver();
        let mut acts = Vec::new();
        let (bag, clean) = (self.bag.clone(), self.clean.clone());
        let count = |t: u32| bag.get(&t).map_or(0, |b| b.1);
        let item = |t: u32| bag.get(&t).filter(|b| b.1 > 0).copied();
        let cleanf = |t: u32| clean.get(&t).copied().unwrap_or(0);
        let merchant = self.merchant;
        let pos = self.pos;
        // the merchant is in view within 40 m
        let seen = |npc: u32| {
            merchant
                .filter(|m| npc == VENDOR_NPC && dist2d(pos, *m) < 40.0)
                .map(|m| (VENDOR_OBJ, m))
        };
        let io = Input {
            now_ms: self.now,
            pos: self.pos,
            me_obj: ME_OBJ,
            gm: self.gm,
            replant: self.replant,
            money: self.money,
            labor: self.labor,
            motion: self.motion,
            errors: &self.errors,
            count: &count,
            item: &item,
            clean: &cleanf,
            merchant: &seen,
            max_trip_m: 4_000.0,
            ops: &self.ops,
        };
        let status = self.machine.step(&io, &mut self.mem, &mut acts);
        self.react(&acts);
        self.acts.extend(acts);
        self.status = status;
        status
    }

    fn react(&mut self, acts: &[Act]) {
        for a in acts {
            match a {
                Act::Travel { x, y, radius } => {
                    // the walk takes no time: the bot ends up `radius` short of the target
                    let d = dist2d(self.pos, [*x, *y, 0.0]);
                    if d > *radius {
                        let k = (d - radius * 0.9) / d;
                        self.pos[0] += (x - self.pos[0]) * k;
                        self.pos[1] += (y - self.pos[1]) * k;
                    }
                    self.motion = MotionStatus::Arrived;
                }
                Act::Gm(line) => {
                    self.gm_lines.push(line.clone());
                    if let Some(rest) = line.strip_prefix("/item add self ") {
                        let mut it = rest.split_whitespace();
                        let (item, n): (u32, i64) = (
                            it.next().unwrap().parse().unwrap(),
                            it.next().unwrap().parse().unwrap(),
                        );
                        self.bag.entry(item).or_insert((800 + u64::from(item), 0)).1 += n;
                    }
                    if line.starts_with("/labor") {
                        self.labor = 60;
                    }
                }
                Act::Buy { npc, item, count } => {
                    let near = self.merchant.is_some_and(|m| dist2d(self.pos, m) <= 3.0);
                    if *npc == VENDOR_OBJ
                        && near
                        && self.shop == Shop::Sells
                        && self.money >= i64::from(self.price) * i64::from(*count)
                    {
                        self.money -= i64::from(self.price) * i64::from(*count);
                        let at = self.now + 300;
                        self.later
                            .push((at, Later::Bag(*item, i64::from(*count), true)));
                        // a purchase is clean for the ledger whatever the window says
                        *self.clean.entry(*item).or_insert(0) += *count as u32;
                    }
                }
                Act::Plant {
                    doodad,
                    pos,
                    item_id,
                    ..
                } => {
                    self.planted_at.push(*pos);
                    assert_eq!(*doodad, DOODAD);
                    assert_eq!(
                        *item_id, self.bag[&SEED].0,
                        "the bag item is named by its id"
                    );
                    match self.plant {
                        Plant::Accept if self.labor < self.plant_cost => {
                            self.errors
                                .push((self.now + 100, StateError::Msg(refusal::NO_LABOR)));
                        }
                        Plant::Accept => {
                            self.labor -= self.plant_cost;
                            self.bag.get_mut(&SEED).unwrap().1 -= 1;
                            if let Some(c) = self.clean.get_mut(&SEED) {
                                *c = c.saturating_sub(1);
                            }
                            self.next_obj += 1;
                            let d = DoodadInfo {
                                obj: self.next_obj,
                                template: DOODAD,
                                pos: (pos[0], pos[1], pos[2]),
                                owner_obj: 0,
                                parent_obj: 0,
                                phase: GROWING,
                                owner_id: ME,
                                plant_time: 0,
                                quest_glow: 0,
                                has_loot: false,
                            };
                            self.later.push((self.now + 400, Later::Created(d)));
                        }
                        Plant::Refuse(code) => {
                            self.errors.push((self.now + 100, StateError::Msg(code)));
                        }
                        Plant::Silent => {}
                    }
                }
                Act::Harvest { obj, .. } => {
                    if self.harvest_works {
                        let at = self.now + 4_200;
                        self.later.push((at, Later::Phase(*obj, GROWING)));
                        self.later.push((at, Later::Bag(POTATO, 3, true)));
                        self.later.push((at, Later::Bag(SEED, 2, true)));
                        self.later.push((at + 1_000, Later::Removed(*obj)));
                    }
                }
                Act::Publish => self.publishes += 1,
                Act::Say(_) | Act::Face { .. } | Act::StopMotion | Act::RequestList => {}
            }
        }
    }

    /// Ticks until the run ends (at most `limit_ms` of bot time).
    fn run(&mut self, limit_ms: u64) -> GoalStatus {
        let end = self.now + limit_ms;
        while self.now < end {
            let s = self.tick();
            if s != GoalStatus::Running {
                return s;
            }
        }
        GoalStatus::Running
    }

    fn count_acts(&self, f: impl Fn(&Act) -> bool) -> usize {
        self.acts.iter().filter(|a| f(a)).count()
    }
}

#[test]
fn seeds_are_bought_from_a_vendor_that_is_walked_to_and_planted_on_three_spots() {
    let spots = [[104.0, 100.0, 0.0], [100.0, 105.0, 0.0], [95.0, 100.0, 0.0]];
    let m = Machine::plant(plan(3, 3, 0, Some(vendor_at(300.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m)
        .with_spots(&spots)
        .with_merchant([300.0, 100.0, 0.0]);
    rig.money = 100;
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(
        rig.count_acts(|a| matches!(a, Act::Buy { count: 3, .. })),
        1
    );
    assert_eq!(rig.money, 70);
    assert_eq!(rig.planted_at.len(), 3);
    assert_eq!(rig.machine.planted, 3);
    assert_eq!(rig.mem.plots.len(), 3);
    for (p, spot) in rig.mem.plots.iter().zip(spots) {
        assert_ne!(p.obj, 0, "every crop was announced");
        assert!(p.honest, "bought seeds make an honest plot");
        assert_eq!(p.pos, spot);
        assert_eq!(p.seed, SEED);
        assert_eq!(p.check_ms, p.planted_ms + 600_000 + 3_000);
    }
    assert_eq!(rig.mem.planted, 3);
    assert!(rig.publishes >= 3);
    assert_eq!(rig.bag[&SEED].1, 0, "the seeds went into the ground");
    assert!(rig.gm_lines.is_empty());
}

#[test]
fn a_vendor_that_is_out_of_view_is_walked_to_and_a_vendor_that_is_not_there_is_given_up() {
    // the vendor spawn is 500 m away and not in view until the bot is near
    let m = Machine::plant(plan(1, 1, 0, Some(vendor_at(600.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m)
        .with_spots(&[[104.0, 100.0, 0.0]])
        .with_merchant([600.0, 100.0, 0.0]);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert!(rig.pos[0] > 590.0 || rig.planted_at.len() == 1);
    assert_eq!(rig.mem.plots.len(), 1);
    // nobody there: after a while the run fails (no GM to mint seeds)
    let m = Machine::plant(plan(1, 1, 0, Some(vendor_at(600.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m).with_spots(&[[104.0, 100.0, 0.0]]);
    assert_eq!(
        rig.run(120_000),
        GoalStatus::Failed(FailReason::Other("the vendor is not there"))
    );
    assert!(rig.planted_at.is_empty());
}

#[test]
fn a_vendor_that_sells_nothing_is_tried_three_times_then_a_gm_mints_dishonest_seeds() {
    let m = Machine::plant(plan(2, 2, 0, Some(vendor_at(110.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m)
        .with_spots(&[[104.0, 100.0, 0.0], [100.0, 105.0, 0.0]])
        .with_merchant([110.0, 100.0, 0.0]);
    rig.shop = Shop::Silent;
    rig.gm = true;
    assert_eq!(rig.run(180_000), GoalStatus::Done);
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Buy { .. })), 3);
    assert_eq!(rig.gm_lines, vec!["/item add self 15659 2"]);
    assert_eq!(rig.mem.plots.len(), 2);
    assert!(
        rig.mem.plots.iter().all(|p| !p.honest),
        "minted seeds are not the bot's own"
    );
    // without a GM the run fails
    let m = Machine::plant(plan(2, 2, 0, Some(vendor_at(110.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m).with_merchant([110.0, 100.0, 0.0]);
    rig.shop = Shop::Silent;
    assert_eq!(
        rig.run(180_000),
        GoalStatus::Failed(FailReason::Other("the vendor sold nothing"))
    );
}

#[test]
fn no_money_means_minting_with_a_gm_and_failing_without() {
    let m = Machine::plant(plan(2, 2, 0, Some(vendor_at(110.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m)
        .with_spots(&[[104.0, 100.0, 0.0], [100.0, 105.0, 0.0]])
        .with_merchant([110.0, 100.0, 0.0]);
    rig.money = 3;
    rig.gm = true;
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert!(rig.gm_lines[0].starts_with("/item add self 15659"));
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Buy { .. })), 0);
    let m = Machine::plant(plan(2, 2, 0, Some(vendor_at(110.0, 100.0))), false, 10_000);
    let mut rig = Rig::new(m).with_merchant([110.0, 100.0, 0.0]);
    rig.money = 3;
    assert_eq!(
        rig.run(60_000),
        GoalStatus::Failed(FailReason::Other("not enough money"))
    );
}

#[test]
fn a_plan_to_mint_asks_the_gm_first_and_plants_what_arrives() {
    let m = Machine::plant(plan(3, 0, 3, None), false, 10_000);
    let mut rig =
        Rig::new(m).with_spots(&[[104.0, 100.0, 0.0], [100.0, 105.0, 0.0], [95.0, 100.0, 0.0]]);
    rig.gm = true;
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.gm_lines, vec!["/item add self 15659 3"]);
    assert_eq!(rig.mem.plots.len(), 3);
    assert!(rig.mem.plots.iter().all(|p| !p.honest));
    // a GM line that does not take effect fails the run in time
    let m = Machine::plant(plan(3, 0, 3, None), false, 10_000);
    let mut rig = Rig::new(m);
    rig.gm = false;
    assert_eq!(
        rig.run(30_000),
        GoalStatus::Failed(FailReason::Other("seeds need a GM"))
    );
}

#[test]
fn seeds_in_the_bag_are_planted_without_shopping_and_the_first_run_asks_for_the_farm_list() {
    let m = Machine::plant(plan(2, 0, 0, None), true, 10_000);
    let mut rig = Rig::new(m)
        .with_seeds(2, 2)
        .with_spots(&[[104.0, 100.0, 0.0], [100.0, 105.0, 0.0]]);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.count_acts(|a| matches!(a, Act::RequestList)), 1);
    assert!(rig.mem.list_asked);
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Buy { .. })), 0);
    assert_eq!(rig.mem.plots.len(), 2);
    assert!(rig.mem.plots.iter().all(|p| p.honest));
    // the list is asked for only once per session
    let mut m2 = Machine::plant(plan(1, 0, 0, None), true, rig.now);
    assert_eq!(m2.phase(), Phase::Fetch);
    let mut acts = Vec::new();
    let c = |_: u32| 0;
    let it = |_: u32| None;
    let me = |_: u32| None;
    let io = Input {
        now_ms: rig.now,
        pos: rig.pos,
        me_obj: ME_OBJ,
        gm: false,
        replant: true,
        money: 0,
        labor: 0,
        motion: MotionStatus::Idle,
        errors: &[],
        count: &c,
        item: &it,
        clean: &c2,
        merchant: &me,
        max_trip_m: 100.0,
        ops: &rig.ops,
    };
    m2.step(&io, &mut rig.mem, &mut acts);
    assert!(
        !acts.contains(&Act::RequestList),
        "the memory knows it asked"
    );
}

fn c2(_: u32) -> u32 {
    0
}

#[test]
fn a_refused_spot_is_marked_and_the_next_one_is_tried() {
    let m = Machine::plant(plan(1, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m)
        .with_seeds(1, 1)
        .with_spots(&[[104.0, 100.0, 0.0], [100.0, 105.0, 0.0]]);
    rig.plant = Plant::Refuse(refusal::TOO_CLOSE_DOODAD);
    // the first spot is refused, then the server relents
    for _ in 0..30 {
        rig.tick();
        if rig.planted_at.len() == 1 {
            break;
        }
    }
    assert_eq!(rig.planted_at, vec![[104.0, 100.0, 0.0]]);
    rig.plant = Plant::Accept;
    assert_eq!(rig.run(60_000), GoalStatus::Done);
    assert_eq!(rig.planted_at.len(), 2);
    assert_eq!(rig.planted_at[1], [100.0, 105.0, 0.0]);
    assert!(
        rig.mem.spot_is_bad(104.0, 100.0, rig.now),
        "the refused spot is remembered"
    );
    assert_eq!(rig.mem.plots.len(), 1, "the refused plot was taken back");
    assert_eq!(rig.mem.plots[0].pos, [100.0, 105.0, 0.0]);
}

#[test]
fn a_full_farm_ends_the_run_with_what_was_planted() {
    let m = Machine::plant(plan(3, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m).with_seeds(3, 3).with_spots(&[
        [104.0, 100.0, 0.0],
        [100.0, 105.0, 0.0],
        [95.0, 100.0, 0.0],
    ]);
    for _ in 0..60 {
        rig.tick();
        if rig.planted_at.len() == 2 && rig.mem.plots.iter().filter(|p| p.obj != 0).count() == 2 {
            break;
        }
    }
    rig.plant = Plant::Refuse(refusal::FARM_COUNT);
    assert_eq!(rig.run(60_000), GoalStatus::Done);
    assert_eq!(rig.machine.planted, 2);
    assert_eq!(rig.mem.plots.len(), 2);
    // the first seed refused for that reason: the run failed, nothing planted
    let m = Machine::plant(plan(3, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m)
        .with_seeds(3, 3)
        .with_spots(&[[104.0, 100.0, 0.0]]);
    rig.plant = Plant::Refuse(refusal::FARM_TYPE);
    assert_eq!(
        rig.run(60_000),
        GoalStatus::Failed(FailReason::Other("farm is full"))
    );
    assert!(rig.mem.plots.is_empty());
}

#[test]
fn a_seed_the_server_will_not_plant_is_remembered() {
    let m = Machine::plant(plan(2, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m)
        .with_seeds(2, 2)
        .with_spots(&[[104.0, 100.0, 0.0]]);
    rig.plant = Plant::Refuse(refusal::BAD_ITEM);
    assert_eq!(
        rig.run(60_000),
        GoalStatus::Failed(FailReason::Other("seed refused"))
    );
    assert!(rig.mem.seed_is_bad(SEED, rig.now));
    assert!(rig.mem.plots.is_empty());
}

#[test]
fn a_server_that_plants_nothing_costs_a_spot_and_never_hangs_the_run() {
    let spots: Vec<[f32; 3]> = (0..10)
        .map(|i| [90.0 + i as f32 * 2.0, 100.0, 0.0])
        .collect();
    let m = Machine::plant(plan(1, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m).with_seeds(1, 1).with_spots(&spots);
    rig.plant = Plant::Silent;
    let s = rig.run(10 * 60_000);
    assert_eq!(s, GoalStatus::Failed(FailReason::Other("no spot accepted")));
    assert_eq!(rig.planted_at.len(), 6, "six spots are tried");
    assert!(rig.mem.plots.is_empty());
}

#[test]
fn no_free_spot_ends_the_run() {
    let m = Machine::plant(plan(2, 0, 0, None), false, 10_000);
    let mut rig = Rig::new(m).with_seeds(2, 2);
    assert_eq!(
        rig.run(30_000),
        GoalStatus::Failed(FailReason::Other("no free spot"))
    );
}

fn ripe_plot(obj: u32, x: f32, honest: bool) -> Plot {
    Plot {
        obj,
        doodad: DOODAD,
        seed: SEED,
        pos: [x, 100.0, 0.0],
        phase: RIPE,
        ripe: true,
        check_ms: 0,
        planted_ms: 1,
        honest,
        land: OWN_LAND,
    }
}

#[test]
fn a_ripe_plot_is_walked_to_harvested_and_planted_again_with_the_seeds_it_gave() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.mem.plots.push(ripe_plot(3_001, 400.0, true));
    let mut window_seen = None;
    let mut status = GoalStatus::Running;
    for _ in 0..400 {
        status = rig.tick();
        if rig.mem.harvest_honest.is_some() {
            window_seen = rig.mem.harvest_honest;
        }
        if status != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(status, GoalStatus::Done);
    assert_eq!(
        window_seen,
        Some(true),
        "the harvest ran inside an honest window"
    );
    assert_eq!(rig.mem.harvest_honest, None, "and the window is closed");
    assert_eq!(rig.machine.harvested, 1);
    assert_eq!(rig.mem.harvested, 1);
    assert_eq!(
        rig.count_acts(|a| matches!(
            a,
            Act::Harvest {
                skill: SKILL,
                obj: 3_001
            }
        )),
        1
    );
    assert_eq!(rig.bag[&POTATO].1, 3);
    assert_eq!(rig.clean[&POTATO], 3, "an honest harvest is the bot's own");
    // the seeds it gave back went into the ground on the same spot
    assert_eq!(rig.planted_at, vec![[400.0, 100.0, 0.0]]);
    assert_eq!(rig.mem.plots.len(), 1);
    let p = rig.mem.plots[0];
    assert_ne!(p.obj, 3_001);
    assert!(p.honest, "the seeds of an honest harvest are honest");
    assert!(!p.ripe);
    assert_eq!(rig.bag[&SEED].1, 1);
    assert_eq!(rig.machine.planted, 1);
}

#[test]
fn a_harvest_of_minted_seeds_opens_a_dishonest_window() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.mem.plots.push(ripe_plot(3_001, 110.0, false));
    let mut window_seen = None;
    for _ in 0..400 {
        let s = rig.tick();
        if rig.mem.harvest_honest.is_some() {
            window_seen = rig.mem.harvest_honest;
        }
        if s != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(window_seen, Some(false));
    assert_eq!(
        rig.clean.get(&POTATO).copied().unwrap_or(0),
        0,
        "minted seeds make no clean produce"
    );
    assert_eq!(rig.bag[&POTATO].1, 3);
    // the replanted seeds are not clean either, so that plot is not honest
    assert!(rig.mem.plots.iter().all(|p| !p.honest));
}

#[test]
fn replanting_can_be_switched_off() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.replant = false;
    rig.mem.plots.push(ripe_plot(3_001, 110.0, true));
    assert_eq!(rig.run(300_000), GoalStatus::Done);
    assert!(rig.planted_at.is_empty());
    assert!(rig.mem.plots.is_empty());
    assert_eq!(
        rig.bag[&SEED].1, 2,
        "the seeds stay in the bag for the next planting run"
    );
}

#[test]
fn only_the_plots_that_are_due_are_visited_nearest_first() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.replant = false;
    rig.mem.plots.push(ripe_plot(3_001, 300.0, true));
    rig.mem.plots.push(ripe_plot(3_002, 120.0, true));
    let mut growing = ripe_plot(3_003, 130.0, true);
    growing.ripe = false;
    growing.phase = GROWING;
    growing.check_ms = rig.now + 3_600_000;
    rig.mem.plots.push(growing);
    assert_eq!(rig.run(600_000), GoalStatus::Done);
    let order: Vec<u32> = rig
        .acts
        .iter()
        .filter_map(|a| match a {
            Act::Harvest { obj, .. } => Some(*obj),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec![3_002, 3_001]);
    assert_eq!(rig.mem.plots.len(), 1, "the growing crop stays");
    assert_eq!(rig.mem.plots[0].obj, 3_003);
}

#[test]
fn a_crop_that_is_not_in_view_at_its_place_is_gone() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    let mut p = ripe_plot(0, 110.0, true);
    p.check_ms = 1;
    p.ripe = false;
    rig.mem.plots.push(p);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert!(rig.mem.plots.is_empty());
    assert_eq!(rig.mem.lost, 1);
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Harvest { .. })), 0);
}

#[test]
fn a_due_crop_that_is_not_ripe_is_looked_at_again_later() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    let mut p = ripe_plot(3_001, 110.0, true);
    p.ripe = false;
    p.phase = GROWING;
    p.check_ms = 1;
    rig.mem.plots.push(p);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.mem.plots.len(), 1);
    assert!(rig.mem.plots[0].check_ms > rig.now, "not due again at once");
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Harvest { .. })), 0);
}

#[test]
fn a_harvest_that_does_nothing_is_cast_three_times_then_left() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.harvest_works = false;
    rig.mem.plots.push(ripe_plot(3_001, 110.0, true));
    assert_eq!(rig.run(300_000), GoalStatus::Done);
    assert_eq!(rig.count_acts(|a| matches!(a, Act::Harvest { .. })), 3);
    assert_eq!(rig.mem.plots.len(), 1);
    assert!(rig.mem.plots[0].check_ms > rig.now);
    assert_eq!(rig.mem.harvest_honest, None);
    assert_eq!(rig.machine.harvested, 0);
}

#[test]
fn a_harvest_needs_labor_a_gm_tops_it_up() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.replant = false;
    rig.labor = 0;
    rig.gm = true;
    rig.mem.plots.push(ripe_plot(3_001, 110.0, true));
    assert_eq!(rig.run(300_000), GoalStatus::Done);
    assert_eq!(rig.gm_lines, vec!["/labor 60"]);
    assert_eq!(rig.machine.harvested, 1);
    // without a GM the run fails and the window stays closed
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.labor = 0;
    rig.mem.plots.push(ripe_plot(3_001, 110.0, true));
    assert_eq!(
        rig.run(60_000),
        GoalStatus::Failed(FailReason::Other("not enough labor"))
    );
    assert_eq!(rig.mem.harvest_honest, None);
    assert_eq!(rig.mem.plots.len(), 1);
}

#[test]
fn a_run_that_goes_on_too_long_times_out_and_closes_the_window() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.mem.harvest_honest = Some(true);
    rig.now = 10_000 + super::machine::RUN_LIMIT_MS + 1_000;
    let s = rig.tick();
    assert_eq!(s, GoalStatus::Failed(FailReason::Timeout));
    assert_eq!(rig.mem.harvest_honest, None);
}

#[test]
fn stopping_closes_the_harvest_window_and_publishes() {
    let mut machine = Machine::harvest(false, 10_000);
    let mut mem = FarmMem::default();
    mem.harvest_honest = Some(false);
    let mut acts = Vec::new();
    machine.stop(&mut mem, &mut acts);
    assert_eq!(mem.harvest_honest, None);
    assert!(acts.contains(&Act::Publish) && acts.contains(&Act::StopMotion));
}

#[test]
fn nothing_due_means_a_harvest_run_is_over_at_once() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    assert_eq!(rig.run(10_000), GoalStatus::Done);
    assert!(rig.acts.is_empty() || rig.count_acts(|a| matches!(a, Act::Harvest { .. })) == 0);
}

#[test]
fn the_goal_scores_itself_and_a_homebody_merchant_likes_it_most() {
    use crate::config::BehaviorCfg;
    use crate::roster::Temperament;
    assert_eq!(
        super::base_weight(&BehaviorCfg::default(), &Temperament::NEUTRAL),
        0.0
    );
    let neutral = super::weight(&Temperament::NEUTRAL);
    assert!((neutral - 0.35).abs() < 1e-6);
    let keen = Temperament {
        homebody: 1.0,
        merchant: 1.0,
        ..Temperament::NEUTRAL
    };
    let wild = Temperament {
        homebody: 0.0,
        merchant: 0.0,
        ..Temperament::NEUTRAL
    };
    assert!(super::weight(&keen) > neutral && neutral > super::weight(&wild));
    assert!(super::weight(&wild) > 0.0);
}

#[test]
fn neighbouring_plots_a_spacing_apart_are_harvested_and_replanted_one_by_one() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    // three crops 1.7 m apart, as close as the planner puts them
    for (i, x) in [100.0f32, 101.7, 103.4].into_iter().enumerate() {
        let mut p = ripe_plot(3_001 + i as u32, x, true);
        p.pos[1] = 102.0;
        rig.mem.plots.push(p);
    }
    assert_eq!(rig.run(600_000), GoalStatus::Done);
    assert_eq!(rig.machine.harvested, 3);
    assert_eq!(
        rig.planted_at.len(),
        3,
        "every harvested place is planted again"
    );
    assert_eq!(rig.mem.plots.len(), 3);
    let mut xs: Vec<f32> = rig.planted_at.iter().map(|p| p[0]).collect();
    xs.sort_by(f32::total_cmp);
    assert_eq!(xs, vec![100.0, 101.7, 103.4]);
    assert_eq!(rig.mem.lost, 0);
}

fn wild_plan(want: u8) -> Plan {
    Plan {
        site: Site::wild([100.0, 100.0]),
        ..plan(want, 0, 0, None)
    }
}

#[test]
fn a_wild_patch_costs_labor_per_seed_and_stops_when_it_runs_out() {
    let spots = [[101.0, 100.0, 0.0], [99.0, 102.0, 0.0], [99.0, 98.0, 0.0]];
    let mut rig = Rig::new(Machine::plant(wild_plan(3), false, 10_000))
        .with_spots(&spots)
        .with_seeds(3, 3);
    rig.plant_cost = 10;
    rig.labor = 25;
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.planted_at.len(), 2, "25 labor pays for two");
    assert_eq!(rig.mem.count_on(WILD_LAND), 2);
    assert!(
        rig.mem.plots.iter().all(|p| p.honest),
        "bought seeds stay honest"
    );
    assert!(rig.gm_lines.is_empty());
    // a GM tops its labor up and plants all three
    let mut rig = Rig::new(Machine::plant(wild_plan(3), false, 10_000))
        .with_spots(&spots)
        .with_seeds(3, 0);
    rig.plant_cost = 10;
    rig.labor = 0;
    rig.gm = true;
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.gm_lines, vec!["/labor 60"]);
    assert_eq!(rig.planted_at.len(), 3);
    assert!(
        rig.mem.plots.iter().all(|p| !p.honest),
        "seeds that are not the bot's own make no honest plot"
    );
    // the server's own refusal ends the run too
    let mut rig = Rig::new(Machine::plant(wild_plan(1), false, 10_000))
        .with_spots(&spots)
        .with_seeds(1, 1);
    rig.plant = Plant::Refuse(refusal::NO_LABOR);
    assert_eq!(
        rig.run(60_000),
        GoalStatus::Failed(FailReason::Other("not enough labor"))
    );
    assert!(rig.mem.plots.is_empty());
}

#[test]
fn a_wild_crop_that_is_gone_at_its_place_is_noted_as_gone_and_one_harvested_as_harvested() {
    // gone: the bot stands at the place and sees nothing
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    let mut p = ripe_plot(0, 110.0, true);
    p.land = WILD_LAND;
    p.check_ms = 1;
    p.ripe = false;
    rig.mem.plots.push(p);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.mem.lost, 1);
    let gone = rig.mem.take_gone();
    assert_eq!(gone.len(), 1);
    assert_eq!((gone[0].land, gone[0].harvested), (WILD_LAND, false));
    assert!(rig.mem.take_gone().is_empty(), "taken once");
    // harvested: the crop leaves the world after the harvest
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    rig.replant = false;
    let mut p = ripe_plot(3_001, 110.0, true);
    p.land = WILD_LAND;
    rig.mem.plots.push(p);
    assert_eq!(rig.run(120_000), GoalStatus::Done);
    assert_eq!(rig.machine.harvested, 1);
    let gone = rig.mem.take_gone();
    assert_eq!(gone.len(), 1);
    assert!(gone[0].harvested);
    assert_eq!(rig.mem.lost, 0);
}

#[test]
fn a_refusal_for_somebody_elses_land_costs_only_the_spot() {
    let spots = [[104.0, 100.0, 0.0], [100.0, 105.0, 0.0]];
    let mut rig = Rig::new(Machine::plant(wild_plan(1), false, 10_000))
        .with_spots(&spots)
        .with_seeds(1, 1);
    rig.plant = Plant::Refuse(refusal::NO_PERM);
    let _ = rig.run(8_000);
    assert!(rig.mem.spot_is_bad(104.0, 100.0, rig.now));
    assert!(rig.planted_at.len() >= 2, "the next spot is tried");
}

#[test]
fn a_stolen_crop_is_harvested_dishonestly_and_its_place_is_not_planted_again() {
    let mut rig = Rig::new(Machine::harvest(false, 10_000));
    let mut p = ripe_plot(3_001, 110.0, false);
    p.land = crate::bot::farm::STOLEN_LAND;
    rig.mem.plots.push(p);
    let mut window = None;
    for _ in 0..400 {
        if rig.tick() != GoalStatus::Running {
            break;
        }
        window = window.or(rig.mem.harvest_honest);
    }
    assert_eq!(rig.status, GoalStatus::Done);
    assert_eq!(window, Some(false), "a theft is never the bot's own");
    assert_eq!(rig.machine.harvested, 1);
    assert!(rig.planted_at.is_empty(), "the thief does not plant there");
    assert!(rig.mem.plots.is_empty());
}
