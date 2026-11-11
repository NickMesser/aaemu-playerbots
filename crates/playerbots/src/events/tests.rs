//! Tests of the event board and the detectors, on a made-up event shaped like the Crimson Rift:
//! wave 1 a timer, wave 2 and 3 kill counts with a controller 500 m out, wave 4 a timer.

use std::sync::Arc;

use aa_proto::sc_event::{EventId, TowerDefInfo};
use aa_proto::sc_world::WorldEvent;
use aa_world::event::{Controller, EventDef, Kill, Step};
use aa_world::{Events, Vec3};

use super::*;
use crate::issues::{IssueKind, Suspect};

const RIFT: EventId = EventId {
    def: 3,
    zone_group: 20,
};
const OTHER: EventId = EventId {
    def: 13,
    zone_group: 20,
};
const ARCHER: u32 = 8834;
const SOLDIER: u32 = 8827;
const GROUP: u16 = 20;

struct Fixture(Events);

impl Source for Fixture {
    fn events(&self) -> &Events {
        &self.0
    }
}

fn step(prog: u32, timer: f32, all: bool, kills: &[(u32, u32)], spawners: &[u32]) -> Step {
    Step {
        prog,
        timer_s: timer,
        all,
        kills: kills
            .iter()
            .map(|&(template, count)| Kill { template, count })
            .collect(),
        spawners: spawners.into(),
    }
}

fn controller(spawner: u32, step: u8, template: u32, x: f32) -> Controller {
    Controller {
        spawner,
        step,
        template,
        pos: Vec3::new(x, 1000.0, 10.0),
        zone_group: GROUP,
    }
}

fn def(id: u32, min_level: u8) -> EventDef {
    EventDef {
        id,
        name: "Crimson Rift".into(),
        first_wave_after_s: 0.0,
        force_end_s: 3600.0,
        target_spawner: 100,
        end_kill: (0, 0),
        steps: vec![
            step(1, 170.0, true, &[], &[]),
            step(2, 0.0, true, &[(ARCHER, 3), (8826, 2)], &[101]),
            step(3, 0.0, true, &[(SOLDIER, 2)], &[102]),
            step(4, 3600.0, true, &[], &[103]),
        ]
        .into(),
        controllers: vec![
            controller(100, 0, 900, 1000.0),
            controller(101, 2, 901, 1500.0),
            controller(102, 3, 902, 1500.0),
            controller(103, 4, 903, 1500.0),
        ]
        .into(),
        min_level,
        max_level: min_level + 5,
        war_zone_group: None,
    }
}

fn board() -> EventBoard {
    let b = EventBoard::manual();
    b.bind_data(Arc::new(Fixture(Events::from_defs(vec![
        def(3, 40),
        def(13, 50),
    ]))));
    b
}

fn start(b: &EventBoard, id: EventId) {
    b.on_event(&WorldEvent::TowerDefStart { id, zone_key: 257 }, true);
}

fn wave(b: &EventBoard, id: EventId, step: u32) {
    b.on_event(
        &WorldEvent::TowerDefWave {
            id,
            zone_key: 257,
            step,
        },
        true,
    );
}

/// `(object id, position)` of a live target.
type Target = (u32, [f32; 3]);

fn list(b: &EventBoard, in_main: bool, infos: &[(EventId, u32, Option<Target>)]) {
    let list = infos
        .iter()
        .map(|&(id, step, target)| TowerDefInfo {
            id,
            zone_key: 257,
            spot_id: 9846,
            target_obj: target.map_or(0, |t| t.0),
            pos: target.map_or((0.0, 0.0, 0.0), |t| (t.1[0], t.1[1], t.1[2])),
            step,
        })
        .collect::<Vec<_>>();
    b.on_event(&WorldEvent::TowerDefList(list.into_boxed_slice()), in_main);
}

fn caps(per_event: u32, max_share: f32) -> Caps {
    Caps {
        per_event,
        max_share,
        min_level: 40,
    }
}

fn population(n: usize, level: u8) -> Population {
    Population::from_levels(std::iter::repeat_n(level, n))
}

// ---- the packets -----------------------------------------------------------------------------

#[test]
fn the_first_bot_posts_an_event_and_the_others_find_it() {
    let b = board();
    assert!(!b.any());
    start(&b, RIFT);
    assert!(b.any());
    // twenty more bots hear the same packets
    for _ in 0..20 {
        start(&b, RIFT);
        wave(&b, RIFT, 1);
        wave(&b, RIFT, 1);
    }
    let v = b.view(RIFT).unwrap();
    assert_eq!((b.views().len(), v.step, v.joined), (1, 1, 0));
    assert_eq!(b.stats().events, 1);
    // a second event is separate; a wave number never goes back
    start(&b, OTHER);
    wave(&b, RIFT, 3);
    wave(&b, RIFT, 2);
    assert_eq!(b.views().len(), 2);
    assert_eq!(b.view(RIFT).unwrap().step, 3);
    assert_eq!(b.view(OTHER).unwrap().step, 0);
}

#[test]
fn the_list_carries_the_wave_and_the_target() {
    let b = board();
    // a bot that logs in while the event runs hears only the list
    list(
        &b,
        true,
        &[(RIFT, 2, Some((77, [14808.0, 12063.0, 180.0])))],
    );
    let v = b.view(RIFT).unwrap();
    assert_eq!(v.step, 2);
    assert_eq!(v.target, Some((77, [14808.0, 12063.0, 180.0])));
    // the target moves on (a new object after it was killed and came back)
    list(
        &b,
        true,
        &[(RIFT, 2, Some((90, [14800.0, 12060.0, 180.0])))],
    );
    assert_eq!(b.view(RIFT).unwrap().target.unwrap().0, 90);
    // no live target: no position
    list(&b, true, &[(RIFT, 3, None)]);
    let v = b.view(RIFT).unwrap();
    assert_eq!((v.step, v.target), (3, None));
}

#[test]
fn a_running_event_names_the_kill_targets_of_its_waves() {
    let b = board();
    assert!(!b.runs_kill_target(ARCHER), "no event runs");
    start(&b, RIFT);
    assert!(b.runs_kill_target(ARCHER));
    assert!(b.runs_kill_target(8826));
    assert!(b.runs_kill_target(SOLDIER));
    assert!(!b.runs_kill_target(9999), "not a wave target");
    b.on_event(
        &WorldEvent::TowerDefEnd {
            id: RIFT,
            zone_key: 257,
        },
        true,
    );
    assert!(!b.runs_kill_target(ARCHER), "the event ended");
}

#[test]
fn the_end_packet_removes_the_event_and_a_late_repeat_changes_nothing() {
    let b = board();
    start(&b, RIFT);
    b.on_event(
        &WorldEvent::TowerDefEnd {
            id: RIFT,
            zone_key: 257,
        },
        true,
    );
    assert!(!b.any() && b.view(RIFT).is_none());
    for _ in 0..3 {
        b.on_event(
            &WorldEvent::TowerDefEnd {
                id: RIFT,
                zone_key: 257,
            },
            true,
        );
    }
    assert!(!b.any());
}

#[test]
fn a_list_without_the_event_ends_it_after_a_grace_but_not_in_an_instance() {
    let b = board();
    start(&b, RIFT);
    // the list of a bot in a dungeon says nothing about the main world
    b.advance(10_000);
    list(&b, false, &[]);
    assert!(b.any());
    // a list from before the start that reaches a slow bot late
    let b2 = board();
    start(&b2, RIFT);
    list(&b2, true, &[]);
    assert!(b2.any(), "too soon to tell");
    b2.advance(10_000);
    list(&b2, true, &[]);
    assert!(!b2.any());
}

#[test]
fn the_board_is_bounded() {
    let b = EventBoard::manual();
    for def in 0..(MAX_EVENTS as u32 + 5) {
        start(&b, EventId { def, zone_group: 1 });
    }
    assert_eq!(b.views().len(), MAX_EVENTS);
}

// ---- slots -----------------------------------------------------------------------------------

#[test]
fn twenty_eligible_bots_fill_the_event_to_its_cap_and_no_further() {
    let b = board();
    start(&b, RIFT);
    b.set_population(population(20, 45));
    // 30 percent of 20 is 6, the per-event cap is 6
    let caps = caps(6, 0.3);
    let mut joined = 0;
    let mut full = 0;
    for bot in 1..=20 {
        match b.join(RIFT, bot, &caps) {
            Join::Joined => joined += 1,
            Join::Full | Join::OverShare => full += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((joined, full), (6, 14));
    assert_eq!(b.view(RIFT).unwrap().joined, 6);
    let s = b.stats();
    assert_eq!((s.joins, s.refused_full), (6, 14));
    // a bot that has a slot gets it again
    assert_eq!(b.join(RIFT, 1, &caps), Join::Already);
    assert_eq!(b.can_join(RIFT, 1, &caps), Join::Already);
    assert_eq!(b.can_join(RIFT, 20, &caps), Join::Full);
}

#[test]
fn the_share_of_the_eligible_bots_caps_all_events_together() {
    let b = board();
    start(&b, RIFT);
    start(&b, OTHER);
    // 20 online, 8 of them at level 50 and 12 at level 40
    let pop =
        Population::from_levels(std::iter::repeat_n(50u8, 8).chain(std::iter::repeat_n(40u8, 12)));
    assert_eq!(
        (
            pop.online,
            pop.eligible(40),
            pop.eligible(50),
            pop.eligible(51)
        ),
        (20, 20, 8, 0)
    );
    b.set_population(pop);
    // a quarter of 20 is 5 for the Crimson Rift (level 40) ...
    let c = caps(6, 0.25);
    let n = (1..=20)
        .filter(|&bot| b.join(RIFT, bot, &c) == Join::Joined)
        .count();
    assert_eq!(n, 5);
    let refused = b.stats().refused_share;
    assert_eq!(refused, 15, "the other fifteen were turned away");
    // ... and they count against the other event too (a quarter of the 8 is 2: already over)
    let c50 = Caps {
        min_level: 50,
        ..caps(6, 0.25)
    };
    assert_eq!(b.join(OTHER, 30, &c50), Join::OverShare);
    assert_eq!(b.stats().refused_share, refused + 1);
}

#[test]
fn without_a_population_only_the_event_cap_counts() {
    let b = board();
    start(&b, RIFT);
    let c = caps(3, 0.01);
    let n = (1..=10)
        .filter(|&bot| b.join(RIFT, bot, &c) == Join::Joined)
        .count();
    assert_eq!(n, 3);
}

#[test]
fn a_share_cap_is_at_least_one_and_a_bad_share_means_one() {
    let p = population(10, 45);
    assert_eq!(p.share_cap(40, 0.0), Some(1));
    assert_eq!(p.share_cap(40, 0.25), Some(3));
    assert_eq!(p.share_cap(40, 1.0), Some(10));
    assert_eq!(p.share_cap(40, 7.0), Some(10));
    assert_eq!(p.share_cap(40, f32::NAN), Some(1));
    assert_eq!(p.share_cap(99, 0.5), Some(1));
    assert_eq!(Population::default().share_cap(40, 0.5), None);
    // levels beyond the histogram land in the last bucket
    let p = Population::from_levels([200u8, 255]);
    assert_eq!(p.eligible(100), 2);
}

#[test]
fn a_lease_that_is_not_renewed_frees_the_slot() {
    let b = board();
    start(&b, RIFT);
    let c = caps(2, 1.0);
    assert_eq!(b.join(RIFT, 1, &c), Join::Joined);
    assert_eq!(b.join(RIFT, 2, &c), Join::Joined);
    assert_eq!(b.join(RIFT, 3, &c), Join::Full);
    // bot 1 keeps renewing, bot 2 vanished
    for _ in 0..4 {
        b.advance(LEASE_MS / 2);
        assert!(b.renew(RIFT, 1));
    }
    assert!(!b.renew(RIFT, 2), "the lease of bot 2 ran out");
    assert_eq!(b.join(RIFT, 3, &c), Join::Joined);
    // sweeping drops the dead lease from the count
    b.advance(LEASE_MS + 1);
    assert!(b.sweep(None).is_empty());
    assert_eq!(b.view(RIFT).unwrap().joined, 0);
}

#[test]
fn a_bot_that_goes_offline_gives_its_slot_back() {
    let b = board();
    start(&b, RIFT);
    let c = caps(1, 1.0);
    assert_eq!(b.join(RIFT, 1, &c), Join::Joined);
    assert_eq!(b.join(RIFT, 2, &c), Join::Full);
    b.release_bot(1);
    assert_eq!(b.join(RIFT, 2, &c), Join::Joined);
    // asking after the event ended
    b.on_event(
        &WorldEvent::TowerDefEnd {
            id: RIFT,
            zone_key: 257,
        },
        true,
    );
    assert_eq!(b.join(RIFT, 3, &c), Join::Gone);
    assert!(!b.renew(RIFT, 2));
    b.release_bot(2);
}

#[test]
fn a_hurt_bot_stays_away_for_a_while_and_three_deaths_are_enough() {
    let b = board();
    start(&b, RIFT);
    let c = caps(6, 1.0);
    assert_eq!(b.join(RIFT, 1, &c), Join::Joined);
    b.leave(RIFT, 1, Leave::Hurt);
    assert_eq!(b.view(RIFT).unwrap().joined, 0);
    assert_eq!(b.join(RIFT, 1, &c), Join::Barred);
    b.advance(HURT_BAR_MS + 1);
    assert_eq!(b.join(RIFT, 1, &c), Join::Joined);
    // dying: twice is forgiven, the third time is the last
    for n in 1..=3 {
        b.leave(RIFT, 1, Leave::Died);
        if n < 3 {
            assert_eq!(b.join(RIFT, 1, &c), Join::Joined, "death {n}");
        }
    }
    assert_eq!(b.join(RIFT, 1, &c), Join::Barred);
    b.advance(24 * 3_600_000);
    assert_eq!(b.join(RIFT, 1, &c), Join::Barred, "until the event ends");
    // another bot is not affected, and neither is another event
    assert_eq!(b.join(RIFT, 2, &c), Join::Joined);
    start(&b, OTHER);
    assert_eq!(b.join(OTHER, 1, &c), Join::Joined);
    // leaving without a mark
    b.leave(RIFT, 2, Leave::Done);
    assert_eq!(b.join(RIFT, 2, &c), Join::Joined);
    b.leave(RIFT, 2, Leave::Idle);
    assert_eq!(b.join(RIFT, 2, &c), Join::Barred);
}

#[test]
fn the_join_delay_is_drawn_from_the_bot_and_stays_in_range() {
    let b = board();
    start(&b, RIFT);
    // [10, 120] seconds
    let delay = [10, 120];
    let mut ready_at = Vec::new();
    for bot in 1..=40u32 {
        assert!(!b.ready(RIFT, bot, delay), "the first ask starts the wait");
        let started = b.now_ms();
        // the wait is over within 120 s and not before 10 s
        b.advance(9_999);
        assert!(!b.ready(RIFT, bot, delay), "bot {bot} ready after 10 s");
        b.advance(1);
        let mut waited = 10;
        while !b.ready(RIFT, bot, delay) {
            b.advance(1_000);
            waited += 1;
            assert!(waited <= 120, "bot {bot} still waiting after 120 s");
        }
        ready_at.push(waited);
        // once over it stays over
        assert!(b.ready(RIFT, bot, delay));
        assert!(b.now_ms() - started >= 10_000);
    }
    ready_at.sort_unstable();
    ready_at.dedup();
    assert!(ready_at.len() >= 15, "the delays are spread: {ready_at:?}");
    // the same bot draws the same delay from the same event
    let b2 = board();
    start(&b2, RIFT);
    let wait = |b: &EventBoard, bot| {
        assert!(!b.ready(RIFT, bot, delay));
        let mut s = 0;
        while !b.ready(RIFT, bot, delay) {
            b.advance(1_000);
            s += 1;
        }
        s
    };
    assert_eq!(wait(&b2, 7), {
        let b3 = board();
        start(&b3, RIFT);
        wait(&b3, 7)
    });
    // no delay at all
    assert!(b2.ready(RIFT, 99, [0, 0]));
    // an event that is gone is never ready
    assert!(!b2.ready(OTHER, 7, delay));
}

// ---- evidence and detectors -----------------------------------------------------------------

fn at_the_target(b: &EventBoard, secs: u64) {
    for _ in 0..secs {
        b.advance(1_000);
        b.observe(RIFT, true);
    }
}

#[test]
fn deaths_count_once_per_object_and_only_for_the_current_wave() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    // three bots see the same archer die
    for _ in 0..3 {
        b.note_death(1001, ARCHER);
    }
    // an NPC the wave does not wait for, and one of the next wave
    b.note_death(1002, 555);
    b.note_death(1003, SOLDIER);
    assert_eq!(b.stats().kills, 1);
    wave(&b, RIFT, 3);
    // the archer's corpse is still in view: not a kill of wave 3 (it is not a target of it
    // anyway), and the soldier that dies now is
    b.note_death(1001, ARCHER);
    b.note_death(1004, SOLDIER);
    b.note_death(1004, SOLDIER);
    assert_eq!(b.stats().kills, 2);
}

#[test]
fn a_corpse_seen_in_the_wave_before_is_not_a_kill_of_this_one() {
    // a template that two consecutive waves wait for
    let mut d = def(3, 40);
    d.steps[2] = step(3, 0.0, true, &[(ARCHER, 2)], &[102]);
    let b = EventBoard::manual();
    b.bind_data(Arc::new(Fixture(Events::from_defs(vec![d]))));
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    b.note_death(1, ARCHER);
    wave(&b, RIFT, 3);
    b.note_death(1, ARCHER);
    b.note_death(2, ARCHER);
    assert_eq!(b.stats().kills, 2, "object 1 died in wave 2");
}

#[test]
fn wave_npcs_that_never_show_are_reported_once_after_the_walk() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    // the controller of wave 2 stands 500 m from the target: 125 s of walking, a minute of grace
    let wait = detect::WAVE_GRACE_MS / 1000 + 125;
    // a bot gets to the target after a while and stands there
    b.advance(20_000);
    at_the_target(&b, 40);
    assert!(b.sweep(None).is_empty(), "too early");
    b.advance((wait - 60) * 1000);
    let reports = b.sweep(None);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let r = &reports[0];
    assert_eq!(r.kind, IssueKind::NoResponse);
    assert_eq!(r.suspect, Suspect::Server);
    assert_eq!(r.key, "event-wave-no-npcs def 3 wave 2");
    assert!(r.actual.contains("no controller either"), "{}", r.actual);
    assert!(r.facts.iter().any(|f| f.0 == "event" && f.1 == "3"));
    // told once
    b.advance(60_000);
    assert!(b.sweep(None).is_empty());
    // the next wave is judged afresh
    wave(&b, RIFT, 3);
    at_the_target(&b, 40);
    b.advance(200_000);
    assert_eq!(b.sweep(None).len(), 1);
    assert_eq!(b.stats().reports, 2);
}

#[test]
fn the_report_names_the_bots_that_stood_in_the_event() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    let c = caps(8, 1.0);
    for bot in [7, 8, 9, 10, 11] {
        assert_eq!(b.join(RIFT, bot, &c), Join::Joined);
    }
    // the bots stand at the target and renew their leases as they tick
    for _ in 0..40 {
        at_the_target(&b, 10);
        for bot in [7, 8, 9, 10, 11] {
            assert!(b.renew(RIFT, bot));
        }
    }
    let w = b.sweep_witnessed(None);
    assert_eq!(w.len(), 1, "{w:?}");
    assert_eq!(&w[0].bots[..], &[7, 8, 9], "the first {MAX_WITNESSES}");
    // nobody there: the report has no witnesses and is still made
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    at_the_target(&b, 40);
    b.advance(400_000);
    let w = b.sweep_witnessed(None);
    assert_eq!(w.len(), 1);
    assert!(w[0].bots.is_empty());
}

#[test]
fn a_wave_npc_that_shows_up_clears_the_suspicion() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    // the controller alone is not a wave NPC
    b.note_unit(901);
    b.note_unit(ARCHER);
    at_the_target(&b, 40);
    b.advance(300_000);
    assert!(b.sweep(None).is_empty());
}

#[test]
fn nobody_at_the_target_means_no_verdict() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    b.advance(600_000);
    assert!(b.sweep(None).is_empty(), "no bot was there to see");
    // a bot that stood there for a few seconds only does not count either
    at_the_target(&b, 10);
    b.advance(600_000);
    assert!(b.sweep(None).is_empty());
    // waves without monsters to wait for (a timer, or a spawner and no kill target) are no case
    wave(&b, RIFT, 4);
    at_the_target(&b, 40);
    b.advance(600_000);
    assert!(b.sweep(None).is_empty());
}

#[test]
fn observing_adds_up_only_the_time_a_bot_kept_reporting() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    // reports every 10 s are gaps, not presence
    for _ in 0..10 {
        b.advance(10_000);
        b.observe(RIFT, true);
    }
    b.advance(300_000);
    assert!(b.sweep(None).is_empty());
    // not near: nothing added
    b.observe(RIFT, false);
}

#[test]
fn a_wave_that_does_not_advance_after_its_kills_is_reported() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    // wave 2 waits for 3 archers and 2 of template 8826
    for obj in 1..=3 {
        b.note_death(obj, ARCHER);
    }
    for obj in 10..=11 {
        b.note_death(obj, 8826);
    }
    assert!(b.sweep(None).is_empty(), "just satisfied");
    b.advance(detect::STUCK_GRACE_MS - 1_000);
    assert!(b.sweep(None).is_empty());
    b.advance(2_000);
    let reports = b.sweep(None);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].key, "event-wave-stuck def 3 wave 2");
    assert!(
        reports[0].actual.contains("8834 3/3"),
        "{}",
        reports[0].actual
    );
    assert!(b.sweep(None).is_empty(), "told once");
    // when the server moves on the suspicion is gone
    wave(&b, RIFT, 3);
    b.advance(120_000);
    assert!(b.sweep(None).is_empty());
}

#[test]
fn a_wave_that_advances_in_time_is_not_reported() {
    let b = board();
    start(&b, RIFT);
    wave(&b, RIFT, 3);
    b.note_death(1, SOLDIER);
    b.advance(10_000);
    assert!(b.sweep(None).is_empty(), "one of two kills");
    b.note_death(2, SOLDIER);
    assert!(b.sweep(None).is_empty());
    b.advance(10_000);
    assert!(b.sweep(None).is_empty());
    wave(&b, RIFT, 4);
    b.advance(600_000);
    assert!(b.sweep(None).is_empty());
}

#[test]
fn an_event_the_server_never_ends_is_reported_and_dropped() {
    let b = board();
    start(&b, RIFT);
    b.advance(3_600_000 + detect::OVERDUE_GRACE_MS - 1);
    assert!(b.sweep(None).is_empty());
    b.advance(1);
    let reports = b.sweep(None);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].key, "event-not-ended def 3");
    assert_eq!(reports[0].kind, IssueKind::Other);
    assert!(b.any());
    // reported once; far past the cap the board forgets the event
    b.advance(30_000);
    assert!(b.sweep(None).is_empty());
    b.advance(100_000);
    assert!(b.sweep(None).is_empty());
    assert!(!b.any());
}

#[test]
fn a_board_without_data_judges_nothing() {
    let b = EventBoard::manual();
    start(&b, RIFT);
    wave(&b, RIFT, 2);
    b.note_death(1, ARCHER);
    b.note_unit(ARCHER);
    b.advance(10_000_000);
    assert!(b.sweep(None).is_empty());
    assert_eq!(b.stats().kills, 0);
}

#[test]
fn the_walk_allowance_follows_the_controllers() {
    let d = def(3, 40);
    // wave 2: the controller is 500 m from the target at 4 m/s
    assert_eq!(detect::walk_allowance_ms(&d, 2, GROUP, None), 125_000);
    // a position given by the packet wins over the data
    let near = Some(Vec3::new(1490.0, 1000.0, 0.0));
    assert_eq!(detect::walk_allowance_ms(&d, 2, GROUP, near), 2_500);
    // a wave without controllers has nothing to walk
    assert_eq!(detect::walk_allowance_ms(&d, 1, GROUP, None), 0);
    // far controllers are capped
    let mut far = d.clone();
    far.controllers = vec![
        controller(100, 0, 900, 0.0),
        controller(101, 2, 901, 90_000.0),
    ]
    .into();
    assert_eq!(
        detect::walk_allowance_ms(&far, 2, GROUP, None),
        detect::WAVE_WALK_MAX_MS
    );
}

#[test]
fn only_the_cheap_checks_run_while_nothing_happens() {
    let b = board();
    // nothing running: none of these may panic or allocate an event
    b.note_death(1, ARCHER);
    b.note_unit(ARCHER);
    b.observe(RIFT, true);
    b.release_bot(3);
    b.leave(RIFT, 3, Leave::Died);
    assert!(b.sweep(Some(population(3, 45))).is_empty());
    assert!(!b.any() && b.views().is_empty());
    assert_eq!(b.stats(), Stats::default());
}
