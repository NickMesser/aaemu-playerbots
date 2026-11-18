//! Plans trade runs against the real `main_world` data, without a server.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> \
//!   cargo test -p playerbots --test trade_run_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_world::{World, WorldPaths};
use playerbots::bot::goals::sail::trip::{danger_zones, plan_passage, Kind, TripCfg};
use playerbots::bot::goals::trade_run::plan::{candidates, carry_only, Plan, Query};
use playerbots::bot::goals::trade_run::route_risky;

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
            let paths = WorldPaths {
                compact,
                server_data,
                baked,
                world: "main_world".into(),
            };
            Some(Arc::new(World::load(&paths).expect("load main_world")))
        })
        .clone()
}

/// Where a new Nuian starts (Solzreed Peninsula).
const NUIAN_START: [f32; 2] = [15578.0, 15382.0];

fn describe(w: &World, p: &Plan) -> String {
    format!(
        "craft {} {:?} -> {:?} at bench {} {:?} ({:.0} m away, {:.0},{:.0}) -> {} {:?} ({:.0} m, {:.0},{:.0})",
        p.craft,
        w.craft(p.craft).map(|c| &*c.name),
        w.item_name(p.item),
        p.bench_tpl,
        w.doodad(p.bench_tpl).map(|d| &*d.name),
        p.to_bench_m,
        p.bench[0],
        p.bench[1],
        p.buyer_tpl,
        w.npc(p.buyer_tpl).map(|n| &*n.name),
        p.carry_m,
        p.buyer[0],
        p.buyer[1],
    )
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn routes_exist_near_the_nuian_start() {
    let Some(w) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set");
        return;
    };
    let packs = w.pack_crafts().count();
    println!("{packs} pack crafts");
    let q = Query {
        from: NUIAN_START,
        max_route_m: 3_000.0,
        radius_m: 3_000.0,
        craft_ok: &|_| true,
        bench_ok: &|_, _| true,
        buyer_ok: &|_| true,
    };
    let c = candidates(&*w, &q);
    println!("{} routes within 3 km of the start", c.len());
    for budget in [3_000.0, 5_000.0, 8_000.0, 12_000.0] {
        let q = Query {
            max_route_m: budget,
            radius_m: budget,
            ..q
        };
        let c = candidates(&*w, &q);
        println!("{} routes within {budget} m of the start:", c.len());
        for p in c.iter().take(6) {
            println!(
                "  {} (score {:.0}, total {:.0})",
                describe(&w, p),
                p.score(),
                p.total_m()
            );
        }
        for p in &c {
            assert!(p.total_m() <= budget + 0.5);
            assert!(w.craft(p.craft).is_some_and(|c| c.is_pack));
            assert!(w.specialty_buyers(p.item).contains(&p.buyer_tpl));
            assert!(w.pack_crafts_at(p.bench_tpl).contains(&p.craft));
        }
    }
    let wide = Query {
        max_route_m: f32::INFINITY,
        radius_m: 40_000.0,
        ..q
    };
    assert!(!candidates(&*w, &wide).is_empty());
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn the_sweeteners_sell_at_misty_in_solzreed() {
    let Some(w) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set");
        return;
    };
    // Marianople Sweeteners (31855) are bought by NPC 10664 (Solzreed), 14766 and more.
    let buyers = w.specialty_buyers(31855);
    println!("buyers of 31855: {buyers:?}");
    assert!(buyers.contains(&10664));
    let p = carry_only(&*w, 31855, [15578.0, 15382.0], &|n| n == 10664).expect("a Solzreed buyer");
    println!("{}", describe(&w, &p));
    assert!(p.carry_m < 3_000.0);
}

#[test]
#[ignore = "needs AA_COMPACT, AA_SERVER_DATA and AA_BAKED"]
fn a_clipper_can_take_a_real_pack_buyer_route_between_continents() {
    let w = world().expect("AA_COMPACT and AA_SERVER_DATA");
    let craft = w.craft(6223).expect("Rokhala pack recipe");
    let bench = craft
        .benches
        .iter()
        .flat_map(|&tpl| w.doodad_spawns(tpl))
        .find(|p| (p.x - 20307.0).abs() < 5.0 && (p.y - 13181.0).abs() < 5.0)
        .expect("authored specialty bench");
    assert!(w.specialty_buyers(craft.product.0).contains(&4478));
    let buyer = &w.npc_spawns(4478)[0];
    let continent = |x, y| {
        w.travel()
            .portals
            .continent_of_zone(w.zone_key_at(x, y).unwrap())
    };
    assert_eq!(continent(bench.x, bench.y), 4);
    assert_eq!(continent(buyer.x, buyer.y), 3);
    let cfg = TripCfg::for_ship(aa_world::travel::ShipClass::CLIPPER);
    let ground = |x, y| w.ground_z(x, y);
    let midpoint = ((bench.x + buyer.x) * 0.5, (bench.y + buyer.y) * 0.5);
    let danger = danger_zones(
        &w,
        w.water().expect("baked sea terrain"),
        101,
        midpoint,
        5_000.0,
    );
    let route = plan_passage(
        w.water_for(&cfg.ship).unwrap(),
        &ground,
        (bench.x, bench.y),
        (buyer.x, buyer.y),
        &cfg,
        &danger,
    )
    .expect("connected sea route");
    assert_eq!(route.kind, Kind::Cross);
    assert!((4_500.0..7_000.0).contains(&route.route_m));
    println!(
        "pack {}, boat leg {:.0} m, buyer 4478",
        craft.product.0, route.route_m
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn survey_of_the_specialty_workbenches() {
    let Some(w) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set");
        return;
    };
    let from = aa_world::Vec3::new(NUIAN_START[0], NUIAN_START[1], 0.0);
    let mut benches: std::collections::BTreeMap<u32, Vec<aa_world::Vec3>> = Default::default();
    for c in w.pack_crafts() {
        for &b in c.benches.iter() {
            let sp = w.doodad_spawns(b);
            benches.entry(b).or_default().extend_from_slice(sp);
        }
    }
    println!("{} bench templates offer pack crafts", benches.len());
    for (tpl, spawns) in &benches {
        let nearest = spawns
            .iter()
            .map(|s| s.dist2d(from))
            .fold(f32::INFINITY, f32::min);
        println!(
            "  bench {tpl} {:?}: {} crafts, {} spawns, nearest {:.0} m",
            w.doodad(*tpl).map(|d| &*d.name),
            w.pack_crafts_at(*tpl).len(),
            spawns.len(),
            nearest
        );
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn safe_routes_remain_near_the_nuian_start() {
    let Some(w) = world() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set");
        return;
    };
    let faction = w.race_faction(1).expect("the Nuian faction");
    let q = Query {
        from: NUIAN_START,
        max_route_m: 8_000.0,
        radius_m: 8_000.0,
        craft_ok: &|_| true,
        bench_ok: &|_, _| true,
        buyer_ok: &|_| true,
    };
    let cands = candidates(&*w, &q);
    let safe: Vec<&Plan> = cands
        .iter()
        .filter(|p| !route_risky(&w, faction, 10, NUIAN_START, p))
        .collect();
    println!(
        "{} of {} routes are safe for a level-10 Nuian",
        safe.len(),
        cands.len()
    );
    for p in &safe {
        println!("  {}", describe(&w, p));
    }
    assert!(!safe.is_empty(), "every route near the start is risky");
}
