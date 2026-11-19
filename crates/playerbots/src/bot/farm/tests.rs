//! Tests of the farm memory: it follows the server's events about the bot's own crops.

use aa_proto::sc_world::{CommonFarmEntry, DoodadInfo, WorldEvent};
use aa_world::{Crop, FarmKind, Yield};
use smallvec::smallvec;

use super::*;

const ME: u32 = 77;

struct Book(Vec<Crop>);

impl CropBook for Book {
    fn by_doodad(&self, doodad: u32) -> Option<&Crop> {
        self.0.iter().find(|c| c.doodad == doodad)
    }
}

/// Potato: doodad 2259 from seed 15659, phases 4456 (growing) and 4457 (ripe).
fn book() -> Book {
    Book(vec![
        Crop {
            seed: 15659,
            doodad: 2259,
            group: 12,
            grow_ms: 600_000,
            radius_mm: 500,
            restrict_zone: 0,
            ripe: vec![(4457, 13980), (10043, 13980)].into(),
            produce: vec![
                Yield {
                    item: 7992,
                    min: 2,
                    max: 4,
                },
                Yield {
                    item: 15659,
                    min: 1,
                    max: 2,
                },
            ]
            .into(),
            kinds: FarmKind::Farm.bit(),
        },
        Crop {
            seed: 15646,
            doodad: 2260,
            group: 12,
            grow_ms: 10_320_000,
            radius_mm: 500,
            restrict_zone: 0,
            ripe: vec![(5001, 13980)].into(),
            produce: vec![Yield {
                item: 7990,
                min: 3,
                max: 5,
            }]
            .into(),
            kinds: FarmKind::Farm.bit(),
        },
    ])
}

fn watcher<'a>(
    book: &'a Book,
    land: &'a dyn Fn(f32, f32) -> u8,
    pos: [f32; 3],
    now_ms: u64,
) -> Watcher<'a> {
    Watcher {
        char_id: ME,
        pos,
        now_ms,
        book,
        land,
    }
}

fn doodad(obj: u32, template: u32, owner: u32, phase: u32, x: f32, y: f32) -> DoodadInfo {
    DoodadInfo {
        obj,
        template,
        pos: (x, y, 10.0),
        owner_obj: 0,
        parent_obj: 0,
        phase,
        owner_id: owner,
        plant_time: 0,
        quest_glow: 0,
        has_loot: false,
    }
}

fn created(list: &[DoodadInfo]) -> WorldEvent {
    WorldEvent::DoodadsCreated(list.iter().copied().collect())
}

fn field(_: f32, _: f32) -> u8 {
    FarmKind::Farm as u8
}

fn entry(template: u32, phase: u32, time_left: u32, x: f32, y: f32) -> CommonFarmEntry {
    CommonFarmEntry {
        farm_type: 1,
        template,
        time_left,
        phase,
        pos: (x, y, 10.0),
        plant_time: 0,
    }
}

#[test]
fn only_the_bots_own_crops_are_remembered() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 1_000);
    let mut m = FarmMem::default();
    m.observe(
        &w,
        &created(&[
            doodad(1, 2259, ME, 4456, 5.0, 5.0),
            doodad(2, 2259, 999, 4456, 6.0, 6.0),
            doodad(3, 4242, ME, 1, 7.0, 7.0),
        ]),
    );
    assert_eq!(
        m.plots.len(),
        1,
        "the other player's crop and the stranger are not ours"
    );
    let p = m.plots[0];
    assert_eq!((p.obj, p.doodad, p.seed, p.phase), (1, 2259, 15659, 4456));
    assert!(!p.ripe);
    assert_eq!(p.land, FarmKind::Farm as u8);
    assert_eq!(
        p.check_ms,
        1_000 + RECHECK_MS,
        "the time left is unknown: look again later"
    );
    assert!(!p.due(1_000));
    assert!(p.due(1_000 + RECHECK_MS));
    // the same crop announced again is the same plot
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4456, 5.0, 5.0)]));
    assert_eq!(m.plots.len(), 1);
    // a bot that has not learned its character id yet remembers nothing
    let mut blank = FarmMem::default();
    let w0 = Watcher { char_id: 0, ..w };
    blank.observe(&w0, &created(&[doodad(1, 2259, 0, 4456, 5.0, 5.0)]));
    assert!(blank.plots.is_empty());
}

#[test]
fn a_phase_change_into_a_ripe_phase_makes_the_plot_due() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 5_000);
    let mut m = FarmMem::default();
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4456, 5.0, 5.0)]));
    m.observe(
        &w,
        &WorldEvent::DoodadPhase {
            obj: 9,
            phase: 4457,
        },
    );
    assert!(!m.plots[0].ripe, "another object's phase is not ours");
    m.observe(
        &w,
        &WorldEvent::DoodadPhase {
            obj: 1,
            phase: 4457,
        },
    );
    let p = m.plots[0];
    assert!(p.ripe && p.due(0));
    assert_eq!((p.phase, p.check_ms), (4457, 0));
    // the withered phase is harvestable too, an unknown phase is not
    m.observe(
        &w,
        &WorldEvent::DoodadPhase {
            obj: 1,
            phase: 10043,
        },
    );
    assert!(m.plots[0].ripe);
    m.observe(
        &w,
        &WorldEvent::DoodadPhase {
            obj: 1,
            phase: 4456,
        },
    );
    assert!(!m.plots[0].ripe);
    assert_eq!(m.due(5_000), 0);
}

#[test]
fn a_crop_found_ripe_is_due_at_once() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 5_000);
    let mut m = FarmMem::default();
    m.observe(&w, &created(&[doodad(4, 2259, ME, 4457, 5.0, 5.0)]));
    assert!(m.plots[0].ripe);
    assert_eq!(m.due(5_000), 1);
}

#[test]
fn a_crop_that_vanishes_near_the_bot_is_gone_and_one_that_leaves_the_view_stays() {
    let b = book();
    let mut m = FarmMem::default();
    let near = watcher(&b, &field, [6.0, 5.0, 10.0], 1_000);
    m.observe(&near, &created(&[doodad(1, 2259, ME, 4457, 5.0, 5.0)]));
    m.observe(&near, &WorldEvent::DoodadsRemoved(smallvec![1]));
    assert!(m.plots.is_empty());
    assert_eq!(m.lost, 1);
    // far away: it only left the view, the plot stays without an object id ...
    m.observe(&near, &created(&[doodad(2, 2259, ME, 4456, 500.0, 500.0)]));
    m.observe(&near, &WorldEvent::DoodadsRemoved(smallvec![2]));
    assert_eq!(m.plots.len(), 1);
    assert_eq!(m.plots[0].obj, 0);
    assert_eq!(m.lost, 1);
    // ... and is the same plot when it comes into view again under a new object id
    m.observe(&near, &created(&[doodad(8, 2259, ME, 4457, 500.3, 500.1)]));
    assert_eq!(m.plots.len(), 1);
    assert_eq!((m.plots[0].obj, m.plots[0].ripe), (8, true));
    // an object that is not a plot of ours is nothing
    m.observe(&near, &WorldEvent::DoodadsRemoved(smallvec![123]));
    assert_eq!(m.plots.len(), 1);
}

#[test]
fn the_list_of_the_public_farms_adds_updates_and_drops() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 100_000);
    let mut m = FarmMem::default();
    // one planted a minute ago on a farm, one planted just now, one on own land, one on a farm
    // that the list no longer has
    let plot = |obj, x, planted_ms, land| Plot {
        obj,
        doodad: 2259,
        seed: 15659,
        pos: [x, 0.0, 10.0],
        phase: 4456,
        ripe: false,
        check_ms: 999_999,
        planted_ms,
        honest: true,
        land,
    };
    m.plots.push(plot(1, 10.0, 40_000, 1));
    m.plots.push(plot(2, 20.0, 99_000, 1));
    m.plots.push(plot(3, 30.0, 40_000, OWN_LAND));
    m.plots.push(plot(4, 40.0, 40_000, 1));
    let list = WorldEvent::CommonFarmList(
        vec![
            entry(2259, 4457, 0, 10.2, 0.1),      // ripe now
            entry(2260, 5000, 90_000, 60.0, 0.0), // a rice we did not know
        ]
        .into_boxed_slice(),
    );
    m.observe(&w, &list);
    let by_x = |x: f32| m.plots.iter().find(|p| (p.pos[0] - x).abs() < 1.0).copied();
    let a = by_x(10.0).unwrap();
    assert!(
        a.ripe && a.check_ms == 0 && a.honest,
        "updated, the honesty is kept"
    );
    assert!(
        by_x(20.0).is_some(),
        "planted just now: the list is older than the plant"
    );
    assert!(by_x(30.0).is_some(), "own land is not on the list at all");
    assert!(by_x(40.0).is_none(), "gone from the farm");
    let rice = by_x(60.0).unwrap();
    assert_eq!((rice.doodad, rice.seed, rice.land), (2260, 15646, 1));
    assert_eq!(rice.check_ms, 100_000 + 90_000);
    assert!(!rice.honest, "a plot found again is not known to be honest");
    assert_eq!(m.plots.len(), 4);
}

#[test]
fn the_memory_is_bounded() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 1);
    let mut m = FarmMem::default();
    let list: Vec<DoodadInfo> = (0..200)
        .map(|i| doodad(1000 + i, 2259, ME, 4456, i as f32 * 10.0, 0.0))
        .collect();
    m.observe(&w, &created(&list));
    assert_eq!(m.plots.len(), MAX_PLOTS);
    let spare = Plot {
        obj: 1,
        doodad: 2259,
        seed: 15659,
        pos: [0.0; 3],
        phase: 0,
        ripe: false,
        check_ms: 0,
        planted_ms: 0,
        honest: false,
        land: 0,
    };
    assert!(!m.plant(spare));
}

#[test]
fn bad_spots_and_seeds_are_forgotten_in_time_and_stay_few() {
    let mut m = FarmMem::default();
    assert!(!m.spot_is_bad(1.0, 1.0, 0));
    m.mark_bad_spot(1.0, 1.0, 1_000);
    assert!(m.spot_is_bad(1.2, 1.0, 2_000));
    assert!(!m.spot_is_bad(5.0, 1.0, 2_000));
    assert!(!m.spot_is_bad(1.0, 1.0, 1_000 + BAD_SPOT_MS));
    for i in 0..20 {
        m.mark_bad_spot(i as f32 * 10.0, 0.0, 1_000);
    }
    assert!(m.bad_spots.len() <= 6);
    assert!(m.spot_is_bad(190.0, 0.0, 2_000), "the newest are kept");
    assert!(!m.seed_is_bad(5, 0));
    m.mark_bad_seed(5, 1_000);
    assert!(m.seed_is_bad(5, 2_000) && !m.seed_is_bad(6, 2_000));
    assert!(!m.seed_is_bad(5, 1_000 + BAD_SEED_MS));
    for s in 0..10 {
        m.mark_bad_seed(s, 1_000);
    }
    assert!(m.bad_seeds.len() <= 3);
}

#[test]
fn the_produce_on_its_way_leaves_out_the_seeds() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 1);
    let mut m = FarmMem::default();
    m.observe(
        &w,
        &created(&[
            doodad(1, 2259, ME, 4456, 5.0, 5.0),
            doodad(2, 2259, ME, 4456, 9.0, 5.0),
            doodad(3, 2260, ME, 5000, 13.0, 5.0),
        ]),
    );
    let mut g = m.growing(&b).to_vec();
    g.sort_by_key(|e| e.0);
    assert_eq!(g, vec![(7990, 4.0), (7992, 6.0)]);
}

#[test]
fn a_harvested_crop_that_leaves_is_no_loss() {
    let b = book();
    let w = watcher(&b, &field, [6.0, 5.0, 10.0], 1_000);
    let mut m = FarmMem::default();
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4457, 5.0, 5.0)]));
    m.expect_removal(1);
    m.observe(&w, &WorldEvent::DoodadsRemoved(smallvec![1]));
    assert!(m.plots.is_empty());
    assert_eq!(m.lost, 0, "the bot harvested it");
    // the expectation is used up
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4457, 5.0, 5.0)]));
    m.observe(&w, &WorldEvent::DoodadsRemoved(smallvec![1]));
    assert_eq!(m.lost, 1);
    // and bounded
    for obj in 0..20 {
        m.expect_removal(obj);
    }
    assert!(m.reaped.len() <= 4);
}

#[test]
fn plots_are_found_by_object_and_by_place() {
    let b = book();
    let w = watcher(&b, &field, [0.0; 3], 1);
    let mut m = FarmMem::default();
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4456, 5.0, 5.0)]));
    assert!(m.by_obj(1).is_some() && m.by_obj(2).is_none() && m.by_obj(0).is_none());
    assert!(m.at(5.3, 5.3).is_some() && m.at(9.0, 5.0).is_none());
    assert!(
        m.at(6.7, 5.0).is_none(),
        "a neighbour one spacing away is another plot"
    );
    assert_eq!(m.count_on(FarmKind::Farm as u8), 1);
    assert_eq!(m.count_on(OWN_LAND), 0);
    m.drop_plot(0);
    m.drop_plot(9);
    assert!(m.plots.is_empty());
}

#[test]
fn wild_plots_survive_the_public_farm_list_and_a_vanished_one_is_noted_as_gone() {
    let b = book();
    let wild = |_: f32, _: f32| WILD_LAND;
    let mut m = FarmMem::default();
    let w = watcher(&b, &wild, [6.0, 5.0, 10.0], 1_000);
    m.observe(&w, &created(&[doodad(1, 2259, ME, 4456, 5.0, 5.0)]));
    assert_eq!(m.plots[0].land, WILD_LAND, "found again out in the open");
    assert!(m.plots[0].kind().is_none());
    // the list of the public farms does not name it, and it stays
    m.observe(&w, &WorldEvent::CommonFarmList(Box::new([])));
    assert_eq!(m.plots.len(), 1);
    assert!(!m.has_gone());
    // it vanishes while the bot looks on: gone, not harvested
    m.observe(&w, &WorldEvent::DoodadsRemoved(smallvec![1]));
    assert!(m.plots.is_empty());
    let gone = m.take_gone();
    assert_eq!(gone.len(), 1);
    assert_eq!((gone[0].land, gone[0].harvested), (WILD_LAND, false));
    assert!(!m.has_gone());
}

#[test]
fn a_farming_bot_keeps_an_eye_on_the_wild_crops_of_others_and_only_those() {
    let b = book();
    let wild = |x: f32, _: f32| if x < 100.0 { WILD_LAND } else { OWN_LAND };
    let mut m = FarmMem::default();
    let w = watcher(&b, &wild, [0.0; 3], 1_000);
    m.observe(
        &w,
        &created(&[
            doodad(1, 2259, 555, 4456, 5.0, 5.0),
            doodad(2, 2259, 556, 4457, 150.0, 5.0),
            doodad(3, 4242, 557, 1, 6.0, 6.0),
            doodad(4, 2259, ME, 4456, 9.0, 9.0),
        ]),
    );
    assert_eq!(
        m.foreign.len(),
        1,
        "a garden's crop and a stranger are left alone"
    );
    assert_eq!((m.foreign[0].obj, m.foreign[0].owner), (1, 555));
    assert_eq!(m.plots.len(), 1, "the bot's own crop is a plot");
    m.observe(
        &w,
        &WorldEvent::DoodadPhase {
            obj: 1,
            phase: 4457,
        },
    );
    assert_eq!(m.foreign[0].phase, 4457);
    m.observe(&w, &WorldEvent::DoodadsRemoved(smallvec![1]));
    assert!(m.foreign.is_empty());
    // bounded
    for i in 0..10 {
        m.observe(
            &w,
            &created(&[doodad(10 + i, 2259, 555, 4456, i as f32, 0.0)]),
        );
    }
    assert_eq!(m.foreign.len(), MAX_FOREIGN);
    // the crops a run took on are forgotten when it ends
    m.plots[0].land = STOLEN_LAND;
    m.drop_stolen();
    assert!(m.plots.is_empty());
}
