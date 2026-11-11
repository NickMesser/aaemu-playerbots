//! The population features against the real `main_world` data: the sides and continents of real
//! factions and zones, the census feeding the World view through a real hub, the filtered spawn and
//! despawn commands reaching the supervisor, and the director's policy through the hub.
//!
//! ```text
//! AA_COMPACT=<.../app/Data/compact.sqlite3> AA_SERVER_DATA=<.../app/Data> [AA_BAKED=<data dir>] \
//!   cargo test -p playerbots --test population_real_data -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use aa_world::{World, WorldPaths};
use pb_web::{
    Activity, Alliance, BotHub, BotSnapshot, Command, Continent, PolicyPatch, SpawnFilter,
};
use playerbots::bot::state::{BotState, Unit, UnitKind};
use playerbots::config::PopulationCfg;
use playerbots::hub::{unix_ms, Hub, HubParams, SupCmd};
use playerbots::population::census::observe;
use playerbots::population::region::{alliance_of_faction, continent_of_zone, place_at};
use playerbots::population::select::Selector;
use playerbots::population::Population;

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

fn hub(
    world: &Arc<World>,
    enabled: bool,
    races: Vec<u8>,
) -> (Hub, tokio::sync::mpsc::Receiver<SupCmd>) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let cfg = PopulationCfg {
        enabled,
        dry_run: false,
        ..PopulationCfg::default()
    };
    let hub = Hub::new(HubParams {
        world: world.as_ref(),
        baked_dir: None,
        supervisor: tx,
        llm: None,
    })
    .with_population(Population::new(&cfg, races, Some(world.clone())));
    (hub, rx)
}

/// Where bots of the live runs stood: the west continent and Falcony Plateau.
const WEST: (f32, f32) = (9_910.7, 15_385.2);
const EAST: (f32, f32) = (24_421.8, 9_259.0);

fn snap(
    id: u32,
    race: &str,
    alliance: Alliance,
    continent: Continent,
    x: f32,
    y: f32,
) -> BotSnapshot {
    BotSnapshot {
        id,
        name: format!("Bot{id}"),
        race: race.into(),
        x,
        y,
        alliance,
        continent,
        zone_key: 1,
        activity: Activity::Idle,
        goal: "wander".into(),
        online_secs: 600,
        ..BotSnapshot::default()
    }
}

#[test]
#[ignore = "needs the real data (AA_COMPACT, AA_SERVER_DATA)"]
fn factions_and_zones_are_placed_on_the_real_map() {
    let Some(world) = world() else {
        eprintln!("no data; skipped");
        return;
    };
    // the player factions: 101..=107 are Nuia's, 108..=113 Haranya's
    for f in 101..=107 {
        assert_eq!(
            alliance_of_faction(&*world, f),
            Alliance::Nuia,
            "faction {f}"
        );
    }
    for f in 108..=113 {
        assert_eq!(
            alliance_of_faction(&*world, f),
            Alliance::Haranya,
            "faction {f}"
        );
    }
    assert_eq!(alliance_of_faction(&*world, 0), Alliance::Unknown);
    // Solzreed (zone key 179) is on the west continent, Falcony Plateau (184) on the east
    assert_eq!(continent_of_zone(&*world, 179), Continent::West);
    assert_eq!(continent_of_zone(&*world, 184), Continent::East);
    assert_eq!(continent_of_zone(&*world, 0), Continent::Unknown);
    // a spot of Gweonid Forest (zone key 129, west) and one on Falcony Plateau (184, east)
    let west = place_at(&*world, WEST.0, WEST.1);
    let east = place_at(&*world, EAST.0, EAST.1);
    println!("west {west:?}, east {east:?}");
    assert_eq!(west.continent, Continent::West);
    assert_eq!(east.continent, Continent::East);
    assert_eq!(east.zone_key, 184);
}

#[test]
#[ignore = "needs the real data (AA_COMPACT, AA_SERVER_DATA)"]
fn the_census_feeds_the_world_view_through_a_real_hub() {
    let Some(world) = world() else {
        return;
    };
    let (hub, _rx) = hub(&world, false, vec![1, 4, 5, 6]);
    hub.register(snap(
        1,
        "Elf",
        Alliance::Nuia,
        Continent::West,
        WEST.0,
        WEST.1,
    ));
    hub.register(snap(
        2,
        "Ferre",
        Alliance::Haranya,
        Continent::East,
        EAST.0,
        EAST.1,
    ));
    hub.mark_bot_char(900);

    // a bot in the west sees one of our characters, a Nuia player and a Haranya player
    let mut st = BotState::new();
    st.me.char_id = 800;
    let unit = |id: u32, faction: u32, x: f32, y: f32| Unit {
        kind: UnitKind::Character,
        id,
        pos: [x, y, 100.0],
        hp: 100,
        faction,
        level: 20,
        flags: 0,
        target: 0,
        name: Some(format!("P{id}").into()),
    };
    st.units.insert(1, unit(900, 101, WEST.0 + 10.0, WEST.1));
    st.units.insert(2, unit(901, 101, WEST.0 + 20.0, WEST.1));
    st.units.insert(3, unit(902, 108, EAST.0, EAST.1));
    let seen = observe(
        &st,
        &hub.pop.census,
        &*world,
        &|id| hub.is_bot_char(id),
        1,
        unix_ms(),
    );
    assert_eq!(seen, 2, "our own character is not a player");

    let w = hub.world();
    assert_eq!(w.totals.players_seen, 2);
    assert_eq!(w.totals.bots_online, 2);
    let nuia_west = w
        .cells
        .iter()
        .find(|c| c.continent == Continent::West && c.alliance == Alliance::Nuia)
        .expect("a west / Nuia cell");
    assert_eq!((nuia_west.players_seen, nuia_west.bots_online), (1, 1));
    // the Haranya player stands on Falcony Plateau, in the east
    let haranya_east = w
        .cells
        .iter()
        .find(|c| c.continent == Continent::East && c.alliance == Alliance::Haranya)
        .expect("an east / Haranya cell");
    assert_eq!(
        (haranya_east.players_seen, haranya_east.bots_online),
        (1, 1)
    );
    assert!(!w.heat.is_empty());
    println!("{}", serde_json::to_string_pretty(&w.totals).unwrap());
}

#[test]
#[ignore = "needs the real data (AA_COMPACT, AA_SERVER_DATA)"]
fn filtered_commands_reach_the_supervisor_and_bad_ones_are_refused() {
    let Some(world) = world() else {
        return;
    };
    let (hub, mut rx) = hub(&world, false, vec![1, 4, 5]);
    let filter = |a: Option<&str>, r: Option<&str>, c: Option<&str>| SpawnFilter {
        alliance: a.map(Into::into),
        race: r.map(Into::into),
        continent: c.map(Into::into),
    };

    hub.command(
        None,
        Command::SpawnWhere {
            count: 3,
            filter: filter(Some("haranya"), None, None),
        },
    )
    .unwrap();
    match rx.try_recv().unwrap() {
        SupCmd::SpawnWhere { n, sel, owned } => {
            assert_eq!(n, 3);
            assert_eq!(sel.alliance, Some(Alliance::Haranya));
            assert!(!owned, "the operator's bots are not the director's");
        }
        other => panic!("{other:?}"),
    }

    hub.command(
        None,
        Command::DespawnWhere {
            count: 2,
            filter: filter(None, Some("elf"), Some("west")),
        },
    )
    .unwrap();
    match rx.try_recv().unwrap() {
        SupCmd::DespawnWhere { n, sel, rule } => {
            assert_eq!(n, 2);
            assert_eq!((sel.race, sel.continent), (Some(4), Some(Continent::West)));
            assert!(!rule.owned_only, "an operator's despawn may stop any bot");
        }
        other => panic!("{other:?}"),
    }

    // a race that no side has on that continent
    let err = hub
        .command(
            None,
            Command::SpawnWhere {
                count: 1,
                filter: filter(None, Some("ferre"), Some("west")),
            },
        )
        .unwrap_err();
    assert!(err.contains("no race matches"), "{err}");
    // a race that is not in bots.races (nuian, elf and hariharan here) and that no bot has
    let err = hub
        .command(
            None,
            Command::SpawnWhere {
                count: 1,
                filter: filter(None, Some("ferre"), None),
            },
        )
        .unwrap_err();
    assert!(err.contains("bots.races"), "{err}");
    assert!(
        rx.try_recv().is_err(),
        "nothing was sent for the refused ones"
    );
    assert_eq!(Selector::ALL, Selector::default());
}

#[test]
#[ignore = "needs the real data (AA_COMPACT, AA_SERVER_DATA)"]
fn the_policy_goes_through_the_hub_and_the_config_can_forbid_it() {
    let Some(world) = world() else {
        return;
    };
    let (hub, _rx) = hub(&world, false, vec![1, 4]);
    let v = hub.director();
    assert!(!v.available && v.mode == "off");
    let err = hub
        .set_policy(PolicyPatch {
            active: Some(true),
            ..PolicyPatch::default()
        })
        .unwrap_err();
    assert!(err.contains("[population] enabled"), "{err}");
    hub.set_policy(PolicyPatch {
        mirror_pct: Some(150),
        ..PolicyPatch::default()
    })
    .unwrap();
    assert_eq!(hub.director().policy.mirror_pct, 150);

    let (hub, _rx) = hub_enabled(&world);
    hub.set_policy(PolicyPatch {
        active: Some(true),
        ..PolicyPatch::default()
    })
    .unwrap();
    // a second of the tick with nobody around: nothing happens, nothing breaks
    hub.tick();
    let v = hub.director();
    assert!(v.available && v.policy.active);
    assert_eq!(v.mode, "live");
    assert!(v.log.iter().all(|r| r.kind == "policy"), "{:?}", v.log);
}

fn hub_enabled(world: &Arc<World>) -> (Hub, tokio::sync::mpsc::Receiver<SupCmd>) {
    hub(world, true, vec![1, 4])
}
