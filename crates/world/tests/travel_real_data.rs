//! Loads the real server data and checks the travel tables (portals, dungeons, instance
//! worlds, boats) against what the server data says.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> \
//!   cargo test -p aa-world --test travel_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use aa_world::travel::boat::slave_kind;
use aa_world::travel::ShipClass;
use aa_world::{Vec3, World, WorldPaths};

fn paths() -> Option<WorldPaths> {
    let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
    let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
    Some(WorldPaths {
        compact,
        server_data,
        baked: None,
        world: "main_world".into(),
    })
}

fn load() -> Option<World> {
    let paths = paths()?;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("aa_world=info")
        .with_test_writer()
        .try_init();
    Some(World::load(&paths).expect("load main_world"))
}

fn near(a: Vec3, x: f32, y: f32, z: f32, tol: f32) -> bool {
    (a.x - x).abs() <= tol && (a.y - y).abs() <= tol && (a.z - z).abs() <= tol
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn portal_facts() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let p = &w.travel().portals;
    let k = p.kit();

    // items, skills and gate NPCs of the portal system
    assert_eq!(k.book_item, 4045);
    assert_eq!(
        w.item_use_skill(4045),
        11216,
        "the Teleport Book casts the open skill"
    );
    assert_eq!((k.open.id, k.open.cast_ms), (11216, 10_000));
    assert_eq!((k.record.id, k.record.cast_ms), (11215, 5_000));
    assert_eq!(
        (k.ink_item, k.ink_amount),
        (4053, 1),
        "Memory Ink is the record reagent"
    );
    assert_eq!(k.rename.id, 16841);
    assert_eq!(
        (k.recall.id, k.recall.cast_ms, k.recall.cooldown_ms),
        (10594, 10_000, 1_800_000)
    );
    assert_eq!((k.bind.id, k.bind.cast_ms), (14602, 3_000));
    assert_eq!((k.entrance_npc, k.exit_npc), (3891, 6949));
    assert!(w.npc(3891).is_some() && w.npc(6949).is_some());

    // buffs the server refuses the skills for (`skill_reqs`): the aura around a Nui statue (2149)
    // lets the open and recall skills through but not the record skill
    assert!(p.blocking_buffs(11215).contains(&2149));
    assert!(!p.blocking_buffs(11216).contains(&2149));
    assert!(!p.blocking_buffs(10594).contains(&2149));
    assert!(
        p.blocking_buffs(11216).len() > 100,
        "stun, fear, landing, ... block the open skill too"
    );
    assert_eq!(k.open_distance, 3.0);
    assert_eq!(w.item_name(4045), Some("Teleport Book"));
    assert_eq!(w.item_name(4053), Some("Memory Ink"));
    assert_eq!(w.item_name(4052), Some("Hereafter Stone"));
    assert_eq!(w.item_name(417), Some("Bound Hereafter Stone"));

    // reagents: 1 stone on the same continent, 3 across
    let pairs = |same: bool| {
        w.portal_reagents(same)
            .iter()
            .map(|r| (r.item, r.amount))
            .collect::<Vec<_>>()
    };
    assert_eq!(pairs(true), vec![(417, 1), (4052, 1)]);
    assert_eq!(pairs(false), vec![(417, 3), (4052, 3)]);
    assert_eq!(p.continent_of_zone(179), 3, "Solzreed");
    assert_eq!(p.continent_of_zone(187), 4, "Haranya");
    assert!(!p.same_continent(179, 187));
    assert!(p.same_continent(184, 187));

    // recall points
    assert_eq!(w.recall_points().len(), 124);
    let distinct: std::collections::BTreeSet<u32> =
        w.recall_points().iter().map(|r| r.id).collect();
    assert_eq!(distinct.len(), 120, "recalls.json repeats four ids");
    let start = w.race_start(1).expect("nuian start");
    let nearest = w.nearest_recall(start).expect("a recall point");
    assert_eq!(nearest.id, 243);
    assert!(
        near(nearest.pos, 15578.04, 15382.12, 126.48, 0.1),
        "{nearest:?}"
    );
    assert_eq!((nearest.zone_key, nearest.sub_zone), (179, 324));
    assert_eq!(
        p.recall(240).map(|r| r.zone_key),
        Some(187),
        "Harani starting area"
    );
    assert_eq!(p.world_gates().len(), 133);
    assert_eq!(p.respawn_points().len(), 281);
    assert!(p.world_gate(372).is_some_and(|g| g.zone_key == 188));
    assert!(near(
        p.nearest_respawn(start).expect("a respawn").pos,
        15573.9,
        15384.4,
        126.2,
        0.1
    ));

    // Memory Tomes: doodad 841 binds district 15; the Nuian start has one nearby
    assert!(
        p.memory_tomes()
            .iter()
            .all(|t| t.skill == 14602 && w.doodad_spawns_anywhere(t.doodad)),
        "every tome spawns and is bound with skill 14602"
    );
    assert!(p.memory_tomes().len() > 100);
    let tome = p
        .nearest_memory_tome(start, 101)
        .expect("a tome near the start");
    assert_eq!((tome.doodad, tome.district), (850, 1));
    assert!(tome.pos.dist2d(start) < 300.0);
    assert_eq!(p.recall_target(1, 101).map(|r| r.id), Some(15), "Desireen");
    let harpa = p
        .memory_tomes()
        .iter()
        .find(|t| t.doodad == 841)
        .expect("the Harpa's Camp Memory Tome spawns");
    assert_eq!((harpa.skill, harpa.district), (14602, 15));
    assert!(
        near(harpa.pos, 10346.98, 16037.06, 357.88, 0.1),
        "{harpa:?}"
    );
    let harpa_recall = p.recall_target(15, 101).expect("a recall point for Nuians");
    assert_eq!(harpa_recall.id, 1);
    assert_eq!(p.recall_target(15, 103).map(|r| r.id), Some(1));
    assert!(p.recall_target(15, 187).is_none(), "no row for the Warborn");
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn dungeon_facts() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let t = w.travel();
    assert_eq!(w.dungeons().len(), 14);

    // (entrance doodad, skill, position, zone id, zone key, group, levels, max, party, item,
    //  closed, world)
    #[allow(clippy::type_complexity)]
    let table: [(
        u32,
        u32,
        (f32, f32, f32),
        u32,
        u16,
        u32,
        (u8, u8),
        u16,
        bool,
        u32,
        bool,
        &str,
    ); 6] = [
        (
            5075,
            19836,
            (11354.8, 13124.0, 143.8),
            184,
            262,
            50,
            (18, 55),
            3,
            false,
            0,
            false,
            "instance_cuttingwind_deadmine",
        ),
        (
            4288,
            17743,
            (19315.3, 8511.0, 196.25),
            170,
            240,
            47,
            (18, 55),
            3,
            false,
            0,
            false,
            "instance_sal_temple",
        ),
        (
            3543,
            18075,
            (15683.35, 12577.4, 166.9),
            168,
            236,
            45,
            (31, 55),
            5,
            false,
            0,
            false,
            "instance_burntcastle_armory",
        ),
        (
            4115,
            17731,
            (20027.4, 12773.9, 136.8),
            169,
            241,
            46,
            (31, 55),
            5,
            false,
            0,
            false,
            "instance_hadir_farm",
        ),
        (
            6805,
            20963,
            (23040.4, 25848.5, 140.2),
            192,
            278,
            55,
            (50, 55),
            10,
            true,
            27965,
            true,
            "instance_nachashgar",
        ),
        (
            5275,
            18701,
            (26872.3, 9043.5, 772.7),
            186,
            264,
            52,
            (45, 55),
            5,
            false,
            0,
            false,
            "instance_cradle_of_destruction",
        ),
    ];
    for (
        doodad,
        skill,
        (x, y, z),
        zone_id,
        zone_key,
        group,
        (lo, hi),
        max,
        party,
        item,
        closed,
        world,
    ) in table
    {
        let d = w
            .dungeon_by_entrance_doodad(doodad)
            .unwrap_or_else(|| panic!("no dungeon for doodad {doodad}"));
        let name = &d.name;
        assert_eq!(
            (d.zone_id, d.zone_key, d.zone_group),
            (zone_id, zone_key, group),
            "{name}"
        );
        assert_eq!(
            (d.level_min, d.level_max, d.max_players),
            (lo, hi, max),
            "{name}"
        );
        assert_eq!(
            (d.party_only, d.required_item, d.closed),
            (party, item, closed),
            "{name}"
        );
        assert_eq!(d.enter_count, 3, "{name}: EnterCount");
        let e = d
            .entrances
            .iter()
            .find(|e| e.doodad == doodad)
            .expect("entrance");
        assert_eq!(e.skill, skill, "{name}");
        assert!(
            e.spawns.iter().any(|s| near(*s, x, y, z, 0.2)),
            "{name}: {:?}",
            e.spawns
        );
        assert_eq!(
            w.doodad_use_skill(doodad),
            skill,
            "{name}: matches the doodad use skill"
        );
        assert_eq!(
            t.dungeons.world_of(d).map(|w| &*w.name),
            Some(world),
            "{name}"
        );
        assert!(w
            .instance_world_by_zone_key(zone_key)
            .is_some_and(|iw| *iw.name == *world));
        assert!(d.accepts_level(lo) && !d.accepts_level(lo - 1));
    }
    assert_eq!(
        w.dungeon_by_entrance_doodad(4288).map(|d| &*d.name),
        Some("Palace Cellar")
    );
    assert_eq!(
        w.dungeon_by_entrance_doodad(5075).map(|d| &*d.name),
        Some("Sharpwind Mines")
    );

    // the Howling Abyss gate opens two dungeons; the two skills tell them apart
    let abyss: Vec<(u32, u32)> = w
        .dungeons_by_entrance_doodad(5085)
        .map(|d| {
            (
                d.zone_group,
                d.entrances.iter().find(|e| e.doodad == 5085).unwrap().skill,
            )
        })
        .collect();
    assert_eq!(abyss, vec![(51, 18339), (58, 21401)]);

    // every dungeon has an instance world, and the library halls share one
    for d in w.dungeons() {
        assert!(t.dungeons.world_of(d).is_some(), "{}", d.name);
    }
    let library: Vec<u32> = w
        .dungeons()
        .iter()
        .filter(|d| {
            t.dungeons
                .world_of(d)
                .is_some_and(|iw| &*iw.name == "instance_library")
        })
        .map(|d| d.zone_group)
        .collect();
    assert_eq!(library, vec![73, 74, 75, 76]);
    assert_eq!(
        w.dungeons().iter().filter(|d| d.closed).count(),
        2,
        "Serpentis and Eternal Isle"
    );
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn instance_world_facts() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let worlds = w.travel().dungeons.worlds();
    assert_eq!(worlds.all().len(), 17);
    assert!(w.instance_world("main_world").is_none());
    for iw in worlds.all() {
        // the spawn point of every zone of a world is one of the world's zones
        for s in iw.spawns.iter() {
            assert!(iw.zone_keys.contains(&s.zone_key), "{}: {s:?}", iw.name);
        }
        assert_eq!(
            iw.npc_stats.missing_z + iw.doodad_stats.missing_z,
            0,
            "{}",
            iw.name
        );
    }

    let mine = w
        .instance_world("instance_cuttingwind_deadmine")
        .expect("deadmine");
    assert_eq!(&*mine.zone_keys, &[262]);
    let spawn = mine.spawn_for_zone(262).expect("spawn point");
    assert!(near(spawn.pos, 796.3, 325.4, 249.5, 0.1), "{spawn:?}");
    assert_eq!(mine.exits().len(), 2);
    assert!(mine
        .exits()
        .iter()
        .all(|e| e.template == 5723 && e.skill == 19754 && e.return_point == 0));
    assert!(mine
        .exits()
        .iter()
        .any(|e| near(e.pos, 804.1, 327.7, 248.1, 0.1)));
    assert!(mine
        .exits()
        .iter()
        .any(|e| near(e.pos, 739.4, 326.7, 166.2, 0.1)));
    let near_exit = mine.nearest_exit(spawn.pos).expect("an exit");
    assert!(
        near(near_exit.pos, 804.1, 327.7, 248.1, 0.1),
        "the exit next to the spawn"
    );
    for boss in [12150, 11362, 11363, 12188] {
        let n = mine
            .npcs()
            .iter()
            .find(|n| n.template == boss)
            .unwrap_or_else(|| panic!("boss {boss} does not spawn"));
        assert_eq!(Some(n.level), w.npc(boss).map(|t| t.level));
        assert!((20..=21).contains(&n.level));
    }
    assert!(mine.npcs().iter().any(|n| n.aggressive));
    assert!(mine.npcs().len() > 50 && mine.npcs().len() < 100);

    let sal = w.instance_world("instance_sal_temple").expect("sal temple");
    assert!(sal
        .exits()
        .iter()
        .any(|e| e.template == 4535 && e.skill == 16560));
    let hadir = w.instance_world("instance_hadir_farm").expect("hadir farm");
    let hadir_exits: Vec<(u32, u32)> = hadir
        .exits()
        .iter()
        .map(|e| (e.template, e.skill))
        .collect();
    assert_eq!(hadir_exits, vec![(4289, 17733), (4927, 17733)]);

    // the library holds seven zones, each with its spawn point
    let library = w.instance_world("instance_library").expect("library");
    assert_eq!(&*library.zone_keys, &[296, 297, 298, 303, 304, 305, 306]);
    assert!(library
        .spawn_for_zone(297)
        .is_some_and(|s| &*s.name == "instance_library_2"));
    assert!(w
        .instance_world_by_zone_key(260)
        .is_some_and(|iw| &*iw.name == "arche_mall_world"));
    assert!(w.instance_world_by_zone_key(9999).is_none());

    // event-driven instances have few static spawns; the counters say how many are left out
    let nachashgar = w.instance_world("instance_nachashgar").expect("nachashgar");
    assert!(nachashgar.npc_stats.event_spawners > 50);
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn boat_facts() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let scrolls = w.boat_scrolls();
    assert_eq!(scrolls.len(), 26);
    assert!(scrolls
        .windows(2)
        .all(|p| (p[0].price, p[0].item) <= (p[1].price, p[1].item)));
    assert_eq!(scrolls[0].item, 17906);
    assert_eq!(scrolls[0].price, 0);

    let sharani = &scrolls[0];
    assert_eq!(&*sharani.name, "Scroll: Sharani's Rowboat");
    assert_eq!(
        (
            sharani.skill.id,
            sharani.skill.cast_ms,
            sharani.skill.cooldown_ms
        ),
        (15802, 5_000, 10_000)
    );
    assert_eq!(w.item_use_skill(17906), 15802);
    for (item, price, slave) in [(17906, 0, 15), (15817, 10_000, 15), (17626, 50_000, 52)] {
        let s = scrolls
            .iter()
            .find(|s| s.item == item)
            .unwrap_or_else(|| panic!("scroll {item}"));
        assert_eq!((s.price, s.slave), (price, slave), "item {item}");
        assert_eq!(w.boat_for_scroll(item).map(|b| b.id), Some(slave));
    }

    let rowboat = w.slave_template(15).expect("rowboat");
    assert_eq!(&*rowboat.name, "Rowboat");
    assert_eq!(
        (rowboat.model, rowboat.kind, rowboat.level),
        (129, slave_kind::BOAT, 50)
    );
    assert!(rowboat.has_driver_seat());
    assert_eq!(rowboat.passenger_seats(), 1);
    assert_eq!(rowboat.seat_points().collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(rowboat.velocity, 6.4);
    assert_eq!(
        rowboat.move_speed_mul, 2.0,
        "+1000 flat MoveSpeedMul on a base of 1000"
    );
    // the body moves at speed * msm / 4 with the speed capped at velocity * msm / 2
    assert!((rowboat.top_speed() - 3.2).abs() < 1e-4);
    assert_eq!(rowboat.max_hp, 6216);
    assert_eq!(rowboat.summon_range, 50);

    let clipper = w.slave_template(52).expect("clipper");
    assert_eq!(&*clipper.name, "Clipper");
    assert_eq!((clipper.model, clipper.kind), (128, slave_kind::SPEEDBOAT));
    assert!(clipper.has_driver_seat() && clipper.passenger_seats() == 0);
    assert!((clipper.top_speed() - 6.5).abs() < 1e-4);

    // only boats, and every scroll resolves
    for s in scrolls {
        let b = w
            .boat_for_scroll(s.item)
            .unwrap_or_else(|| panic!("scroll {}", s.item));
        assert!(b.is_boat() && b.has_driver_seat(), "{}", b.name);
    }
    let b = &w.travel().boats;
    assert_eq!(b.cheapest_scroll(0.0).map(|s| s.item), Some(17906));
    assert_eq!(
        b.cheapest_scroll(5.0).map(|s| s.item),
        Some(27266),
        "the free harpoon clipper, 6.5 m/s"
    );
    assert_eq!(
        b.cheapest_scroll(9.0).map(|s| s.item),
        Some(13711),
        "the 9.8 m/s yawl"
    );
}

/// The ship classes of the hulls the bots sail: the figures of `ship_models` and the kind's
/// turn caps (`systems/ai/physics/ship.rs`) summed up.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn ship_classes() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let b = &w.travel().boats;
    for t in b.slave_templates() {
        let c = t.class();
        println!(
            "{:>4} {:<28} kind {:>2} {:?} top {:.1} m/s radius {:>4.0} m box {:.1} x {:.1} x {:.1}              draft {:.1} spawn {:.0}/{:.0} portal {} ms cannons {} racks {} seats {} clear {}",
            t.id,
            t.name,
            t.kind,
            c.size,
            c.top_speed,
            c.radius,
            c.beam,
            c.length,
            c.height,
            c.draft,
            c.spawn_ahead,
            c.spawn_side,
            c.portal_ms,
            c.cannons,
            c.racks,
            c.passengers,
            c.route_clear()
        );
    }
    let class = |id: u32| {
        b.slave_template(id)
            .unwrap_or_else(|| panic!("slave {id}"))
            .class()
    };
    let near = |a: f32, b: f32, tol: f32| (a - b).abs() <= tol;
    // rowboat 15: 3.2 m/s, 4.35 degrees a second
    let r = class(15);
    assert_eq!(r.size, aa_world::travel::HullSize::Small);
    assert!(
        near(r.top_speed, 3.2, 1e-3) && near(r.radius, 42.0, 1.0),
        "{r:?}"
    );
    assert_eq!((r.spawn_ahead, r.portal_ms, r.passengers), (1.5, 0, 1));
    // clipper 52 and the Adventurer's 76 (one cannon at point 10), harpoon clipper 14
    let c = class(52);
    assert!(
        near(c.top_speed, 6.5, 1e-3) && near(c.radius, 64.0, 1.0),
        "{c:?}"
    );
    assert_eq!((c.beam, c.length, c.height), (4.0, 8.0, 7.0));
    assert_eq!((c.spawn_ahead, c.spawn_side, c.portal_ms), (5.0, 5.0, 7000));
    assert_eq!(class(76).cannons, 1);
    assert_eq!(class(76).spawn_ahead, 25.0);
    assert_eq!(
        b.slave_template(76)
            .unwrap()
            .cannon_points()
            .collect::<Vec<_>>(),
        vec![10]
    );
    assert_eq!(class(14).cannons, 0, "the harpoon gun is not a cannon");
    // cutter 21 and junk 92: 8 cannons (points 9..=16), 4 pack racks (60..=63)
    for id in [21, 92] {
        let c = class(id);
        assert_eq!(c.size, aa_world::travel::HullSize::Large, "{id}");
        assert!(
            near(c.top_speed, 6.3, 1e-3) && near(c.radius, 139.0, 1.0),
            "{c:?}"
        );
        assert_eq!((c.beam, c.length, c.height), (6.5, 30.0, 14.5));
        assert_eq!(
            (c.spawn_ahead, c.spawn_side, c.portal_ms),
            (30.0, 20.0, 7000)
        );
        assert_eq!((c.cannons, c.racks, c.passengers), (8, 4, 0), "{id}");
        assert_eq!(c.route_clear(), 5);
    }
    assert_eq!(
        b.slave_template(21)
            .unwrap()
            .cannon_points()
            .collect::<Vec<_>>(),
        (9..=16).collect::<Vec<u8>>()
    );
    assert_eq!(
        b.slave_template(21).unwrap().rack_points(),
        &[60, 61, 62, 63]
    );
    // yawl 24 and the Merchant Schooner 75: 2 cannons (points 9 and 10), 20 racks (60..=79)
    let y = class(24);
    assert!(
        near(y.top_speed, 9.8, 1e-3) && near(y.radius, 216.0, 1.0),
        "{y:?}"
    );
    let s = class(75);
    assert!(
        near(s.top_speed, 6.5, 1e-3) && near(s.radius, 143.0, 1.5),
        "{s:?}"
    );
    assert_eq!((s.cannons, s.racks), (2, 20));
    assert_eq!(
        b.slave_template(75).unwrap().rack_points(),
        (60..=79).collect::<Vec<u8>>()
    );
    // the constants the bots' tests use are these hulls
    for (id, k) in [
        (15, ShipClass::ROWBOAT),
        (52, ShipClass::CLIPPER),
        (21, ShipClass::CUTTER),
    ] {
        let c = class(id);
        let same = |a: f32, b: f32| (a - b).abs() <= 0.01 * b.abs().max(1.0);
        assert_eq!((c.slave, c.kind, c.size), (k.slave, k.kind, k.size), "{id}");
        for (name, a, b) in [
            ("top", c.top_speed, k.top_speed),
            ("rate", c.turn_rate, k.turn_rate),
            ("accel", c.turn_accel, k.turn_accel),
            ("radius", c.radius, k.radius),
            ("beam", c.beam, k.beam),
            ("length", c.length, k.length),
            ("height", c.height, k.height),
            ("draft", c.draft, k.draft),
            ("ahead", c.spawn_ahead, k.spawn_ahead),
            ("side", c.spawn_side, k.spawn_side),
            ("brake", c.brake, k.brake),
            ("accel", c.accel, k.accel),
            ("depth", c.box_depth, k.box_depth),
        ] {
            assert!(same(a, b), "{id} {name}: {a} vs {b}");
        }
        assert_eq!(
            (c.portal_ms, c.cannons, c.racks, c.passengers),
            (k.portal_ms, k.cannons, k.racks, k.passengers),
            "{id}"
        );
    }
    // the families resolve to a scroll each
    for f in aa_world::travel::ShipFamily::ALL {
        let s = b
            .scroll_for(f, |_| 0)
            .unwrap_or_else(|| panic!("no scroll for {}", f.name()));
        println!("{:<9} -> item {} ({})", f.name(), s.item, s.name);
    }
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn travel_memory_report() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let t = w.travel();
    let p = &t.portals;
    println!(
        "recall points {}  respawn points {}  world gates {}  memory tomes {}",
        p.recall_points().len(),
        p.respawn_points().len(),
        p.world_gates().len(),
        p.memory_tomes().len()
    );
    println!(
        "dungeons {}  instance worlds {}  boat scrolls {}  boat slaves {}",
        w.dungeons().len(),
        t.dungeons.worlds().all().len(),
        w.boat_scrolls().len(),
        t.boats.slave_templates().len()
    );
    for iw in t.dungeons.worlds().all() {
        println!(
            "  {:<32} zones {:?} npcs {:>4} doodads {:>4} exits {} (event spawners left out: {}, unknown templates: {}+{})",
            iw.name,
            iw.zone_keys,
            iw.npcs().len(),
            iw.doodads().len(),
            iw.exits().len(),
            iw.npc_stats.event_spawners,
            iw.npc_stats.unknown_template,
            iw.doodad_stats.unknown_template
        );
    }
    let total: usize = t.memory_breakdown().iter().map(|(_, b)| *b).sum();
    for (name, bytes) in t.memory_breakdown() {
        println!("  {name:<40} {:>8} bytes", bytes);
    }
    println!(
        "travel heap total: {total} bytes ({:.0} KiB)",
        total as f64 / 1024.0
    );
    assert_eq!(total, t.bytes());
    assert!(total < 512 * 1024, "travel data must stay small: {total}");
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn mirage_isle_portals_arrival_and_exit() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let m = w.mirage().expect("the Mirage Isle portal is in the data");
    println!(
        "mirage: zone {} key {} world {} portals {} arrival {:?} exit {:?} meeting {:?}",
        m.zone_id,
        m.zone_key,
        m.world,
        m.portal.spawns.len(),
        m.arrival,
        m.exit,
        m.meeting_point
    );
    assert_eq!((m.zone_id, m.zone_key), (183, 260));
    assert_eq!((m.portal.doodad, m.portal.skill), (4894, 17829));
    assert_eq!(&*m.world, "arche_mall_world");
    // one portal beside each auctioneer of the towns
    assert!(m.portal.spawns.len() > 50, "{}", m.portal.spawns.len());
    // Austera: the portal is within a few metres of the auctioneer
    let near = m
        .nearest_portal(Vec3::new(16700.6, 9036.6, 0.0))
        .expect("a portal near Austera");
    assert!(
        ((near.x - 16700.6).powi(2) + (near.y - 9036.6).powi(2)).sqrt() < 40.0,
        "{near:?}"
    );
    assert!(near_xy(m.arrival, 3680.5, 4572.2, 10.0), "{:?}", m.arrival);
    let (exit, pos, skill) = m.exit.expect("an exit doodad");
    assert_eq!((exit, skill), (4895, 17838));
    assert!(near_xy(pos, 3683.9, 4545.5, 5.0), "{pos:?}");
    assert!(
        near_xy(m.meeting_point, 3675.0, 4560.1, 5.0),
        "{:?}",
        m.meeting_point
    );
    // bots keep both doors in view (neither offers anything a bot could use otherwise)
    assert!(w.is_mirage_door(4894) && w.is_mirage_door(4895) && !w.is_mirage_door(532));
    // the island is no dungeon: the dungeon goal must never pick it
    assert!(w.dungeons().iter().all(|d| d.zone_group != 49));
    assert!(w.dungeon_by_entrance_doodad(4894).is_none());
    // the islands's world is known by its zone key
    assert_eq!(
        w.instance_world_by_zone_key(260).map(|i| &*i.name),
        Some("arche_mall_world")
    );
}

fn near_xy(p: Vec3, x: f32, y: f32, tol: f32) -> bool {
    (p.x - x).abs() <= tol && (p.y - y).abs() <= tol
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn mounts_and_gliders_of_the_real_data() {
    let Some(w) = load() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let m = w.mounts();
    println!(
        "mounts: {} rideable summon items, {} gliders",
        m.mount_count(),
        m.glider_count()
    );
    // The 1.2 default of `[mounts] item = 0`: the Lilyut horse.
    let horse = m.mount(4177).expect("item 4177 summons the Lilyut horse");
    assert_eq!(horse.npc, 3599);
    assert_eq!(horse.use_skill, 10602);
    assert_eq!(m.default_mount(0), Some(4177));
    // The 1.2 default of `[mounts] glider_item = 0`: the old glider.
    let glider = m.glider(16385).expect("item 16385 is Tooloop's old glider");
    assert_eq!(glider.use_skill, 19230);
    assert_eq!(m.default_glider(0), Some(16385));
    // Every loaded mount has a summon skill and a driver seat (the loader drops the rest).
    for (item, row) in m.iter_mounts().take(20) {
        assert_ne!(row.use_skill, 0, "mount {item} has no use skill");
    }
}
