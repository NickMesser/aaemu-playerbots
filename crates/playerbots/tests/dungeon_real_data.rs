//! The dungeons feature against the real `main_world` data (ignored by default).
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test dungeon_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_world::{Vec3, World, WorldPaths};
use playerbots::bot::party::{self, Wish};

fn world() -> Option<Arc<World>> {
    static WORLD: OnceLock<Option<Arc<World>>> = OnceLock::new();
    WORLD
        .get_or_init(|| {
            let compact = PathBuf::from(std::env::var_os("AA_COMPACT")?);
            let server_data = PathBuf::from(std::env::var_os("AA_SERVER_DATA")?);
            let baked = std::env::var_os("AA_BAKED").map(PathBuf::from);
            Some(Arc::new(
                World::load(&WorldPaths {
                    compact,
                    server_data,
                    baked,
                    world: "main_world".into(),
                })
                .expect("load main_world"),
            ))
        })
        .clone()
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn every_usable_dungeon_has_a_door_a_world_a_spawn_and_an_exit() {
    let Some(w) = world() else { return };
    let sites = party::sites(&w);
    assert!(sites.len() >= 4, "only {} usable dungeons", sites.len());
    for s in &sites {
        let d = &w.dungeons()[usize::from(s.idx)];
        let door = &d.entrances[0];
        assert!(!door.spawns.is_empty(), "{}: no entrance spawn", d.name);
        assert!(
            door.skill != 0 || w.doodad_use_skill(door.doodad) != 0,
            "{}: the entrance doodad {} has no skill",
            d.name,
            door.doodad
        );
        assert!(w.is_dungeon_door(door.doodad));
        let iw = w.dungeon_world(d).expect("instance world");
        assert!(!iw.exits().is_empty(), "{}: no exit", d.name);
        for x in iw.exits() {
            assert!(
                w.is_dungeon_door(x.template),
                "{}: exit {}",
                d.name,
                x.template
            );
            assert!(x.skill != 0 || w.doodad_use_skill(x.template) != 0);
        }
        assert!(iw.spawn_for_zone(d.zone_key).is_some());
        assert!(!iw.npcs().is_empty(), "{}: the data places no NPC", d.name);
        println!(
            "{:30} group {:3} lvl {:2}-{:2} party {} npcs {:4} exits {}",
            d.name,
            d.zone_group,
            d.level_min,
            d.level_max,
            s.max_party,
            iw.npcs().len(),
            iw.exits().len()
        );
    }
    assert!(!w.is_dungeon_door(1));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn sharpwind_mines_is_the_cheapest_dungeon_for_three_level_20_bots_at_its_door() {
    let Some(w) = world() else { return };
    let sites = party::sites(&w);
    let mines = sites
        .iter()
        .find(|s| s.zone_group == 50)
        .expect("Sharpwind Mines (zone group 50) is a usable site");
    let d = &w.dungeons()[usize::from(mines.idx)];
    assert_eq!(d.entrances[0].doodad, 5075);
    assert_eq!(d.entrances[0].skill, 19836);
    let door = d.entrances[0].spawns[0];
    assert!((door.x - 11354.8).abs() < 1.0 && (door.y - 13124.0).abs() < 1.0);
    let wish = |bot: u32, dx: f32| Wish {
        bot,
        at: (door.x + dx, door.y - 10.0),
        level: 20,
        alliance: 1,
        skip: 0,
        raise: false,
        jump: false,
    };
    let wishes = [(wish(1, 0.0), 0), (wish(2, 2.0), 0), (wish(3, 4.0), 0)];
    let f = party::plan(&wishes, &sites, party::RELAX_AFTER_SECS).expect("a party forms");
    assert_eq!(f.site.zone_group, 50);
    assert_eq!(f.members.len(), 3);
    // the nearest exit of the instance world from its spawn point
    let iw = w.dungeon_world(d).unwrap();
    let spawn = iw.spawn_for_zone(d.zone_key).unwrap();
    let exit = iw
        .nearest_exit(Vec3::new(spawn.pos.x, spawn.pos.y, spawn.pos.z))
        .unwrap();
    assert_eq!((exit.template, exit.skill), (5723, 19754));
}

#[test]
#[ignore = "needs AA_COMPACT and AA_SERVER_DATA"]
fn a_level_1_bot_without_a_gm_raise_fits_no_dungeon_and_with_one_all_that_start_above() {
    let Some(w) = world() else { return };
    let sites = party::sites(&w);
    let w1 = Wish {
        bot: 1,
        at: (0.0, 0.0),
        level: 1,
        alliance: 1,
        skip: 0,
        raise: false,
        jump: false,
    };
    assert!(sites.iter().all(|s| !w1.fits(s)));
    let raised = Wish { raise: true, ..w1 };
    assert!(sites.iter().all(|s| raised.fits(s)));
    // every site the server refused is out
    let blocked = Wish {
        skip: u32::MAX,
        ..raised
    };
    assert!(sites.iter().all(|s| !blocked.fits(s)));
}
