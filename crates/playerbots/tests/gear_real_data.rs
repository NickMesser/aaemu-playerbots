//! The gear planner against the real `main_world` data (ignored by default): every class build
//! gets a sane piece to mint at each tier, a lunagem that fits its chest, and a quest reward.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> \
//!   cargo test -p playerbots --test gear_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use aa_world::gear::{armor_kind, equip_slot, slot_type, GearClass};
use aa_world::{World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::gear::eval::{BagPiece, Facts, Fit, Tier, Worn};
use playerbots::bot::gear::plan::{self, Job, Me, Rules};
use playerbots::bot::gear::{looks_real, pick_reward, WorldCatalog};

fn world() -> Option<World> {
    let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
    let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
    let paths = WorldPaths {
        compact,
        server_data,
        baked: None,
        world: "main_world".into(),
    };
    Some(World::load(&paths).expect("load main_world"))
}

fn me<'a>(level: u8, fit: &'a Fit, worn: &'a Worn, bag: &'a [BagPiece]) -> Me<'a> {
    Me {
        id: 42,
        level,
        gender: 1,
        money: 1_000_000,
        gearhead: 0.9,
        fit,
        worn,
        bag,
        fetched: &[],
        minted: 0,
        bought: 0,
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn every_build_mints_a_sane_piece_at_every_tier() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set: skipped");
        return;
    };
    let cat = WorldCatalog {
        world: &world,
        pos: [0.0, 0.0, 0.0],
        max_trip_m: 3_000.0,
    };
    let rules = Rules {
        buy: false,
        mint: true,
        socket: false,
        regrade: false,
        max_spend_pct: 30,
        guild_budget: 0,
    };
    let archetypes = Archetypes::builtin().unwrap();
    for a in archetypes.all() {
        let fit = Fit::of(a);
        for level in [10u8, 25, 40, 55] {
            let worn = Worn::default();
            let job = plan::choose(&me(level, &fit, &worn, &[]), &rules, &cat, None)
                .unwrap_or_else(|e| panic!("{} at {level}: {e}", a.name));
            let Job::Mint {
                template, grade, ..
            } = job
            else {
                panic!("{} at {level}: {job:?}", a.name);
            };
            let item = cat.item(template).unwrap();
            let name = world.item_name(template).unwrap_or("?");
            println!(
                "{:<12} level {level}: mint {name} ({template}) item level {} grade {grade}",
                a.name, item.level
            );
            assert!(item.level <= level && item.wearable_by(level, 1), "{name}");
            assert!(looks_real(name), "{name}");
            assert!(fit.affinity(&item) >= plan::MINT_FIT, "{name}");
            assert_eq!(grade, item.grade_at(Tier::of_level(level).grade), "{name}");
            if item.class == GearClass::Armor && item.slot_type != slot_type::BACK {
                assert_eq!(item.kind, a.armor_type, "{name}: the build's armor");
            }
            assert_ne!(item.kind, armor_kind::PET, "{name}");
        }
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn a_lunagem_for_the_chest_and_a_quest_reward() {
    let Some(world) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set: skipped");
        return;
    };
    let gear = world.gear();
    let cat = WorldCatalog {
        world: &world,
        pos: [0.0, 0.0, 0.0],
        max_trip_m: 3_000.0,
    };
    let archetypes = Archetypes::builtin().unwrap();
    let a = archetypes.get("Abolisher").unwrap();
    let fit = Fit::of(a);
    // a level-30 plate chest at ancient grade with no gems
    let (chest, _) = gear
        .items()
        .find(|(_, i)| {
            i.slot_type == slot_type::CHEST
                && i.kind == armor_kind::PLATE
                && i.level == 30
                && i.flags & aa_world::gear::F_GRADABLE != 0
        })
        .expect("a level-30 plate chest");
    let worn = Worn::build([(equip_slot::CHEST, 77, chest, 4)], |_| Some(0), gear, &fit);
    assert!(
        gear.sockets(slot_type::CHEST, 4) > 0,
        "an ancient chest has sockets"
    );
    let rules = Rules {
        buy: false,
        mint: true,
        socket: true,
        regrade: false,
        max_spend_pct: 30,
        guild_budget: 0,
    };
    let mut bot = me(35, &fit, &worn, &[]);
    bot.fetched = &[];
    // (minting is on, but every slot but the chest is empty: ask for a gem directly)
    let job = plan::choose(&bot, &rules, &cat, Some(30_915));
    println!("socket job: {job:?}");
    let Ok(Job::Socket {
        target,
        gem,
        mint,
        chance,
        held,
        ..
    }) = job
    else {
        panic!("{job:?}");
    };
    assert_eq!(
        (target, gem, mint, chance, held),
        (77, 30_915, true, 10_000, 0)
    );
    assert_eq!(world.item_use_skill(gem), 23_728);
    // the rewards of quest 13
    let rewards = gear.quest_rewards(13);
    for &r in rewards {
        println!("reward {r}: {:?}", world.item_name(r));
    }
    let pick = pick_reward(rewards, &Worn::default(), gear, &fit, 5, 1, |t| {
        world.item_name(t)
    });
    println!("an Abolisher picks {pick:?}");
}
