//! Loads the real server data and checks the farming tables (crops, produce, growth times, seed
//! vendors) against what the server data says.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> \
//!   cargo test -p aa-world --test farm_real_data -- --ignored --nocapture
//! ```
//!
//! With `AA_BAKED=<dir with farm_areas.json>` the public farm polygons are checked too.

use std::path::PathBuf;

use aa_world::{FarmKind, StockAction, World, WorldPaths};

fn load() -> Option<World> {
    let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
    let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
    let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
    let _ = tracing_subscriber::fmt()
        .with_env_filter("aa_world=info")
        .with_test_writer()
        .try_init();
    Some(
        World::load(&WorldPaths {
            compact,
            server_data,
            baked,
            world: "main_world".into(),
        })
        .expect("load main_world"),
    )
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn the_potato_and_the_rice_grow_as_the_server_says() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let f = w.farm();
    // Potato Eyes (15659) plant the Potato doodad 2259: 60 s + 540 s of growth, harvest skill
    // 13980 in the ripe phase 4457 and in the withered phase 10043
    let potato = f.crop_of_seed(15659).expect("the potato is a crop");
    assert_eq!(potato.doodad, 2259);
    assert_eq!(potato.grow_ms, 600_000);
    assert!(potato.is_ripe(4457) && potato.is_ripe(10043));
    assert_eq!(potato.harvest_skill(4457), Some(13980));
    assert_eq!(potato.skill(), 13980);
    assert!(potato.allowed_in(FarmKind::Farm) || potato.kinds == 0);
    let potatoes = potato.yield_of(7992).expect("potatoes come out");
    assert_eq!((potatoes.min, potatoes.max), (2, 4));
    assert!(
        potato.yield_of(15659).is_some(),
        "the harvest gives the seed back"
    );
    assert!(f.crops_yielding(7992).any(|c| c.seed == 15659));
    assert!(f.is_crop_doodad(2259));
    // Rice (15646): about 2 h 52 min
    let rice = f.crop_of_seed(15646).expect("the rice is a crop");
    assert_eq!(rice.grow_ms, 10_320_000);
    assert_eq!(rice.skill(), 13980);
    assert!(rice.allowed_in(FarmKind::Farm));
    assert!(rice.plantable_on_land());
    // the grapevine sapling (4865) is a nursery tree
    let vine = f.crop_of_seed(4865).expect("the grapevine is a crop");
    assert!(vine.allowed_in(FarmKind::Nursery) && !vine.allowed_in(FarmKind::Farm));

    let chick = f.crop_of_seed(16230).expect("the chick can be raised");
    assert_eq!(chick.doodad, 2673);
    assert!(chick.allowed_in(FarmKind::Ranch));
    assert_eq!(
        f.stock_action(chick.doodad, 5792),
        Some(StockAction::Feed(13820))
    );
    assert_eq!(
        f.stock_action(chick.doodad, 5797),
        Some(StockAction::Treat(13794))
    );
    assert_eq!(
        f.stock_action(chick.doodad, 5798),
        Some(StockAction::Collect(13801))
    );
    assert_eq!(f.stock_action(chick.doodad, 5799), None);
    let eggs = f
        .livestock_yields(chick.doodad)
        .unwrap()
        .iter()
        .find(|y| y.item == 3603)
        .unwrap();
    assert_eq!((eggs.min, eggs.max), (2, 4));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn vendors_sell_the_everyday_seeds() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let f = w.farm();
    let mut sold = 0;
    for crop in f.crops() {
        let vendors = f.vendors_of(crop.seed);
        if vendors.is_empty() {
            continue;
        }
        sold += 1;
        for &npc in vendors {
            assert!(w.npc(npc).is_some_and(|t| t.is_merchant()));
            assert!(w.npc_spawns_anywhere(npc), "vendor {npc} has a spawn");
        }
    }
    eprintln!(
        "{} crops, {} of them sold by a vendor, {} farm areas",
        f.crops().len(),
        sold,
        f.areas().len()
    );
    assert!(sold >= 20, "the vendors sell many seeds, got {sold}");
    for seed in [15659, 15664, 15661, 15663, 15652, 15646] {
        assert!(
            !f.vendors_of(seed).is_empty(),
            "seed {seed} ({:?}) has a vendor",
            w.item_name(seed)
        );
    }
    // print the crops a bot could buy: what it costs, how long it takes, what it gives
    let mut rows: Vec<_> = f
        .crops()
        .iter()
        .filter(|c| f.is_sold(c.seed) && c.plantable_on_land())
        .collect();
    rows.sort_by_key(|c| c.grow_ms);
    for c in rows {
        let price = w.item_facts(c.seed).map_or(0, |i| i.price);
        let produce: Vec<String> = c
            .produce
            .iter()
            .filter(|y| y.item != c.seed)
            .map(|y| format!("{}x{}-{}", w.item_name(y.item).unwrap_or("?"), y.min, y.max))
            .collect();
        eprintln!(
            "{:>6} {:<22} price {:>5} grow {:>6} s skill {} kinds {:#06b} vendors {} -> {}",
            c.seed,
            w.item_name(c.seed).unwrap_or("?"),
            price,
            c.grow_ms / 1000,
            c.skill(),
            c.kinds,
            f.vendors_of(c.seed).len(),
            produce.join(", ")
        );
    }
}

#[test]
#[ignore = "needs AA_COMPACT, AA_SERVER_DATA and AA_BAKED with farm_areas.json"]
fn the_public_farms_are_baked() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let areas = w.farm().areas();
    if areas.is_empty() {
        eprintln!("no farm_areas.json in AA_BAKED; skipping");
        return;
    }
    for a in areas {
        eprintln!(
            "{:<8} sub zone {} zone {} centre ({:.0}, {:.0}) {:.0} m2 zone name {:?}",
            a.kind.name(),
            a.sub_zone,
            a.zone_key,
            a.centre[0],
            a.centre[1],
            a.area_m2(),
            w.zone_name(a.zone_key as u16)
        );
        assert!(a.area_m2() > 100.0, "a farm has room for plots");
        assert!(a.contains(a.centre[0], a.centre[1]) || a.area_m2() > 0.0);
    }
    assert!(
        areas.iter().any(|a| a.kind == FarmKind::Farm),
        "at least one field farm"
    );
}
