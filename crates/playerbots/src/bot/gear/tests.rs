//! Tests of the gear feature: the planner against a made-up catalog, whole runs of the machine
//! against a scripted server, and the bot state following the equipment.

use std::collections::{HashMap, HashSet};

use aa_proto::sc::ServerEvent;
use aa_proto::sc_world::{EquipSlot, InvItem, ItemChange, ItemTaskInfo, OwnState, WorldEvent};
use aa_world::gear::{
    armor_kind, equip_slot, slot_type, GearClass, GearItem, GradeFacts, Lunagem, SoldGear,
};
use smallvec::smallvec;

use super::eval::tests::{armor, plate_sword_and_shield, weapon, Table};
use super::eval::{BagPiece, Facts, Fit, Move, Worn};
use super::machine::{Act, Entry, Input, Machine, Phase, CAST_EXTRA_MS, RUN_LIMIT_MS};
use super::plan::{self, Catalog, Job, Me, Rules, Vendor};
use super::*;
use crate::bot::goals::{FailReason, GoalStatus};
use crate::bot::motion::MotionStatus;
use crate::bot::state::{BotState, StateError};

// ---- a made-up world --------------------------------------------------------------------------

const CHEST_10: u32 = 101;
const CHEST_20: u32 = 102;
const CHEST_30: u32 = 103;
const CLOTH_CHEST_20: u32 = 104;
const SWORD_20: u32 = 110;
const SHIELD_20: u32 = 111;
const HEAD_22: u32 = 120;
const LEGS_14: u32 = 201;
const GEM: u32 = 301;
const HIGH_GEM: u32 = 302;
const SCROLL: u32 = 401;
const GEM_SKILL: u32 = 23_728;
const SCROLL_SKILL: u32 = 9_001;
const MERCHANT: u32 = 7_000;

/// The catalog of the tests: the [`Table`] of eval's tests plus offers, prices and the rest.
struct Shop {
    t: Table,
    offers: Vec<SoldGear>,
    prices: HashMap<u32, u32>,
    not_mintable: HashSet<u32>,
    skills: HashMap<u32, u32>,
    grades: HashMap<u8, GradeFacts>,
}

impl Facts for Shop {
    fn item(&self, template: u32) -> Option<GearItem> {
        self.t.item(template)
    }
    fn grade_factor(&self, grade: u8) -> f32 {
        self.t.grade_factor(grade)
    }
    fn grade_order(&self, grade: u8) -> u8 {
        self.t.grade_order(grade)
    }
    fn gem(&self, template: u32) -> Option<Lunagem> {
        self.t.gem(template)
    }
    fn sockets(&self, slot_type: u8, grade: u8) -> u8 {
        self.t.sockets(slot_type, grade)
    }
    fn socket_chance(&self, held: u8) -> u16 {
        self.t.socket_chance(held)
    }
}

impl Catalog for Shop {
    fn offers(&self) -> &[SoldGear] {
        &self.offers
    }
    fn vendor(&self, npcs: &[u32]) -> Option<Vendor> {
        npcs.contains(&MERCHANT).then_some(Vendor {
            npc: MERCHANT,
            pos: [100.0, 0.0, 0.0],
            dist: 100.0,
        })
    }
    fn price(&self, template: u32) -> u32 {
        self.prices.get(&template).copied().unwrap_or(0)
    }
    fn each_mintable(&self, f: &mut dyn FnMut(u32, GearItem)) {
        let mut ids: Vec<u32> = self.t.items.keys().copied().collect();
        ids.sort_unstable();
        for t in ids {
            if !self.not_mintable.contains(&t) {
                f(t, self.t.items[&t]);
            }
        }
    }
    fn each_gem(&self, f: &mut dyn FnMut(u32, Lunagem)) {
        let mut ids: Vec<u32> = self.t.gems.keys().copied().collect();
        ids.sort_unstable();
        for t in ids {
            f(t, self.t.gems[&t]);
        }
    }
    fn use_skill(&self, item: u32) -> u32 {
        self.skills.get(&item).copied().unwrap_or(0)
    }
    fn grade_facts(&self, grade: u8) -> GradeFacts {
        self.grades.get(&grade).copied().unwrap_or(GradeFacts {
            order: self.t.grade_order(grade),
            stat_pct: 100,
            regrade_success: 3_000,
            regrade_break: 0,
            regrade_downgrade: 0,
        })
    }
    fn scroll(&self, class: GearClass) -> u32 {
        match class {
            GearClass::Armor => SCROLL,
            _ => 0,
        }
    }
}

fn shop() -> Shop {
    let mut t = Table::default();
    t.items
        .insert(CHEST_10, armor(slot_type::CHEST, armor_kind::PLATE, 10));
    t.items
        .insert(CHEST_20, armor(slot_type::CHEST, armor_kind::PLATE, 20));
    t.items
        .insert(CHEST_30, armor(slot_type::CHEST, armor_kind::PLATE, 30));
    t.items.insert(
        CLOTH_CHEST_20,
        armor(slot_type::CHEST, armor_kind::CLOTH, 20),
    );
    t.items
        .insert(SWORD_20, weapon(slot_type::ONE_HANDED, 1, 20));
    t.items.insert(SHIELD_20, weapon(slot_type::SHIELD, 24, 20));
    t.items
        .insert(HEAD_22, armor(slot_type::HEAD, armor_kind::PLATE, 22));
    t.items
        .insert(LEGS_14, armor(slot_type::LEGS, armor_kind::PLATE, 14));
    let chest_only = 1 << slot_type::CHEST;
    t.gems.insert(
        GEM,
        Lunagem {
            slot_types: chest_only,
            min_level: 0,
        },
    );
    t.gems.insert(
        HIGH_GEM,
        Lunagem {
            slot_types: chest_only,
            min_level: 40,
        },
    );
    Shop {
        t,
        offers: vec![SoldGear {
            item: LEGS_14,
            grade: 0,
            npcs: vec![MERCHANT].into_boxed_slice(),
        }],
        prices: [(LEGS_14, 500), (CHEST_20, 900)].into_iter().collect(),
        not_mintable: HashSet::new(),
        skills: [
            (GEM, GEM_SKILL),
            (HIGH_GEM, GEM_SKILL),
            (SCROLL, SCROLL_SKILL),
        ]
        .into_iter()
        .collect(),
        grades: HashMap::new(),
    }
}

fn all_rules() -> Rules {
    Rules {
        buy: true,
        mint: true,
        socket: true,
        regrade: true,
        max_spend_pct: 30,
    }
}

/// The bot of a planner test.
struct Bot {
    level: u8,
    money: i64,
    gearhead: f32,
    fit: Fit,
    worn: Vec<(u8, u64, u32, u8)>,
    gems: HashMap<u64, u8>,
    bag: Vec<BagPiece>,
    fetched: Vec<u32>,
}

impl Bot {
    fn new(level: u8) -> Bot {
        Bot {
            level,
            money: 10_000,
            gearhead: 0.5,
            fit: plate_sword_and_shield(),
            worn: Vec::new(),
            gems: HashMap::new(),
            bag: Vec::new(),
            fetched: Vec::new(),
        }
    }

    fn wear(mut self, slot: u8, id: u64, template: u32, grade: u8, gems: Option<u8>) -> Bot {
        self.worn.push((slot, id, template, grade));
        if let Some(g) = gems {
            self.gems.insert(id, g);
        }
        self
    }

    fn holding(mut self, id: u64, template: u32) -> Bot {
        self.bag.push(BagPiece {
            slot: self.bag.len() as u16,
            id,
            template,
            grade: 0,
            gems: Some(0),
        });
        self
    }

    fn worn_set(&self, shop: &Shop) -> Worn {
        Worn::build(
            self.worn.iter().copied(),
            |id| self.gems.get(&id).copied(),
            shop,
            &self.fit,
        )
    }

    fn choose(&self, shop: &Shop, rules: &Rules, item: Option<u32>) -> Result<Job, &'static str> {
        let worn = self.worn_set(shop);
        let me = self.me(&worn);
        plan::choose(&me, rules, shop, item)
    }

    fn want(&self, shop: &Shop, rules: &Rules) -> plan::Want {
        let worn = self.worn_set(shop);
        plan::want(&self.me(&worn), rules, shop)
    }

    fn me<'a>(&'a self, worn: &'a Worn) -> Me<'a> {
        Me {
            id: 7,
            level: self.level,
            gender: 1,
            money: self.money,
            gearhead: self.gearhead,
            fit: &self.fit,
            worn,
            bag: &self.bag,
            fetched: &self.fetched,
            minted: 0,
            bought: 0,
        }
    }
}

// ---- the planner ------------------------------------------------------------------------------

#[test]
fn a_merchant_piece_is_bought_when_it_is_worth_it_and_affordable() {
    let shop = shop();
    let bot = Bot::new(15);
    let job = bot.choose(&shop, &all_rules(), None).unwrap();
    assert_eq!(
        job,
        Job::Buy {
            template: LEGS_14,
            grade: 0,
            price: 500,
            vendor: Vendor {
                npc: MERCHANT,
                pos: [100.0, 0.0, 0.0],
                dist: 100.0
            },
            forced: false,
        }
    );
    assert!(bot.want(&shop, &all_rules()).buy);
    // 500 copper is more than 30 % of 1000: minted instead
    let mut poor = Bot::new(15);
    poor.money = 1_000;
    assert!(matches!(
        poor.choose(&shop, &all_rules(), None),
        Ok(Job::Mint { .. })
    ));
    let no_mint = Rules {
        mint: false,
        socket: false,
        regrade: false,
        ..all_rules()
    };
    assert!(poor.choose(&shop, &no_mint, None).is_err());
    // a level-25 bot does not buy the level-14 legs while it may mint the tier...
    let bot = Bot::new(25);
    assert!(matches!(
        bot.choose(&shop, &all_rules(), None),
        Ok(Job::Mint { .. })
    ));
    // ... but it does when minting is off
    assert!(matches!(
        bot.choose(&shop, &no_mint, None),
        Ok(Job::Buy {
            template: LEGS_14,
            ..
        })
    ));
    // a piece it bought this session is not bought again
    let mut again = Bot::new(15);
    again.fetched.push(LEGS_14);
    assert!(again.choose(&shop, &no_mint, None).is_err());
}

#[test]
fn minting_picks_the_best_piece_of_the_build_at_the_tier_grade() {
    let mut shop = shop();
    let bot = Bot::new(25).wear(equip_slot::CHEST, 900, CHEST_10, 0, Some(0));
    let no_buy = Rules {
        buy: false,
        ..all_rules()
    };
    // the empty head gains most: the level-22 plate helmet at uncommon (the grade of band 2)
    assert_eq!(
        bot.choose(&shop, &no_buy, None),
        Ok(Job::Mint {
            template: HEAD_22,
            grade: 2,
            forced: false
        })
    );
    // the helmet is not mintable (a quest or test item): the sword or the shield; never the
    // level-30 chest (above the bot's level) nor the cloth chest (a poor fit)
    shop.not_mintable.insert(HEAD_22);
    match bot.choose(&shop, &no_buy, None) {
        Ok(Job::Mint { template, .. }) => {
            assert!([SWORD_20, SHIELD_20].contains(&template), "{template}")
        }
        other => panic!("{other:?}"),
    }
    let mut bot = bot;
    bot.fetched = vec![SWORD_20, SHIELD_20, LEGS_14];
    assert_eq!(
        bot.choose(&shop, &no_buy, None),
        Ok(Job::Mint {
            template: CHEST_20,
            grade: 2,
            forced: false
        })
    );
    // minting off and nothing for sale: nothing to do
    let nothing = Rules {
        buy: false,
        mint: false,
        socket: false,
        regrade: false,
        max_spend_pct: 30,
    };
    assert_eq!(bot.choose(&shop, &nothing, None), Err("nothing to upgrade"));
}

#[test]
fn an_operators_item_is_taken_from_the_bag_bought_or_minted() {
    let shop = shop();
    let bot = Bot::new(25).holding(5, CLOTH_CHEST_20);
    assert_eq!(
        bot.choose(&shop, &all_rules(), Some(CLOTH_CHEST_20)),
        Ok(Job::Equip {
            template: CLOTH_CHEST_20,
            forced: true
        })
    );
    assert!(matches!(
        bot.choose(&shop, &all_rules(), Some(LEGS_14)),
        Ok(Job::Buy {
            template: LEGS_14,
            forced: true,
            ..
        })
    ));
    assert_eq!(
        bot.choose(&shop, &all_rules(), Some(CHEST_20)),
        Ok(Job::Mint {
            template: CHEST_20,
            grade: 2,
            forced: true
        })
    );
    assert_eq!(
        bot.choose(&shop, &all_rules(), Some(CHEST_30)),
        Err("above the bot's level")
    );
    assert_eq!(
        bot.choose(&shop, &all_rules(), Some(999)),
        Err("not a piece of gear")
    );
    let worn = Bot::new(25).wear(equip_slot::CHEST, 900, CHEST_20, 0, Some(0));
    assert_eq!(
        worn.choose(&shop, &all_rules(), Some(CHEST_20)),
        Err("already wears it")
    );
}

#[test]
fn the_odds_a_bot_takes_with_a_gem_follow_its_gearhead() {
    assert_eq!(plan::min_chance(0.5), 10_000);
    assert_eq!(plan::min_chance(1.0), 3_000);
    assert!(plan::may_socket(0, 10_000, 0.0), "the first gem always");
    assert!(
        !plan::may_socket(1, 5_000, 0.5),
        "a cautious bot keeps its gems"
    );
    assert!(!plan::may_socket(0, 5_000, 0.5));
    assert!(plan::may_socket(1, 5_000, 0.8));
    assert!(!plan::may_socket(1, 2_000, 0.8));
    assert!(!plan::may_socket(0, 0, 1.0));
    assert!(plan::affordable(300, 1_000, 30));
    assert!(!plan::affordable(301, 1_000, 30));
    assert!(
        !plan::affordable(0, 1_000, 30),
        "a free piece is a data error"
    );
}

#[test]
fn a_gem_goes_into_a_free_socket_when_the_odds_suit_the_bot() {
    let shop = shop();
    let gems_only = Rules {
        buy: false,
        mint: false,
        regrade: false,
        ..all_rules()
    };
    // a rare chest (two sockets) without gems and a gem in the bag
    let bot = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 3, Some(0))
        .holding(60, GEM);
    assert_eq!(
        bot.choose(&shop, &gems_only, None),
        Ok(Job::Socket {
            target: 900,
            slot: equip_slot::CHEST,
            gem: GEM,
            mint: false,
            chance: 10_000,
            held: 0
        })
    );
    assert!(bot.want(&shop, &gems_only).socket);
    // gems not known: never
    let unknown = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 3, None)
        .holding(60, GEM);
    assert!(unknown.choose(&shop, &gems_only, None).is_err());
    // the second gem only for a bot that takes the risk
    let one = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 3, Some(1))
        .holding(60, GEM);
    assert!(one.choose(&shop, &gems_only, None).is_err());
    let mut bold = one;
    bold.gearhead = 0.9;
    assert!(matches!(
        bold.choose(&shop, &gems_only, None),
        Ok(Job::Socket {
            chance: 5_000,
            held: 1,
            ..
        })
    ));
    // full
    let full = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 3, Some(2))
        .holding(60, GEM);
    assert!(full.choose(&shop, &gems_only, None).is_err());
    // a common chest has no socket
    let common = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 0, Some(0))
        .holding(60, GEM);
    assert!(common.choose(&shop, &gems_only, None).is_err());
    // the gem for item level 40 does not fit; minting finds the one that does
    let high = Bot::new(25)
        .wear(equip_slot::CHEST, 900, CHEST_20, 3, Some(0))
        .holding(61, HIGH_GEM);
    assert!(high.choose(&shop, &gems_only, None).is_err());
    let mint_gems = Rules {
        mint: true,
        ..gems_only
    };
    assert!(matches!(
        high.choose(&shop, &mint_gems, Some(GEM)),
        Ok(Job::Socket {
            gem: GEM,
            mint: true,
            ..
        })
    ));
}

#[test]
fn a_regrade_is_tried_only_where_a_failure_cannot_destroy_the_piece() {
    let mut shop = shop();
    let regrade_only = Rules {
        buy: false,
        socket: false,
        ..all_rules()
    };
    let mut bot = Bot::new(25).wear(equip_slot::CHEST, 900, CHEST_20, 0, Some(0));
    bot.fetched = vec![HEAD_22, SWORD_20, SHIELD_20, LEGS_14, CHEST_20];
    // band 2 wants uncommon: the common chest is regraded with a minted scroll... after the
    // minting found nothing (all fetched)
    assert_eq!(
        bot.choose(&shop, &regrade_only, None),
        Ok(Job::Regrade {
            target: 900,
            slot: equip_slot::CHEST,
            scroll: SCROLL,
            mint: true,
            from: 0
        })
    );
    // without minting the scroll must be in the bag
    let no_mint = Rules {
        mint: false,
        ..regrade_only
    };
    assert!(bot.choose(&shop, &no_mint, None).is_err());
    let with_scroll = Bot {
        bag: vec![BagPiece {
            slot: 1,
            id: 70,
            template: SCROLL,
            grade: 0,
            gems: None,
        }],
        ..Bot::new(25).wear(equip_slot::CHEST, 900, CHEST_20, 0, Some(0))
    };
    assert!(matches!(
        with_scroll.choose(&shop, &no_mint, None),
        Ok(Job::Regrade { mint: false, .. })
    ));
    // a grade whose failure can lower it: only for a bot that likes the risk
    shop.grades.insert(
        0,
        GradeFacts {
            order: 1,
            stat_pct: 100,
            regrade_success: 5_000,
            regrade_break: 0,
            regrade_downgrade: 8_000,
        },
    );
    assert!(bot.choose(&shop, &regrade_only, None).is_err());
    bot.gearhead = 0.9;
    assert!(bot.choose(&shop, &regrade_only, None).is_ok());
    // a grade whose failure destroys the piece: never
    shop.grades.insert(
        0,
        GradeFacts {
            order: 1,
            stat_pct: 100,
            regrade_success: 5_000,
            regrade_break: 10_000,
            regrade_downgrade: 0,
        },
    );
    assert!(bot.choose(&shop, &regrade_only, None).is_err());
    // regrading off
    let off = Rules {
        regrade: false,
        ..regrade_only
    };
    assert!(!bot.want(&shop, &off).regrade);
}

// ---- whole runs against a scripted server -----------------------------------------------------

/// The scripted server and the bot of a machine test.
struct Rig {
    now: u64,
    pos: [f32; 3],
    money: i64,
    bag: Vec<(u64, u32)>,
    worn: Vec<(u8, u64, u32)>,
    errors: Vec<(u64, StateError)>,
    merchant: Option<(u32, [f32; 3])>,
    /// The new piece is worth putting on.
    better: bool,
    mem: GearMem,
    acts: Vec<Act>,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            now: 1_000,
            pos: [0.0, 0.0, 0.0],
            money: 10_000,
            bag: Vec::new(),
            worn: Vec::new(),
            errors: Vec::new(),
            merchant: None,
            better: true,
            mem: GearMem::default(),
            acts: Vec::new(),
        }
    }

    /// One tick, 500 ms after the last.
    fn step(&mut self, m: &mut Machine) -> GoalStatus {
        self.now += 500;
        let (bag, worn, better) = (self.bag.clone(), self.worn.clone(), self.better);
        let bag_item = |t: u32| bag.iter().find(|b| b.1 == t).map(|b| b.0);
        let worn_at = |id: u64| worn.iter().find(|w| w.1 == id).map(|w| (w.0, w.2));
        let place = |id: u64, forced: bool| {
            let b = bag.iter().find(|b| b.0 == id)?;
            (forced || better).then_some(Move {
                id,
                template: b.1,
                bag_slot: 3,
                slot: equip_slot::CHEST,
                worn_id: 0,
                gain: 5.0,
            })
        };
        let merchant = |npc: u32| self.merchant.filter(|_| npc == MERCHANT);
        let use_skill = |t: u32| match t {
            GEM => GEM_SKILL,
            SCROLL => SCROLL_SKILL,
            _ => 0,
        };
        let cast_ms = |_s: u32| 3_000;
        let io = Input {
            now_ms: self.now,
            pos: self.pos,
            motion: MotionStatus::Idle,
            money: self.money,
            errors: &self.errors,
            bag_item: &bag_item,
            worn: &worn_at,
            place: &place,
            merchant: &merchant,
            use_skill: &use_skill,
            cast_ms: &cast_ms,
        };
        let mut acts = Vec::new();
        let st = m.step(&io, &mut self.mem, &mut acts);
        for a in &acts {
            if let Act::Travel { x, y, .. } = a {
                self.pos = [*x, *y, 0.0];
            }
        }
        self.acts = acts;
        st
    }

    fn sent(&self, f: impl Fn(&Act) -> bool) -> bool {
        self.acts.iter().any(f)
    }

    /// The server puts bag item `id` on (it echoes the move).
    fn lands(&mut self, id: u64) {
        let i = self.bag.iter().position(|b| b.0 == id).unwrap();
        let (_, t) = self.bag.remove(i);
        self.worn.push((equip_slot::CHEST, id, t));
    }
}

fn vendor() -> Vendor {
    Vendor {
        npc: MERCHANT,
        pos: [100.0, 0.0, 0.0],
        dist: 100.0,
    }
}

#[test]
fn a_purchase_walks_to_the_merchant_buys_and_puts_the_piece_on() {
    let mut rig = Rig::new();
    let mut m = Machine::new(
        Job::Buy {
            template: LEGS_14,
            grade: 0,
            price: 500,
            vendor: vendor(),
            forced: false,
        },
        rig.now,
    );
    assert_eq!(m.phase(), Phase::Shop);
    // the merchant is not in view: walk to its spawn
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| matches!(a, Act::Travel { x, .. } if *x == 100.0)));
    rig.merchant = Some((5_000, [101.0, 0.0, 0.0]));
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| *a
        == Act::Buy {
            npc: 5_000,
            item: LEGS_14,
            grade: 0
        }));
    // no second purchase while the first is on its way
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(!rig.sent(|a| matches!(a, Act::Buy { .. })));
    rig.bag.push((900, LEGS_14));
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| *a
        == Act::Journal(Entry::Bought {
            template: LEGS_14,
            price: 500
        })));
    assert_eq!(m.phase(), Phase::Equip);
    assert_eq!(
        rig.mem.gems_of(900),
        Some(0),
        "a bought piece holds no gems"
    );
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| matches!(a, Act::Equip(mv) if mv.id == 900)));
    rig.lands(900);
    assert_eq!(rig.step(&mut m), GoalStatus::Done);
    assert!(rig.sent(|a| matches!(a, Act::Journal(Entry::Equipped { .. }))));
    let s = rig.mem.stats;
    assert_eq!((s.bought, s.spent, s.equipped), (1, 500, 1));
    assert!(rig.mem.fetched.contains(&LEGS_14));
}

#[test]
fn a_merchant_that_sells_nothing_or_is_not_there_ends_the_run() {
    let mut rig = Rig::new();
    rig.pos = [100.0, 0.0, 0.0];
    rig.merchant = Some((5_000, [101.0, 0.0, 0.0]));
    let job = Job::Buy {
        template: LEGS_14,
        grade: 0,
        price: 500,
        vendor: vendor(),
        forced: false,
    };
    let mut m = Machine::new(job, rig.now);
    let mut status = GoalStatus::Running;
    for _ in 0..40 {
        status = rig.step(&mut m);
        if status != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(
        status,
        GoalStatus::Failed(FailReason::Other("the merchant sold nothing"))
    );
    // standing at the spawn, the merchant never shows up
    let mut rig = Rig::new();
    rig.pos = [100.0, 0.0, 0.0];
    let mut m = Machine::new(job, rig.now);
    let mut status = GoalStatus::Running;
    for _ in 0..40 {
        status = rig.step(&mut m);
        if status != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(
        status,
        GoalStatus::Failed(FailReason::Other("the merchant is not there"))
    );
    // too poor by now
    let mut rig = Rig::new();
    rig.money = 100;
    let mut m = Machine::new(job, rig.now);
    assert_eq!(
        rig.step(&mut m),
        GoalStatus::Failed(FailReason::Other("not enough money"))
    );
}

#[test]
fn a_minted_piece_the_server_refuses_is_left_alone() {
    let mut rig = Rig::new();
    let mut m = Machine::new(
        Job::Mint {
            template: CHEST_20,
            grade: 2,
            forced: false,
        },
        rig.now,
    );
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| *a == Act::Gm("/item add self 102 1 2".into())));
    rig.bag.push((901, CHEST_20));
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert_eq!(rig.mem.grade_of(901), 2);
    assert_eq!(rig.mem.stats.minted, 1);
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| matches!(a, Act::Equip(mv) if mv.id == 901)));
    // INVALID_SLOT, twice
    rig.errors.push((rig.now, StateError::Msg(203)));
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| matches!(a, Act::Equip(_))), "tried once more");
    rig.errors.push((rig.now, StateError::Msg(203)));
    assert_eq!(
        rig.step(&mut m),
        GoalStatus::Failed(FailReason::Other("the server refused the piece"))
    );
    assert!(rig.mem.refused(901, rig.now));
    assert!(!rig.mem.refused(901, rig.now + REFUSE_MS + 1));
}

#[test]
fn a_new_piece_that_is_no_better_stays_in_the_bag() {
    let mut rig = Rig::new();
    rig.better = false;
    let mut m = Machine::new(
        Job::Mint {
            template: CHEST_20,
            grade: 0,
            forced: false,
        },
        rig.now,
    );
    rig.step(&mut m);
    assert!(rig.sent(|a| *a == Act::Gm("/item add self 102 1".into())));
    rig.bag.push((901, CHEST_20));
    rig.step(&mut m);
    assert_eq!(rig.step(&mut m), GoalStatus::Done);
    assert!(!rig.sent(|a| matches!(a, Act::Equip(_))));
    // the item the operator asked for goes on anyway
    let mut rig = Rig::new();
    rig.better = false;
    rig.bag.push((902, CLOTH_CHEST_20));
    let mut m = Machine::new(
        Job::Equip {
            template: CLOTH_CHEST_20,
            forced: true,
        },
        rig.now,
    );
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    assert!(rig.sent(|a| matches!(a, Act::Equip(mv) if mv.id == 902)));
    rig.lands(902);
    assert_eq!(rig.step(&mut m), GoalStatus::Done);
}

#[test]
fn a_minted_gem_is_cast_on_the_worn_piece_and_the_result_counts() {
    for ok in [true, false] {
        let mut rig = Rig::new();
        rig.worn.push((equip_slot::CHEST, 950, CHEST_20));
        rig.mem.note_new(950, 3);
        let mut m = Machine::new(
            Job::Socket {
                target: 950,
                slot: equip_slot::CHEST,
                gem: GEM,
                mint: true,
                chance: 10_000,
                held: 0,
            },
            rig.now,
        );
        assert_eq!(m.phase(), Phase::Mint);
        rig.step(&mut m);
        assert!(rig.sent(|a| *a == Act::Gm("/item add self 301 1".into())));
        rig.bag.push((960, GEM));
        rig.step(&mut m);
        assert_eq!(m.phase(), Phase::Cast);
        assert_eq!(rig.step(&mut m), GoalStatus::Running);
        assert!(rig.sent(|a| *a
            == Act::Cast {
                skill: GEM_SKILL,
                item_id: 960,
                item_template: GEM,
                target: 950
            }));
        // the server: the gem is used up, the item task and the result arrive
        rig.bag.clear();
        assert_eq!(
            rig.step(&mut m),
            GoalStatus::Running,
            "waits for the result"
        );
        let now = rig.now;
        rig.mem.on_lunagem(now, u8::from(ok), 950);
        assert_eq!(rig.step(&mut m), GoalStatus::Done);
        assert!(rig.sent(|a| matches!(a, Act::Journal(Entry::Socket { ok: o, .. }) if *o == ok)));
        let s = rig.mem.stats;
        if ok {
            assert_eq!((s.socketed, s.socket_fails), (1, 0));
            assert_eq!(rig.mem.gems_of(950), Some(1));
        } else {
            assert_eq!((s.socketed, s.socket_fails), (0, 1));
            assert_eq!(rig.mem.gems_of(950), Some(0));
        }
    }
}

#[test]
fn a_cast_the_server_refuses_or_never_answers_fails() {
    let job = Job::Socket {
        target: 950,
        slot: equip_slot::CHEST,
        gem: GEM,
        mint: false,
        chance: 10_000,
        held: 0,
    };
    let mut rig = Rig::new();
    rig.worn.push((equip_slot::CHEST, 950, CHEST_20));
    rig.bag.push((960, GEM));
    let mut m = Machine::new(job, rig.now);
    assert_eq!(rig.step(&mut m), GoalStatus::Running);
    rig.errors.push((
        rig.now,
        StateError::Skill {
            skill: GEM_SKILL,
            result: 7,
        },
    ));
    assert_eq!(
        rig.step(&mut m),
        GoalStatus::Failed(FailReason::Other("the server refused the cast"))
    );
    // no answer at all
    let mut rig = Rig::new();
    rig.worn.push((equip_slot::CHEST, 950, CHEST_20));
    rig.bag.push((960, GEM));
    let mut m = Machine::new(job, rig.now);
    rig.step(&mut m);
    let mut status = GoalStatus::Running;
    for _ in 0..((3_000 + CAST_EXTRA_MS) / 500 + 2) {
        status = rig.step(&mut m);
        if status != GoalStatus::Running {
            break;
        }
    }
    assert_eq!(status, GoalStatus::Failed(FailReason::Timeout));
    // the gem is not in the bag
    let mut rig = Rig::new();
    rig.worn.push((equip_slot::CHEST, 950, CHEST_20));
    let mut m = Machine::new(job, rig.now);
    assert!(matches!(rig.step(&mut m), GoalStatus::Failed(_)));
    // a run that takes too long
    let mut rig = Rig::new();
    let mut m = Machine::new(job, rig.now);
    rig.now += RUN_LIMIT_MS;
    assert_eq!(rig.step(&mut m), GoalStatus::Failed(FailReason::Timeout));
}

#[test]
fn a_regrade_reports_its_result_even_when_the_piece_breaks() {
    for (result, breaks) in [(3u8, false), (2, false), (0, true)] {
        let mut rig = Rig::new();
        rig.worn.push((equip_slot::CHEST, 950, CHEST_20));
        rig.bag.push((970, SCROLL));
        let mut m = Machine::new(
            Job::Regrade {
                target: 950,
                slot: equip_slot::CHEST,
                scroll: SCROLL,
                mint: false,
                from: 0,
            },
            rig.now,
        );
        rig.step(&mut m);
        assert!(rig.sent(|a| matches!(
            a,
            Act::Cast {
                skill: SCROLL_SKILL,
                target: 950,
                ..
            }
        )));
        rig.bag.clear();
        if breaks {
            rig.worn.clear();
        }
        let now = rig.now;
        let after = if result >= 3 { 2 } else { 0 };
        rig.mem.on_grade(now, result, 950, 0, after);
        assert_eq!(rig.step(&mut m), GoalStatus::Done, "result {result}");
        let s = rig.mem.stats;
        assert_eq!(
            (s.regraded, s.regrade_fails),
            if result >= 3 { (1, 0) } else { (0, 1) }
        );
        assert_eq!(rig.mem.grade_of(950), after);
    }
}

// ---- the memory and the bot state ------------------------------------------------------------

#[test]
fn the_memory_stays_bounded_and_forgets_refusals() {
    let mut mem = GearMem::default();
    for id in 1..=(MAX_KNOWN as u64 + 10) {
        mem.note_gems(id, 1);
    }
    assert!(mem.full());
    assert_eq!(mem.gems_of(1), None, "the oldest went first");
    assert_eq!(mem.gems_of(MAX_KNOWN as u64 + 10), Some(1));
    mem.prune(|id| id % 2 == 0);
    assert!(!mem.full());
    // a common piece the bot does not know needs no entry
    mem.note_grade(5_000, 0);
    assert_eq!(mem.grade_of(5_000), 0);
    assert!(mem.gems_of(5_000).is_none());
    for id in 0..20 {
        mem.refuse(id, 100);
    }
    assert!(mem.refused(19, 100) && !mem.refused(0, 100));
    for t in 0..30 {
        mem.fetched_now(t);
    }
    assert_eq!(mem.fetched.len(), MAX_FETCHED);
    mem.pending = Some(PendingEquip {
        id: 1,
        template: 2,
        slot: 3,
        sent_ms: 4,
    });
    mem.after_reset();
    assert!(mem.pending.is_none());
    assert_eq!(
        mem.gems_of(MAX_KNOWN as u64 + 10),
        Some(1),
        "facts outlive a reconnect"
    );
    mem.changed(&"y".repeat(400));
    assert_eq!(mem.last_change.chars().count(), MAX_CHANGE_CHARS);
}

fn world(w: WorldEvent) -> ServerEvent {
    ServerEvent::World(w)
}

fn own_state() -> OwnState {
    OwnState {
        id: 500,
        name: "Aria".into(),
        race: 1,
        gender: 2,
        level: 25,
        hp: 1000,
        mp: 400,
        zone_id: 7,
        faction_id: 101,
        expedition_id: 0,
        family_id: 0,
        abilities: [1, 11, 11],
        pos: (15000.0, 15100.0, 126.0),
        yaw: 1.0,
        instance_id: 1,
        exp: 50,
        recoverable_exp: 0,
        labor: 50,
        money: 20_000,
        bank_money: 0,
        loyalty: 0,
        bag_slots: 50,
        bank_slots: 0,
        equipment: Box::new([
            EquipSlot {
                slot: equip_slot::CHEST,
                template: CHEST_20,
                id: 51,
                grade: 3,
            },
            EquipSlot {
                slot: equip_slot::MAINHAND,
                template: SWORD_20,
                id: 52,
                grade: 0,
            },
        ]),
        equipment_gems: Box::new([2, 0]),
    }
}

fn task(changes: &[ItemChange]) -> ServerEvent {
    world(WorldEvent::ItemTask(Box::new(ItemTaskInfo {
        reason: 5,
        changes: changes.iter().copied().collect(),
        force_removed: smallvec![],
    })))
}

#[test]
fn the_state_follows_the_equipment_its_grades_and_gems() {
    let mut s = BotState::new();
    s.apply(&world(WorldEvent::CharacterState(Box::new(own_state()))));
    let g = s.gear.as_deref().expect("allocated at log-in");
    assert_eq!(
        (g.gems_of(51), g.gems_of(52), g.bag_slots),
        (Some(2), Some(0), 50)
    );
    s.apply(&world(WorldEvent::Inventory {
        slot_type: 2,
        chunk: 0,
        items: Box::new([
            InvItem {
                slot: 3,
                id: 77,
                template: CHEST_30,
                grade: 4,
                count: 1,
                treasure: None,
            },
            InvItem {
                slot: 5,
                id: 78,
                template: HEAD_22,
                grade: 0,
                count: 1,
                treasure: None,
            },
        ]),
    }));
    assert_eq!(s.gear.as_deref().unwrap().grade_of(77), 4);
    // a swap: the worn chest goes to bag slot 3, the bag chest on (with its grade)
    s.apply(&task(&[ItemChange::Moved {
        from_slot_type: 1,
        from_slot: equip_slot::CHEST,
        to_slot_type: 2,
        to_slot: 3,
        from_id: 51,
        to_id: 77,
    }]));
    let chest = s
        .equipment
        .iter()
        .find(|e| e.slot == equip_slot::CHEST)
        .unwrap();
    assert_eq!((chest.id, chest.template, chest.grade), (77, CHEST_30, 4));
    assert_eq!(s.bag.get(&3).map(|i| i.id), Some(51));
    assert_eq!(
        s.gear.as_deref().unwrap().grade_of(51),
        3,
        "the grade went along"
    );
    // an equip into the empty head: from the empty slot (id 0) to the bag item
    s.apply(&task(&[ItemChange::Moved {
        from_slot_type: 1,
        from_slot: equip_slot::HEAD,
        to_slot_type: 2,
        to_slot: 5,
        from_id: 0,
        to_id: 78,
    }]));
    assert!(s
        .equipment
        .iter()
        .any(|e| e.slot == equip_slot::HEAD && e.id == 78));
    assert!(!s.bag.contains_key(&5));
    // an unequip into a free bag slot
    s.apply(&task(&[ItemChange::Moved {
        from_slot_type: 1,
        from_slot: equip_slot::HEAD,
        to_slot_type: 2,
        to_slot: 9,
        from_id: 78,
        to_id: 0,
    }]));
    assert!(!s.equipment.iter().any(|e| e.slot == equip_slot::HEAD));
    assert_eq!(s.bag.get(&9).map(|i| i.id), Some(78));
    // a socketing updates the gems, a regrade the grade
    s.apply(&task(&[
        ItemChange::Updated {
            slot_type: 1,
            slot: equip_slot::CHEST,
            id: 77,
            gems: Some(3),
        },
        ItemChange::GradeChange {
            slot_type: 1,
            slot: equip_slot::CHEST,
            id: 77,
            grade: 5,
        },
    ]));
    assert_eq!(s.gear.as_deref().unwrap().gems_of(77), Some(3));
    let chest = s.equipment.iter().find(|e| e.id == 77).unwrap();
    assert_eq!(chest.grade, 5);
    // a piece added straight into the equipment keeps its grade
    s.apply(&task(&[ItemChange::Added {
        action: 5,
        slot_type: 1,
        slot: equip_slot::FEET,
        template: 9,
        id: 90,
        grade: 2,
        count: 1,
        treasure: None,
    }]));
    assert_eq!(
        s.equipment.iter().find(|e| e.id == 90).map(|e| e.grade),
        Some(2)
    );
    // the results
    s.now_ms = 5_000;
    s.apply(&world(WorldEvent::LunagemResult {
        result: 0,
        item_id: 77,
        gem: GEM,
        installed: true,
    }));
    let g = s.gear.as_deref().unwrap();
    assert_eq!(
        g.socket_result.map(|r| (r.at_ms, r.result)),
        Some((5_000, 0))
    );
    assert_eq!(g.gems_of(77), Some(0), "a failure wipes the gems");
    s.apply(&world(WorldEvent::GradeEnchantResult {
        result: 3,
        item_id: 77,
        template: CHEST_30,
        before: 5,
        after: 6,
    }));
    assert_eq!(
        s.gear.as_deref().unwrap().grade_result.map(|r| r.after),
        Some(6)
    );
    // a new connection keeps the facts
    s.reset();
    assert_eq!(s.gear.as_deref().unwrap().gems_of(77), Some(0));
    assert!(s.equipment.is_empty());
}

// ---- quest rewards ----------------------------------------------------------------------------

#[test]
fn the_reward_that_adds_most_to_the_worn_set_is_picked() {
    let shop = shop();
    let fit = plate_sword_and_shield();
    let worn = Worn::build(
        [(equip_slot::CHEST, 1, CHEST_20, 0)],
        |_| Some(0),
        &shop,
        &fit,
    );
    let names = |t: u32| match t {
        900 => Some("Box of Nodachi"),
        901 => Some("Sword Bundle"),
        902 => Some("Plate Armor Crate"),
        _ => None,
    };
    // the empty head beats a cloth chest and the chest the bot has
    assert_eq!(
        pick_reward(
            &[CLOTH_CHEST_20, CHEST_10, HEAD_22],
            &worn,
            &shop,
            &fit,
            25,
            1,
            names
        ),
        Some(2)
    );
    // boxes: the build's weapon kind wins, then its armor kind; unknown boxes do not count
    assert_eq!(
        pick_reward(&[900, 901, 902], &worn, &shop, &fit, 25, 1, names),
        Some(1)
    );
    assert_eq!(
        pick_reward(&[900, 902], &worn, &shop, &fit, 25, 1, names),
        Some(1)
    );
    assert_eq!(
        pick_reward(&[900, 903], &worn, &shop, &fit, 25, 1, names),
        None
    );
    // a known piece beats any name
    assert_eq!(
        pick_reward(&[901, SWORD_20], &worn, &shop, &fit, 25, 1, names),
        Some(1)
    );
    assert!(
        name_hint("Greatsword of Dawn", &fit) == 0.0,
        "not a word of the build"
    );
    assert!(looks_real("Tree Shade Hood"));
    assert!(!looks_real("[Test] Hood") && !looks_real("GM Test Sword") && !looks_real(""));
    assert!(!looks_real("npc shield_60_1") && !looks_real("staff_1h_46_1"));
    assert!(contains_nocase("Tree Shade HOOD", "hood"));
}

#[test]
fn the_gm_lines_of_a_mint_are_allowed() {
    for line in ["/item add self 102 1", "/item add self 102 1 2"] {
        let wire = crate::gm::plan(line, "Aria").unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(wire.line, line, "an /item line names `self`");
    }
}
