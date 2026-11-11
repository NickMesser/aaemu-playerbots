//! Tests of the mount and glider decisions and of the glide motion model.

use aa_proto::cs;
use aa_proto::sc_travel::MateInfo;
use aa_proto::sc_world::WorldEvent;

use super::on_gone;
use super::plan::*;
use crate::bot::motion::{Ground, Motion, MotionCfg, MotionEnv, MotionStatus, Pace};
use crate::bot::travel::journey::SkillNote;
use crate::config::MountsCfg;

const T0: u64 = 100_000;
const MOUNT: u32 = 4177;
const GLIDER: u32 = 16392;

fn test_rules() -> Rules {
    Rules {
        mounts: true,
        gliders: true,
        summon_over_m: 250.0,
        dismount_under_m: 40.0,
        idle_despawn_ms: 120_000,
        glide_drop_m: 8.0,
        glide_lookahead_m: 30.0,
    }
}

fn item(template: u32, skill: u32) -> ItemRef {
    ItemRef {
        item_id: u64::from(template) + 1,
        template,
        skill,
        bag_slot: 9,
    }
}

fn facts(now: u64, r: Rules) -> Facts {
    let mut f = Facts::idle(now, r);
    f.me_pos = [0.0, 0.0, 200.0];
    f.moving = true;
    f.remaining = Some(300.0);
    f.default_mount = Some(MOUNT);
    f.default_glider = Some(GLIDER);
    f.glider_equipped = Some(item(GLIDER, 12373));
    f.gm = true;
    f
}

fn skill(skill: u32, result: u8, at_ms: u64) -> SkillNote {
    SkillNote {
        skill,
        result,
        at_ms,
    }
}

#[test]
fn the_rules_follow_the_configuration() {
    let c = MountsCfg {
        summon_over_m: 400.0,
        dismount_under_m: 55.0,
        idle_despawn_secs: 7,
        ..MountsCfg::default()
    };
    let r = super::rules(&c);
    assert!(r.mounts && r.gliders);
    assert_eq!(r.summon_over_m, 400.0);
    assert_eq!(r.dismount_under_m, 55.0);
    assert_eq!(r.idle_despawn_ms, 7_000);
}

// ---- when to summon -------------------------------------------------------------------------

#[test]
fn a_mount_is_summoned_only_for_a_long_trip() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.mount_item = Some(item(MOUNT, 10602));
    assert_eq!(
        &*step(&mut mem, &f),
        &[Act::Summon {
            item_id: u64::from(MOUNT) + 1,
            template: MOUNT,
            skill: 10602,
        }]
    );
    assert!(matches!(mem.phase, Phase::Summoning { .. }));

    let mut short = facts(T0, r);
    short.remaining = Some(r.summon_over_m - 1.0);
    short.mount_item = Some(item(MOUNT, 10602));
    let mut mem = Mem::default();
    assert!(step(&mut mem, &short).is_empty());
    assert_eq!(mem.phase, Phase::Idle);

    let mut combat = facts(T0, r);
    combat.combat = true;
    combat.mount_item = Some(item(MOUNT, 10602));
    let mut mem = Mem::default();
    assert!(step(&mut mem, &combat).is_empty());

    let mut busy = facts(T0, r);
    busy.busy = true;
    busy.mount_item = Some(item(MOUNT, 10602));
    let mut mem = Mem::default();
    assert!(step(&mut mem, &busy).is_empty());

    let mut standing = facts(T0, r);
    standing.moving = false;
    standing.remaining = None;
    standing.mount_item = Some(item(MOUNT, 10602));
    let mut mem = Mem::default();
    assert!(step(&mut mem, &standing).is_empty());
}

#[test]
fn without_an_item_a_gm_grants_one_and_a_plain_bot_backs_off() {
    let r = test_rules();
    let mut mem = Mem::default();
    let f = facts(T0, r);
    assert_eq!(&*step(&mut mem, &f), &[Act::GrantMount { template: MOUNT }]);
    assert!(matches!(mem.phase, Phase::NeedItem { .. }));

    let later = facts(T0 + ITEM_WAIT_MS + 1, r);
    assert!(step(&mut mem, &later).is_empty());
    assert_eq!(mem.phase, Phase::Idle);
    assert!(mem.refused_until_ms > later.now_ms);

    let mut mem = Mem::default();
    let mut poor = facts(T0, r);
    poor.gm = false;
    assert!(step(&mut mem, &poor).is_empty());
    assert!(mem.refused_until_ms >= T0 + NO_ITEM_MS);
}

#[test]
fn a_server_without_mount_data_does_not_look_every_tick() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.default_mount = None;
    f.default_glider = None; // (the glider grant already wears one in the default facts)
    f.glider_equipped = Some(item(GLIDER, 12373));
    assert!(step(&mut mem, &f).is_empty());
    assert!(mem.refused_until_ms >= T0 + NO_ITEM_MS);
}

#[test]
fn a_refused_summon_backs_off_and_tries_again_later() {
    let r = test_rules();
    let mut mem = Mem::default();
    let f = facts(T0, r);
    assert_eq!(&*step(&mut mem, &f), &[Act::GrantMount { template: MOUNT }]);
    let mut with_item = facts(T0 + 500, r);
    with_item.mount_item = Some(item(MOUNT, 10602));
    assert_eq!(step(&mut mem, &with_item).len(), 1);
    let mut refused = with_item;
    refused.now_ms = T0 + 600;
    refused.skill_note = Some(skill(10602, 1, T0 + 590));
    assert!(step(&mut mem, &refused).is_empty());
    assert_eq!(mem.phase, Phase::Idle);
    assert_eq!(mem.refused_until_ms, T0 + 600 + REFUSE_MS);
    let mut inside = refused;
    inside.now_ms = T0 + 601;
    assert!(step(&mut mem, &inside).is_empty());
    let mut after = inside;
    after.now_ms = T0 + 601 + REFUSE_MS;
    after.skill_note = None;
    assert_eq!(step(&mut mem, &after).len(), 1);
}

// ---- riding ---------------------------------------------------------------------------------

fn out_with_mate() -> Mem {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.mount_item = Some(item(MOUNT, 10602));
    let _ = step(&mut mem, &f);
    mem.on_spawned(5, u64::from(MOUNT) + 1, T0 + 20);
    mem.on_mate_state(0x500, 5, 1, 1, [0.0, 3.0, 200.0]);
    let _ = step(&mut mem, &f);
    assert!(matches!(mem.phase, Phase::Mounting { .. }));
    mem
}

#[test]
fn a_mate_is_boarded_and_a_fight_makes_the_bot_leave_it() {
    let r = test_rules();
    let (mut mem, _) = (out_with_mate(), ());
    assert_eq!(mem.phase, Phase::Mounting { since_ms: T0 });
    mem.on_attached(0x500, 1, T0 + 30);
    assert!(mem.mounted());
    let mut f = facts(T0 + 40, r);
    f.mount_item = Some(item(MOUNT, 10602));
    f.combat = true;
    assert_eq!(step(&mut mem, &f)[0], Act::Dismount { tl: 5 });
    assert!(matches!(mem.phase, Phase::Dismounting { .. }));
    mem.on_detached(T0 + 50);
    assert!(!mem.mounted());
    assert_eq!(mem.mate_tl, 5);

    let mut idle = facts(T0 + 50 + r.idle_despawn_ms, r);
    idle.moving = false;
    idle.remaining = None;
    assert_eq!(step(&mut mem, &idle)[0], Act::Despawn { tl: 5 });
    let mut gone = idle;
    gone.now_ms += DESPAWN_WAIT_MS;
    assert!(step(&mut mem, &gone).is_empty());
    assert_eq!(mem.mate_tl, 0);
}

#[test]
fn a_mounted_bot_gets_off_near_the_goal_and_for_blocking_situations() {
    let r = test_rules();
    let (mut mem, _) = (out_with_mate(), ());
    mem.on_attached(0x500, 1, T0 + 30);
    for (name, change) in [
        ("a short rest of the trip", 0u8),
        ("a fight", 1),
        ("a blocking situation", 2),
        ("a stop", 3),
    ] {
        let mut f = facts(T0 + 40, r);
        match change {
            0 => f.remaining = Some(r.dismount_under_m),
            1 => f.combat = true,
            2 => f.busy = true,
            _ => {
                f.moving = false;
                f.remaining = None;
            }
        }
        assert!(
            matches!(step(&mut mem, &f).first(), Some(Act::Dismount { tl: 5 })),
            "{name} must dismount"
        );
        mem.on_detached(T0 + 50);
        mem.on_attached(0x500, 1, T0 + 60);
    }
}

#[test]
fn a_far_mate_is_dismissed_and_summoned_again() {
    let r = test_rules();
    let mut mem = Mem {
        mate_tl: 5,
        mate_obj: 0x500,
        mate_pos: Some([500.0, 0.0, 200.0]),
        mount_item: Some(item(MOUNT, 10602)),
        ..Mem::default()
    };
    let mut f = facts(T0, r);
    f.mount_item = Some(item(MOUNT, 10602));
    assert_eq!(step(&mut mem, &f)[0], Act::Despawn { tl: 5 });
    assert!(matches!(mem.phase, Phase::Despawning { .. }));
    let mut later = f;
    later.now_ms = T0 + DESPAWN_WAIT_MS;
    assert!(step(&mut mem, &later).is_empty());
    assert_eq!(mem.mate_tl, 0);
    assert!(mem.next_try_ms > later.now_ms);
    later.now_ms = mem.next_try_ms;
    assert!(matches!(
        step(&mut mem, &later).first(),
        Some(Act::Summon { .. })
    ));
}

#[test]
fn a_mate_that_never_reports_its_object_is_dismissed() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.mount_item = Some(item(MOUNT, 10602));
    let _ = step(&mut mem, &f);
    mem.on_spawned(5, u64::from(MOUNT) + 1, T0 + 10);
    let mut late = facts(T0 + 10 + MATE_OBJ_WAIT_MS, r);
    late.mount_item = Some(item(MOUNT, 10602));
    assert_eq!(step(&mut mem, &late)[0], Act::Despawn { tl: 5 });
}

#[test]
fn attach_events_of_a_boat_do_not_mount_the_bot() {
    let mut mem = Mem {
        mate_tl: 5,
        mate_obj: 0x500,
        ..Mem::default()
    };
    mem.on_attached(0x600, 1, T0);
    assert!(!mem.mounted());
    mem.on_attached(0x500, 2, T0);
    assert!(!mem.mounted());
    mem.on_attached(0x500, 1, T0);
    assert!(mem.mounted());
}

#[test]
fn a_dead_bot_does_nothing() {
    let r = test_rules();
    let mut f = facts(T0, r);
    f.dead = true;
    f.mount_item = Some(item(MOUNT, 10602));
    let mut mem = Mem::default();
    assert!(step(&mut mem, &f).is_empty());
}

// ---- gliders --------------------------------------------------------------------------------

#[test]
fn a_glider_is_equipped_and_deployed_from_a_ledge() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.glider_equipped = None;
    f.glider_item = Some(item(GLIDER, 12373));
    assert_eq!(
        step(&mut mem, &f)[0],
        Act::EquipGlider {
            item_id: u64::from(GLIDER) + 1,
            template: GLIDER,
            bag_slot: 9,
        }
    );
    assert!(mem.glider_pending.is_some());

    let mut worn = facts(T0 + 500, r);
    worn.glider_item = None;
    worn.glider_equipped = Some(item(GLIDER, 12373));
    assert!(step(&mut mem, &worn).is_empty());
    assert!(mem.glider_pending.is_none());

    let mut ledge = worn;
    ledge.now_ms = T0 + 1_000;
    ledge.ledge = Some(Ledge {
        dir: (0.0, 1.0),
        ground: 100.0,
        drop_m: 100.0,
        edge_m: 2.0,
    });
    assert_eq!(
        step(&mut mem, &ledge)[0],
        Act::Glide {
            item_id: u64::from(GLIDER) + 1,
            template: GLIDER,
            skill: 12373,
            dir: (0.0, 1.0),
        }
    );
    let mut again = ledge;
    again.now_ms = T0 + 1_001;
    assert!(step(&mut mem, &again).is_empty());
}

#[test]
fn no_glide_without_a_worn_glider_a_ledge_or_a_long_trip() {
    let r = test_rules();
    let ledge = Some(Ledge {
        dir: (0.0, 1.0),
        ground: 100.0,
        drop_m: 100.0,
        edge_m: 2.0,
    });
    for (name, mut f) in [
        ("no glider", {
            let mut f = facts(T0, r);
            f.gm = false;
            f.ledge = ledge;
            f
        }),
        ("no ledge", {
            let mut f = facts(T0, r);
            f.rules.mounts = false;
            f
        }),
        ("short trip", {
            let mut f = facts(T0, r);
            f.rules.mounts = false;
            f.remaining = Some(GLIDE_MIN_REMAINING_M - 1.0);
            f.ledge = ledge;
            f
        }),
        ("mounted", {
            let mut f = facts(T0, r);
            f.rules.mounts = false;
            f.ledge = ledge;
            f
        }),
    ] {
        f.rules.mounts = false;
        f.glider_equipped = None;
        f.glider_item = None;
        if name != "no glider" {
            f.glider_equipped = Some(item(GLIDER, 12373));
        }
        let mut mem = if name == "mounted" {
            Mem {
                mate_tl: 5,
                mate_obj: 0x500,
                ..Mem::default()
            }
        } else {
            Mem::default()
        };
        if name == "mounted" {
            mem.on_attached(0x500, 1, T0);
        }
        assert!(step(&mut mem, &f).is_empty(), "{name} must not glide");
    }
}

#[test]
fn a_carried_pack_blocks_the_glider() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.glider_equipped = None;
    f.glider_item = Some(item(GLIDER, 12373));
    f.back_blocked = true;
    f.rules.mounts = false;
    assert!(step(&mut mem, &f).is_empty());
    assert!(mem.glider_pending.is_none());
}

#[test]
fn a_gm_grants_a_glider_when_the_bag_has_none() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.rules.mounts = false;
    f.glider_equipped = None;
    let acts = step(&mut mem, &f);
    assert_eq!(&*acts, &[Act::GrantGlider { template: GLIDER }]);
    assert!(mem.glider_pending.is_some());
    let mut with_item = facts(T0 + 500, r);
    with_item.glider_equipped = None;
    with_item.glider_item = Some(item(GLIDER, 12373));
    assert!(matches!(
        step(&mut mem, &with_item).first(),
        Some(Act::EquipGlider { .. })
    ));
}

// ---- the ledge ------------------------------------------------------------------------------

fn cliff_ground(x: f32, y: f32) -> Option<f32> {
    let _ = x;
    Some(if y < 10.0 { 200.0 } else { 100.0 })
}

#[test]
fn a_cliff_ahead_is_a_glide_and_a_slope_is_not() {
    let pos = [0.0, 0.0, 200.0];
    let ledge = find_ledge(cliff_ground, pos, 0.0, 30.0, 8.0).expect("a cliff");
    assert_eq!(ledge.ground, 100.0);
    assert_eq!(ledge.drop_m, 100.0);
    assert!((ledge.dir.0).abs() < 1e-6 && (ledge.dir.1 - 1.0).abs() < 1e-6);

    let slope = |x: f32, y: f32| {
        let _ = x;
        Some(200.0 - (y / 2.0).floor())
    };
    assert!(find_ledge(slope, pos, 0.0, 30.0, 8.0).is_none());

    let wall = |x: f32, y: f32| {
        let _ = x;
        Some(if (5.0..10.0).contains(&y) {
            260.0
        } else if y < 5.0 {
            200.0
        } else {
            100.0
        })
    };
    assert!(find_ledge(wall, pos, 0.0, 30.0, 8.0).is_none());

    let sea = |x: f32, y: f32| {
        let _ = x;
        Some(if y < 10.0 { 200.0 } else { 98.0 })
    };
    assert!(find_ledge(sea, pos, 0.0, 30.0, 8.0).is_none());

    assert!(find_ledge(|_, _| None, pos, 0.0, 30.0, 8.0).is_none());
    assert!(find_ledge(
        |x, y| if y > 4.0 { None } else { cliff_ground(x, y) },
        pos,
        0.0,
        30.0,
        8.0
    )
    .is_none());

    assert!(find_ledge(|_, _| Some(200.0), pos, 0.0, 30.0, 8.0).is_none());
}

// ---- the glide motion -----------------------------------------------------------------------

struct Valley;

impl Ground for Valley {
    fn ground_z(&self, _x: f32, _y: f32) -> Option<f32> {
        Some(200.0)
    }
}

#[test]
fn a_glide_descends_lands_and_continues_the_trip() {
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 0.0, 250.0], 0.0, 0);
    m.go_to(0.0, 1000.0, 0.0, Pace::Run, 0);
    assert!(m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_000));
    assert!(m.gliding());

    let mut out = Vec::new();
    let mut rng = fastrand::Rng::with_seed(7);
    let valley = Valley;
    let mut zs = Vec::new();
    let mut glide_packet = false;
    for i in 1..=300u64 {
        let mut env = MotionEnv {
            ground: &valley,
            now_ms: 1_000 + i * 100,
            out: &mut out,
            rng: &mut rng,
            send_gap_ms: 100,
            combat: false,
        };
        m.tick(&mut env);
        zs.push(m.pos()[2]);
        for p in &out {
            if p.opcode == aa_proto::opcodes::cs::CSMoveUnitPacket
                && p.body.get(30) == Some(&cs::STANCE_GLIDE)
            {
                glide_packet = true;
            }
        }
        out.clear();
        if !m.gliding() {
            break;
        }
    }
    let first = zs.first().copied().unwrap_or(250.0);
    let last = *zs.last().unwrap_or(&250.0);
    assert!(first < 250.0, "the glide must descend, was {first}");
    assert!(last <= 201.0, "the glide must land, was {last}");
    assert!(!m.gliding(), "the glide must end");
    assert!(glide_packet, "the updates must carry the glide stance");
    assert_eq!(m.status(), MotionStatus::Moving);
}

/// Level ground at 200 m up to y = 5, then a cliff down to 150 m.
struct Cliff;

impl Ground for Cliff {
    fn ground_z(&self, _x: f32, y: f32) -> Option<f32> {
        Some(if y < 5.0 { 200.0 } else { 150.0 })
    }
}

/// Ticks a glide of a bot standing at (0, 0, `z`) every 100 ms for up to `ticks`, until it ends.
/// Returns the heights and the y positions reached, and whether the glide is still on.
fn run_glide(ground: &dyn Ground, z: f32, ticks: u64) -> (Vec<f32>, Vec<f32>, bool, Motion) {
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 0.0, z], 0.0, 1_000);
    m.go_to(0.0, 1000.0, 0.0, Pace::Run, 1_000);
    assert!(m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_000));
    let mut out = Vec::new();
    let mut rng = fastrand::Rng::with_seed(7);
    let (mut zs, mut ys) = (Vec::new(), Vec::new());
    for i in 1..=ticks {
        let mut env = MotionEnv {
            ground,
            now_ms: 1_000 + i * 100,
            out: &mut out,
            rng: &mut rng,
            send_gap_ms: 100,
            combat: false,
        };
        m.tick(&mut env);
        zs.push(m.pos()[2]);
        ys.push(m.pos()[1]);
        out.clear();
        if !m.gliding() {
            break;
        }
    }
    let on = m.gliding();
    (zs, ys, on, m)
}

#[test]
fn a_glide_that_starts_before_the_edge_runs_on_the_ground_and_then_descends() {
    // The bot stands on the level ground 4 m before the edge (the glide starts within
    // `GLIDE_START_EDGE_M`): one tick moves it less than the distance, so it must not land.
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 1.0, 200.0], 0.0, 1_000);
    m.go_to(0.0, 1000.0, 0.0, Pace::Run, 1_000);
    assert!(m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_000));
    let mut out = Vec::new();
    let mut rng = fastrand::Rng::with_seed(7);
    let cliff = Cliff;
    let mut zs = Vec::new();
    for i in 1..=400u64 {
        let mut env = MotionEnv {
            ground: &cliff,
            now_ms: 1_000 + i * 100,
            out: &mut out,
            rng: &mut rng,
            send_gap_ms: 100,
            combat: false,
        };
        m.tick(&mut env);
        zs.push((m.pos()[1], m.pos()[2]));
        out.clear();
        if !m.gliding() {
            break;
        }
    }
    // on the ground (not sunk into it) while it runs up to the edge ...
    let before_edge: Vec<_> = zs.iter().filter(|(y, _)| *y < 5.0).collect();
    assert!(!before_edge.is_empty());
    assert!(
        before_edge.iter().all(|(_, z)| *z == 200.0),
        "run-up heights {before_edge:?}"
    );
    // ... then it flies down, lands on the lower ground and the trip goes on
    assert!(zs.iter().any(|(_, z)| *z < 199.0 && *z > 151.0), "no glide");
    assert!(!m.gliding(), "the glide must end");
    assert!((m.pos()[2] - 150.0).abs() < 0.1, "landed at {}", m.pos()[2]);
    assert_eq!(m.status(), MotionStatus::Moving);
}

#[test]
fn a_glide_with_no_edge_ahead_ends_as_a_landing_after_the_run_up() {
    // level ground all the way: the bot runs on the ground, then the glide ends on its own
    let (zs, _, on, m) = run_glide(&Valley, 200.0, 100);
    assert!(!on, "the glide must end when the edge is not there");
    assert!(zs.iter().all(|z| *z == 200.0), "it never leaves the ground");
    assert_eq!(m.pos()[2], 200.0);
    assert_eq!(m.status(), MotionStatus::Moving);
}

#[test]
fn a_ledge_far_ahead_does_not_start_a_glide_until_the_edge_is_close() {
    let r = test_rules();
    let mut mem = Mem::default();
    let mut f = facts(T0, r);
    f.glider_item = None;
    f.glider_equipped = Some(item(GLIDER, 12373));
    f.ledge = Some(Ledge {
        dir: (0.0, 1.0),
        ground: 100.0,
        drop_m: 100.0,
        edge_m: GLIDE_START_EDGE_M + 2.0,
    });
    let glides = |acts: &[Act]| acts.iter().any(|a| matches!(a, Act::Glide { .. }));
    assert!(!glides(&step(&mut mem, &f)), "the edge is still far");
    f.now_ms = T0 + 500;
    f.ledge = f.ledge.map(|l| Ledge {
        edge_m: GLIDE_START_EDGE_M,
        ..l
    });
    assert!(glides(&step(&mut mem, &f)), "the edge is close enough");
}

#[test]
fn an_unanswered_dismount_is_not_sent_again_every_few_seconds() {
    let r = test_rules();
    let mut mem = Mem {
        mate_tl: 5,
        phase: Phase::Mounted { since_ms: T0 },
        ..Mem::default()
    };
    let mut f = facts(T0, r);
    f.combat = true; // a fight: the bot has to get off
    assert!(matches!(step(&mut mem, &f)[..], [Act::Dismount { .. }]));
    // the server never answers: the wait ends and the bot is "mounted" again ...
    f.now_ms = T0 + DISMOUNT_WAIT_MS;
    assert!(step(&mut mem, &f).is_empty());
    assert!(matches!(mem.phase, Phase::Mounted { .. }));
    // ... but the request is not repeated at once
    f.now_ms += 1_000;
    assert!(step(&mut mem, &f).is_empty(), "no retry loop");
    f.now_ms = T0 + DISMOUNT_WAIT_MS + DISMOUNT_RETRY_MS;
    assert!(matches!(step(&mut mem, &f)[..], [Act::Dismount { .. }]));
}

#[test]
fn find_ledge_reports_the_distance_to_the_edge() {
    // level ground, then a cliff at y = 9: the first sample past the edge is 10 m ahead
    let ground = |_x: f32, y: f32| Some(if y < 9.0 { 200.0 } else { 100.0 });
    let l = find_ledge(ground, [0.0, 0.0, 200.0], 0.0, 30.0, 8.0).expect("a ledge");
    assert_eq!(l.edge_m, 10.0);
    assert_eq!(l.dir, (0.0, 1.0));
}

#[test]
fn set_speeds_changes_both_paces() {
    let mut m = Motion::new(MotionCfg::default());
    m.set_speeds(10.0, 10.0);
    assert_eq!(m.speed(Pace::Run), 10.0);
    assert_eq!(m.speed(Pace::Walk), 10.0);
    m.set_speeds(5.4, 1.8);
    assert_eq!(m.speed(Pace::Run), 5.4);
    assert_eq!(m.speed(Pace::Walk), 1.8);
}

#[test]
fn a_goal_reasking_for_the_same_trip_keeps_the_glide() {
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 0.0, 250.0], 0.0, 0);
    m.go_to(0.0, 100.0, 0.0, Pace::Run, 0);
    assert!(m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_000));
    m.go_to(0.0, 100.0, 0.0, Pace::Run, 1_100);
    assert!(m.gliding(), "the same goal again must not cancel the glide");
    m.go_to(0.0, 120.0, 0.0, Pace::Run, 1_200);
    assert!(!m.gliding(), "a new goal must cancel it");
}

#[test]
fn a_new_trip_or_a_stop_ends_a_glide() {
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 0.0, 250.0], 0.0, 0);
    m.go_to(0.0, 100.0, 0.0, Pace::Run, 0);
    assert!(m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_000));
    m.stop();
    assert!(!m.gliding());
    assert!(!m.begin_glide((0.0, 1.0), 9.0, 3.0, 1_001));
}

#[test]
fn going_away_dismisses_the_mate_once() {
    let mut state = crate::bot::state::BotState::new();
    let mut motion = Motion::new(MotionCfg::default());
    {
        let mem = state.travel.mount_mut();
        mem.mate_tl = 5;
        mem.phase = Phase::Mounted { since_ms: T0 };
    }
    let mut out = Vec::new();
    on_gone(&mut state, &mut motion, &mut out, (5.4, 1.8));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].opcode, aa_proto::opcodes::cs::CSRemoveMatePacket);
    assert_eq!(state.travel.mount_ref().unwrap().mate_tl, 0);
    out.clear();
    on_gone(&mut state, &mut motion, &mut out, (5.4, 1.8));
    assert!(out.is_empty(), "the mate is only dismissed once");
}

#[test]
fn the_mount_events_reach_the_travel_state() {
    let mut state = crate::bot::state::BotState::new();
    state.me.char_id = 1;
    state.me.obj = 0x400;
    state.now_ms = T0;
    let ev = WorldEvent::MateSpawned(Box::new(MateInfo {
        tl: 5,
        id: 9,
        item_id: 77,
        user_state: 1,
        experience: 0,
        mileage: 0,
        spawn_delay_ms: 0,
        skills: Box::default(),
    }));
    state.apply(&aa_proto::sc::ServerEvent::World(ev.clone()));
    // A spawn nobody asked for (a battle pet) is not adopted.
    assert!(state
        .travel
        .inner_ref()
        .is_none_or(|i| i.mount.mate_tl == 0));
    // Once this feature's summon is in flight, the spawn is its mate.
    state.travel.mount_mut().phase = Phase::Summoning {
        item_id: 77,
        template: MOUNT,
        skill: 10602,
        sent_ms: T0,
    };
    state.apply(&aa_proto::sc::ServerEvent::World(ev));
    assert!(state
        .travel
        .inner_ref()
        .is_some_and(|i| i.mount.mate_tl == 5));
    state.apply(&aa_proto::sc::ServerEvent::World(
        WorldEvent::UnitAttached {
            child: 0x400,
            point: 1,
            parent: 0x500,
            reason: 4,
        },
    ));
    // The mate object id has not been learned yet, so the attach is not the bot's mount.
    assert!(!state.travel.inner_ref().unwrap().mount.mounted());
}

#[test]
fn a_riding_bot_sends_the_mates_object_id() {
    let mut m = Motion::new(MotionCfg::default());
    m.enter_world(0x400, [0.0, 0.0, 200.0], 0.0, 0);
    m.go_to(0.0, 100.0, 0.0, Pace::Run, 0);
    m.set_move_obj(0x500);
    assert_eq!(m.move_obj(), 0x500);
    let valley = Valley;
    let mut out = Vec::new();
    let mut rng = fastrand::Rng::with_seed(1);
    let mut env = MotionEnv {
        ground: &valley,
        now_ms: 1_000,
        out: &mut out,
        rng: &mut rng,
        send_gap_ms: 100,
        combat: false,
    };
    m.tick(&mut env);
    let p = out
        .iter()
        .find(|p| p.opcode == aa_proto::opcodes::cs::CSMoveUnitPacket)
        .expect("a movement update");
    assert_eq!(
        &p.body[..3],
        &[0x00, 0x05, 0x00],
        "the packet names the mate (bc 0x500)"
    );
    m.set_move_obj(0);
    assert_eq!(m.move_obj(), 0x400);
}
