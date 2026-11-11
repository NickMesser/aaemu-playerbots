//! Loads the real `main_world` from the AAEmu server data and checks well-known facts.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p aa-world -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::time::Instant;

use aa_world::{ObjKind, QuestAtlas, QuestReport, QuestStart, Relation, Vec3, World, WorldPaths};

fn paths() -> Option<WorldPaths> {
    let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
    let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
    let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
    Some(WorldPaths {
        compact,
        server_data,
        baked,
        world: "main_world".into(),
    })
}

fn near(a: Vec3, x: f32, y: f32, z: f32, tol: f32) -> bool {
    (a.x - x).abs() <= tol && (a.y - y).abs() <= tol && (a.z - z).abs() <= tol
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn main_world_facts() {
    let Some(paths) = paths() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter("aa_world=info")
        .with_test_writer()
        .try_init();

    let t = Instant::now();
    let w = World::load(&paths).expect("load main_world");
    let load = t.elapsed();

    // The closed zone keys of the data (Aubre Cradle, Airain Rock, Rainbow Field 1, ...) and the
    // quest atlas that must not send a bot there.
    assert!(w.zone_closed(150), "w_cradle_of_genesis (Aubre Cradle)");
    assert!(w.zone_closed(154), "w_bronze_rock_1 (Airain Rock)");
    assert!(w.zone_closed(146), "e_rainbow_field_1");
    assert!(!w.zone_closed(129), "w_gweonid_forest_1 is open");
    let atlas = QuestAtlas::build(&w, &|_| true);
    let in_closed: Vec<(u32, u16)> = atlas
        .points()
        .iter()
        .filter(|p| w.zone_closed(p.zone))
        .map(|p| (p.quest, p.zone))
        .collect();
    assert!(
        in_closed.is_empty(),
        "quests are planned in closed zones: {in_closed:?}"
    );

    // The 13 quests whose item-use skill has a target NPC group (unit_reqs 0x22); the bot must
    // have a group member to cast at (2394 negates the group: any other NPC).
    let group_quests = [
        2261u32, 2394, 2565, 134, 1769, 1790, 3381, 955, 3828, 4212, 4251, 614, 261,
    ];
    for quest in group_quests {
        let q = w.quest(quest).unwrap_or_else(|| panic!("quest {quest}"));
        let mut found = false;
        for step in q.progress.iter() {
            for obj in step.objectives.iter() {
                let ObjKind::UseItem { item, .. } = &obj.kind else {
                    continue;
                };
                let skill = w.item_use_skill(*item);
                let needs = w.skill_needs(skill);
                let Some((group, negate)) = needs.iter().find_map(|n| match n {
                    aa_world::SkillNeed::NpcGroup(group, negate) => Some((*group, *negate)),
                    _ => None,
                }) else {
                    continue;
                };
                let members = w.quest_monster_group(group);
                assert!(!members.is_empty(), "quest {quest}: group {group} is empty");
                if !negate {
                    assert!(
                        members.iter().any(|m| w.npc_spawns_anywhere(*m)),
                        "quest {quest}: no member of group {group} spawns"
                    );
                }
                found = true;
            }
        }
        assert!(found, "quest {quest} uses no target-group item skill");
    }

    // ---- report -------------------------------------------------------------------------
    let ns = w.npc_spawn_stats();
    let ds = w.doodad_spawn_stats();
    println!("load time            : {load:?}");
    println!("npc templates        : {}", w.npc_templates().len());
    println!("npc spawn files/entries/kept-dups/unknown/event/noZ: {ns:?}");
    println!("doodad spawn stats   : {ds:?}");
    let q = w.quest_stats();
    println!(
        "quests               : {} ({} with objectives)",
        q.quests, q.with_objectives
    );
    println!("objectives by kind   : {:?}", q.objectives);
    println!("Other objective types: {:?}", q.other_classes);
    println!(
        "start Other: {}   report Unknown: {}   gather without NPC dropper: {}",
        q.start_other, q.report_unknown, q.gather_without_droppers
    );
    let (mut gather, mut no_source) = (0, 0);
    for quest in w.quests() {
        for step in quest.progress.iter() {
            for o in step.objectives.iter() {
                if let ObjKind::Gather { item, droppers, .. } = &o.kind {
                    gather += 1;
                    let supplied = quest.supplies.iter().any(|(i, _)| i == item);
                    if droppers.is_empty() && w.gather_doodads(*item).is_empty() && !supplied {
                        no_source += 1;
                    }
                }
            }
        }
    }
    println!(
        "gather objectives: {gather}; supplied by the quest: {}; no NPC dropper: {};          no dropper, no doodad, not supplied: {no_source}",
        q.gather_supplied, q.gather_without_droppers
    );
    let (mut kill_started, mut no_start_component, mut story) = (0, 0, 0);
    let mut starts = std::collections::BTreeMap::<&str, usize>::new();
    for quest in w.quests() {
        story += usize::from(quest.story);
        let kind = match quest.start {
            QuestStart::Npc(_) => "Npc",
            QuestStart::Doodad(_) => "Doodad",
            QuestStart::Item(_) => "Item",
            QuestStart::Sphere(_) => "Sphere",
            QuestStart::Level(_) => "Level",
            QuestStart::AfterQuest(_) => "AfterQuest",
            QuestStart::Other if !quest.kill_start.is_empty() => {
                kill_started += 1;
                "Other(kill)"
            }
            QuestStart::Other => {
                no_start_component += usize::from(quest.start_component == 0);
                "Other(none)"
            }
        };
        *starts.entry(kind).or_default() += 1;
    }
    println!(
        "start kinds: {starts:?} (of the none: {no_start_component} have no start component);          kill-started: {kill_started}; story quests: {story}"
    );
    let mem = w.memory_estimate();
    println!(
        "memory estimate      : {} KiB ({:.1} MiB)",
        mem / 1024,
        mem as f64 / 1048576.0
    );
    for (name, bytes) in w.memory_breakdown() {
        println!("  {name:<32} {:>8} KiB", bytes / 1024);
    }

    // ---- spawn files ----------------------------------------------------------------------
    assert!(ns.entries > 25_000, "npc entries {}", ns.entries);
    assert!(ds.entries > 42_000, "doodad entries {}", ds.entries);

    // ---- characters -----------------------------------------------------------------------
    let nuian = w.race_start(1).expect("nuian start");
    assert!(near(nuian, 15578.04, 15382.12, 126.48, 0.05), "{nuian:?}");
    assert!(w.creatable(1, 1) && w.creatable(1, 2));
    assert!(!w.creatable(3, 1));
    assert_eq!(w.race_faction(1), Some(101));

    // ---- npcs and spawns ------------------------------------------------------------------
    let maude = w.npc(3597).expect("npc 3597");
    println!(
        "npc 3597 = {:?} level {} faction {}",
        maude.name, maude.level, maude.faction_id
    );
    assert!(maude.is_quest_giver);
    let spawn = w
        .nearest_npc_spawn(3597, Vec3::new(15562.0, 15354.0, 128.0))
        .expect("3597 spawns");
    assert!(near(spawn, 15562.5, 15354.3, 128.0, 1.0), "{spawn:?}");
    let mut near_start = 0;
    w.npc_spawns_within(nuian, 100.0, |_, _| near_start += 1);
    println!("npc spawns within 100 m of the Nuian start: {near_start}");
    assert!(near_start >= 1);
    assert!(w.npc(14766).expect("specialty npc").is_specialty());

    // ---- relations ------------------------------------------------------------------------
    let boar = w.npc(3475).expect("boar");
    assert_eq!(boar.faction_id, 115);
    assert_eq!(w.relation(101, 101), Relation::Friendly);
    assert_eq!(w.relation(101, boar.faction_id), Relation::Hostile);

    // ---- quests ---------------------------------------------------------------------------
    let q330 = w.quest(330).expect("quest 330");
    assert_eq!(&*q330.name, "A Friendly Reminder");
    assert_eq!(q330.start, QuestStart::Npc(3597));
    assert_eq!(q330.report, QuestReport::Npc(3511));
    assert_eq!(w.quests_from_npc(3597).first(), Some(&330));
    println!(
        "quest 330 progress steps: {} (the database holds no progress component)",
        q330.progress.len()
    );

    let q250 = w.quest(250).expect("quest 250");
    assert_eq!(&*q250.name, "Fox Infestation");
    assert_eq!(q250.start, QuestStart::Doodad(5047));
    assert!(w.quests_from_doodad(5047).contains(&250));
    let hunt = &q250.progress[0].objectives[0];
    assert_eq!(
        hunt.kind,
        ObjKind::Hunt {
            npcs: Box::new([3492]),
            count: 3
        }
    );

    let q251 = w.quest(251).expect("quest 251");
    match &q251.progress[0].objectives[0].kind {
        ObjKind::Gather {
            item,
            count,
            droppers,
        } => {
            assert_eq!((*item, *count), (4058, 3));
            assert!(droppers.contains(&3475), "{droppers:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(q251.report, QuestReport::Npc(3512));
    assert_eq!(w.item_name(4058), Some("Solzreed Boar Meat"));
    // zone ids vs zone keys
    assert_eq!(q250.zone_id, 125);
    assert_eq!(w.zone_key_for_id(125), Some(179));
    assert_eq!(w.zone_name(179), Some("Solzreed Peninsula"));
    // tutorial rows are not quests
    assert!(w.quests().all(|q| q.category != 45));

    // ---- crafts and trade packs -----------------------------------------------------------
    let craft = w.craft(6226).expect("craft 6226");
    println!("craft 6226: {craft:?}");
    assert!(craft.is_pack);
    assert_eq!(craft.product.0, 31855);
    assert!(craft.benches.contains(&4233));
    assert!(craft.labor > 0);
    assert!(w.pack_crafts_at(4233).contains(&6226));
    assert!(w.specialty_buyers(31855).contains(&14766));
    assert!(w.doodad(4233).expect("bench doodad").offers_crafts());

    // ---- housing --------------------------------------------------------------------------
    let designs = w.housing_designs();
    let standard: Vec<_> = designs.iter().take_while(|d| d.standard).collect();
    assert!(standard.windows(2).all(|p| p[0].tax <= p[1].tax));
    assert!(designs.iter().skip(standard.len()).all(|d| !d.standard));
    let d267 = designs
        .iter()
        .find(|d| d.design == 267)
        .expect("design 267");
    assert_eq!(d267.item, 15596);
    let d172 = designs
        .iter()
        .find(|d| d.design == 172)
        .expect("design 172");
    assert_eq!(d172.item, 21166);
    println!("cheapest designs: {:?}", &designs[..3.min(designs.len())]);
    assert!(w.housing_area_allows(52, d172.category));

    // ---- skills ---------------------------------------------------------------------------
    let basic = w.ability_skills(1);
    assert!(!basic.is_empty());
    let levels: Vec<u8> = basic
        .iter()
        .map(|id| w.skill(*id).unwrap().ability_level)
        .collect();
    assert!(levels.windows(2).all(|p| p[0] <= p[1]));

    // ---- data the bot crate used to read itself ----------------------------------------------
    // mana grows with the level (skills.mana_level_md)
    let scaling = basic
        .iter()
        .filter_map(|id| w.skill(*id))
        .find(|s| s.mana_level_md > 0.0)
        .expect("a basic skill whose mana cost scales with the level");
    println!(
        "skill {} ({}): mana {} + {} per level -> {} at 1, {} at 50",
        scaling.id,
        scaling.name,
        scaling.mana_cost,
        scaling.mana_level_md,
        scaling.mana_cost(1),
        scaling.mana_cost(50)
    );
    assert!(scaling.mana_cost(50) > scaling.mana_cost(10));
    assert!(scaling.mana_cost(10) >= scaling.mana_cost(1));
    // the craft skill needs level 10, and so does every pack craft
    assert_eq!(craft.req_level, 10);
    assert_eq!(w.min_pack_craft_level(), 10);
    println!(
        "craft 6226: level {} actability {} limit {}",
        craft.req_level, craft.actability_group, craft.actability_limit
    );
    // the skill that uses a doodad, and what an item use skill needs
    let with_skill: Vec<(u32, u32)> = (1..40_000)
        .map(|d| (d, w.doodad_use_skill(d)))
        .filter(|(_, s)| *s != 0)
        .collect();
    println!(
        "doodads with a use skill: {} (first {:?})",
        with_skill.len(),
        &with_skill[..3.min(with_skill.len())]
    );
    assert!(with_skill.len() > 100);
    let known = with_skill
        .iter()
        .filter(|(_, s)| w.skill(*s).is_some())
        .count();
    println!("  of which the skill table knows the skill: {known}");
    assert!(known * 10 > with_skill.len() * 9, "most use skills exist");
    assert_eq!(w.doodad_use_skill(4233), 0, "a bench is used by crafting");
    assert_eq!(w.doodad_use_skill(0), 0, "unknown doodads have none");
    assert_eq!(w.item_use_skill(0), 0);
    // quest start gates: level and the like
    println!("quests with start gates: {}", w.start_gates().len());
    assert!(!w.start_gates().is_empty());
    println!(
        "mother faction of the Nuian start faction: {}",
        w.mother_faction(w.race_faction(1).expect("Nuian faction"))
    );
    assert_eq!(w.mother_faction(0), 0);
    // trade pack prices: the Solzreed bench pack sold at its buyer
    let price = w
        .pack_price_estimate(31855, 14766, 0)
        .expect("a price for pack 31855 at NPC 14766");
    println!("pack 31855 at NPC 14766: {price} copper");
    assert!(price > 10_000, "a pack pays gold, not copper: {price}");
    assert_eq!(
        w.pack_price_estimate(31855, 1, 0),
        None,
        "that NPC buys no pack"
    );
    // garden radius of the two designs the bots build
    assert_eq!(d267.garden_radius, 4.0);
    assert_eq!(d172.garden_radius, 7.5);

    // ---- baked data (only when AA_BAKED is set) --------------------------------------------
    if paths.baked.is_some() {
        println!("terrain present: {}", w.baked().terrain.is_some());
        if let Some(z) = w.ground_z(nuian.x, nuian.y) {
            println!(
                "ground z at the Nuian start: {z:.2} (server says {:.2})",
                nuian.z
            );
            assert!((z - nuian.z).abs() < 6.0);
        }
        println!(
            "zone key at the Nuian start: {:?}",
            w.zone_key_at(nuian.x, nuian.y)
        );
        println!(
            "housing area at (13950, 14230): {:?}",
            w.housing_area_at(13950.0, 14230.0)
        );
    }

    // Loading twice must not accumulate anything.
    let t = Instant::now();
    let again = World::load(&paths).expect("second load");
    println!(
        "second load: {:?}, memory {} KiB",
        t.elapsed(),
        again.memory_estimate() / 1024
    );
    assert_eq!(again.memory_estimate(), mem);
}

/// Loads the real database next to synthetic baked data and checks the baked-backed queries.
#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA (real server data)"]
fn baked_data_is_wired_in() {
    use aa_world::baked::{write_json, HousingAreaPoly, QuestSphere, ZoneGrid};
    use aa_world::terrain::{write_height_grid, HeightGridHeader};

    let Some(mut paths) = paths() else {
        eprintln!("AA_COMPACT / AA_SERVER_DATA not set; skipping");
        return;
    };
    let dir = std::env::temp_dir().join(format!("aa-world-baked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A flat 130 m plateau around the Nuian start.
    let header = HeightGridHeader {
        width: 256,
        height: 256,
        origin_x: 15_000.0,
        origin_y: 14_800.0,
        step: 4.0,
        scale: 1.0 / 64.0,
        offset: 0.0,
        flags: 0,
    };
    write_height_grid(
        &dir.join("heights_main_world.bin"),
        &header,
        &vec![130 * 64; header.sample_count()],
    )
    .unwrap();
    ZoneGrid {
        width: 560,
        height: 608,
        sector: 64.0,
        keys: vec![179; 560 * 608],
    }
    .write(&dir.join("zones_main_world.bin"))
    .unwrap();
    write_json(
        &dir.join("housing_areas.json"),
        &vec![HousingAreaPoly {
            id: 52,
            zone_key: 179,
            points: vec![
                [15500.0, 15300.0],
                [15700.0, 15300.0],
                [15700.0, 15500.0],
                [15500.0, 15500.0],
            ],
        }],
    )
    .unwrap();
    write_json(
        &dir.join("quest_spheres.json"),
        &vec![QuestSphere {
            quest: 250,
            component: 271,
            x: 15_600.0,
            y: 15_400.0,
            z: 128.0,
            radius: 5.0,
        }],
    )
    .unwrap();

    paths.baked = Some(dir.clone());
    let w = World::load(&paths).expect("load with baked data");

    assert_eq!(w.ground_z(15_578.0, 15_382.0), Some(130.0));
    assert_eq!(w.ground_z(0.0, 0.0), None);
    assert_eq!(w.zone_key_at(15_578.0, 15_382.0), Some(179));
    assert_eq!(w.zone_name(179), Some("Solzreed Peninsula"));
    assert_eq!(w.housing_area_at(15_578.0, 15_382.0), Some(52));
    assert_eq!(w.housing_area_at(15_000.0, 15_000.0), None);
    let s = w.quest_sphere(250, 271).expect("sphere");
    assert_eq!((s.x, s.y, s.radius), (15_600.0, 15_400.0, 5.0));
    assert!(w.quest_sphere(250, 1).is_none());
    assert!(w.baked().terrain.is_some());
    // NPC entries without a Z are given the terrain height where the grid covers them.
    let filled = w
        .npc_templates()
        .iter()
        .flat_map(|n| w.npc_spawns(n.id))
        .filter(|p| p.z == 130.0)
        .count();
    println!("npc spawns with the plateau height: {filled}");
    println!("memory with baked data: {} KiB", w.memory_estimate() / 1024);
    let _ = std::fs::remove_dir_all(&dir);
}
