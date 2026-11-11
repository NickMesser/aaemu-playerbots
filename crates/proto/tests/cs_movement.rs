//! Byte-exact tests of `CSMoveUnitPacket` and the yaw conventions.
//!
//! Server read: `aaemu-game/src/packets/c2g_world.rs:224`, `UnitMove::read`
//! `models/movement.rs:418`; the facing values are the server's own compass checks
//! (`+Y = 0, -X = 32, +X = -32, -Y = 64`, test client `game.rs` `facing_tests`).

use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use aa_proto::cs::{self, JumpPhase, MoveUnit};
use aa_proto::game::LEVEL_GAME;
use aa_proto::opcodes::cs as op;
use aa_proto::pos::encode_position;

fn base() -> MoveUnit {
    MoveUnit {
        obj: 0x03_0201,
        time: 0x0A0B_0C0D,
        pos: (100.0, 200.0, 50.0),
        vel: (0.0, 0.0, 0.0),
        yaw: 0.0,
        moving: false,
        stance: cs::STANCE_RELAXED,
        alertness: cs::ALERT_IDLE,
        jump: JumpPhase::None,
    }
}

/// Wire units of a velocity of `v` m/s: the 1.2 client reads `wire * 60 / 32767`.
fn wire(v: f32) -> i16 {
    (v * 32767.0 / 60.0).round() as i16
}

/// `bc obj, u8 1, u32 time, u8 flags, position, 3 x i16 vel, 3 x i8 rot, 3 x i8 delta,
/// u8 stance, u8 alertness, u8 actor`.
#[allow(clippy::too_many_arguments)]
fn expected(
    flags: u8,
    pos: (f32, f32, f32),
    vel: [i16; 3],
    rot_z: i8,
    delta: [i8; 3],
    stance: u8,
    alertness: u8,
    actor: u8,
) -> Vec<u8> {
    let mut b = vec![1, 2, 3, 1, 0x0D, 0x0C, 0x0B, 0x0A, flags];
    b.extend_from_slice(&encode_position(pos.0, pos.1, pos.2));
    for v in vel {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&[0, 0, rot_z as u8]);
    b.extend(delta.map(|d| d as u8));
    b.extend_from_slice(&[stance, alertness, actor]);
    b
}

#[track_caller]
fn check(m: &MoveUnit, body: Vec<u8>) {
    let out = cs::move_unit(m);
    assert_eq!(out.level, LEVEL_GAME);
    assert_eq!(out.opcode, op::CSMoveUnitPacket);
    assert_eq!(out.body, body);
    assert_eq!(out.body.len(), 33);
}

#[test]
fn running_forward_along_y() {
    // 5.4 m/s north: velocity 2949 (546.1 wire units per m/s), running actor flags 4, forward
    // stick fully pushed.
    let m = MoveUnit {
        vel: (0.0, 5.4, 0.0),
        moving: true,
        ..base()
    };
    assert_eq!(wire(5.4), 2949);
    check(
        &m,
        expected(0x02, m.pos, [0, 2949, 0], 0, [0, 127, 0], 1, 0, 4),
    );
}

#[test]
fn the_velocity_scale_is_the_clients_sixty_over_32767() {
    // What a watching 1.2 client decodes (`wire * 60 / 32767`) is what the bot walks.
    for v in [0.5f32, 1.8, 3.0, 5.4, 5.9, 8.06, 20.0] {
        let m = MoveUnit {
            vel: (v, 0.0, 0.0),
            moving: true,
            ..base()
        };
        let body = cs::move_unit(&m).body;
        let got = i16::from_le_bytes([body[18], body[19]]) as f32 * 60.0 / 32767.0;
        assert!((got - v).abs() < 0.002, "{v} m/s decodes as {got}");
    }
}

#[test]
fn walking_is_announced_with_actor_flags_5_and_half_the_stick() {
    let m = MoveUnit {
        vel: (1.0, 0.0, 0.0),
        yaw: -FRAC_PI_2,
        moving: true,
        ..base()
    };
    check(
        &m,
        expected(0x02, m.pos, [wire(1.0), 0, 0], -32, [0, 63, 0], 1, 0, 5),
    );
    // 3.0 m/s exactly is still a walk, anything above runs.
    let at_threshold = MoveUnit {
        vel: (3.0, 0.0, 0.0),
        moving: true,
        ..base()
    };
    let b = cs::move_unit(&at_threshold).body;
    assert_eq!((b[32], b[28] as i8), (5, 63));
    let above = MoveUnit {
        vel: (3.01, 0.0, 0.0),
        moving: true,
        ..base()
    };
    let b = cs::move_unit(&above).body;
    assert_eq!((b[32], b[28] as i8), (4, 127));
    // the vertical speed of a jump does not turn a walk into a run
    let slow_up = MoveUnit {
        vel: (1.0, 0.0, 8.0),
        moving: true,
        ..base()
    };
    assert_eq!(cs::move_unit(&slow_up).body[32], 5);
}

#[test]
fn negative_and_clamped_velocities() {
    let m = MoveUnit {
        vel: (-2.0, 100.0, -100.0),
        moving: true,
        ..base()
    };
    // -2 m/s = -1092; +/- 100 m/s saturate the i16.
    check(
        &m,
        expected(0x02, m.pos, [-1092, 32767, -32768], 0, [0, 127, 0], 1, 0, 4),
    );
}

#[test]
fn a_stop_has_no_velocity_and_no_input() {
    let m = MoveUnit {
        // The velocity of a stopped update is ignored.
        vel: (5.0, 5.0, 0.0),
        yaw: FRAC_PI_2,
        moving: false,
        ..base()
    };
    // a standing real client reports the ground (actor flag 4), not "airborne"
    check(&m, expected(0x04, m.pos, [0, 0, 0], 32, [0, 0, 0], 1, 0, 4));
}

#[test]
fn combat_stance_and_alertness_are_passed_through() {
    let m = MoveUnit {
        stance: cs::STANCE_COMBAT,
        alertness: cs::ALERT_COMBAT,
        ..base()
    };
    check(&m, expected(0x04, m.pos, [0, 0, 0], 0, [0, 0, 0], 0, 2, 4));
}

#[test]
fn swimming_is_the_swim_stance() {
    let m = MoveUnit {
        vel: (0.0, 3.5, 0.0),
        moving: true,
        stance: cs::STANCE_SWIM,
        ..base()
    };
    let body = cs::move_unit(&m).body;
    assert_eq!(body[30], 2, "stance");
    assert_eq!(body[8], 0x02);
}

#[test]
fn a_standing_jump_is_an_event_with_the_jump_actor_flag() {
    // take-off, standing: header 0x04, actor 0x10, vel.z = the take-off speed
    let up = MoveUnit {
        vel: (0.0, 0.0, 8.06),
        jump: JumpPhase::TakeOff,
        ..base()
    };
    check(
        &up,
        expected(0x04, up.pos, [0, 0, wire(8.06)], 0, [0, 0, 0], 1, 0, 0x10),
    );
    // in the air, standing: no input, no ground, the height and vertical speed are sent
    let air = MoveUnit {
        pos: (100.0, 200.0, 51.2),
        vel: (0.0, 0.0, -2.0),
        jump: JumpPhase::Air,
        ..base()
    };
    check(
        &air,
        expected(0x00, air.pos, [0, 0, wire(-2.0)], 0, [0, 0, 0], 1, 0, 0),
    );
}

#[test]
fn a_running_jump_keeps_the_stick_and_loses_the_ground() {
    let up = MoveUnit {
        vel: (0.0, 5.4, 8.06),
        moving: true,
        jump: JumpPhase::TakeOff,
        ..base()
    };
    check(
        &up,
        expected(
            0x06,
            up.pos,
            [0, wire(5.4), wire(8.06)],
            0,
            [0, 127, 0],
            1,
            0,
            0x10,
        ),
    );
    let air = MoveUnit {
        pos: (100.0, 201.0, 51.0),
        vel: (0.0, 5.4, 1.0),
        moving: true,
        jump: JumpPhase::Air,
        ..base()
    };
    check(
        &air,
        expected(
            0x02,
            air.pos,
            [0, wire(5.4), wire(1.0)],
            0,
            [0, 127, 0],
            1,
            0,
            0,
        ),
    );
    // the landing is an ordinary running update again: ground flag, no vertical speed
    let down = MoveUnit {
        vel: (0.0, 5.4, 0.0),
        moving: true,
        ..base()
    };
    check(
        &down,
        expected(0x02, down.pos, [0, wire(5.4), 0], 0, [0, 127, 0], 1, 0, 4),
    );
    // a walking hop keeps the walk bit in the air
    let hop = MoveUnit {
        vel: (1.8, 0.0, 5.0),
        moving: true,
        jump: JumpPhase::Air,
        ..base()
    };
    assert_eq!(cs::move_unit(&hop).body[32], 1);
    let hop_up = MoveUnit {
        jump: JumpPhase::TakeOff,
        ..hop
    };
    assert_eq!(cs::move_unit(&hop_up).body[32], 0x11);
}

#[test]
fn the_touchdown_is_ground_plus_jump_with_the_event_flag() {
    // standing: header 0x04, actor 0x14 (ground and jump), no vertical speed
    let down = MoveUnit {
        jump: JumpPhase::Land,
        ..base()
    };
    check(
        &down,
        expected(0x04, down.pos, [0, 0, 0], 0, [0, 0, 0], 1, 0, 0x14),
    );
    // running: the stick is still held, the horizontal speed goes on
    let run = MoveUnit {
        vel: (0.0, 5.4, 0.0),
        moving: true,
        jump: JumpPhase::Land,
        ..base()
    };
    check(
        &run,
        expected(0x06, run.pos, [0, wire(5.4), 0], 0, [0, 127, 0], 1, 0, 0x14),
    );
    let walk = MoveUnit {
        vel: (0.0, 1.8, 0.0),
        moving: true,
        jump: JumpPhase::Land,
        ..base()
    };
    assert_eq!(cs::move_unit(&walk).body[32], 0x15);
}

#[test]
fn the_fall_velocity_is_never_sent() {
    for jump in [
        JumpPhase::None,
        JumpPhase::TakeOff,
        JumpPhase::Air,
        JumpPhase::Land,
    ] {
        for moving in [false, true] {
            let m = MoveUnit {
                vel: (0.0, 5.4, -30.0),
                moving,
                jump,
                ..base()
            };
            let body = cs::move_unit(&m).body;
            assert_eq!(body.len(), 33, "no fall velocity short follows");
            assert_eq!(body[32] & 0x80, 0);
        }
    }
}

#[test]
fn the_emote_packet_is_two_object_ids_and_the_emotion() {
    let out = cs::express_emotion(0x03_0201, 0, cs::emotion::GREET);
    assert_eq!(out.level, LEVEL_GAME);
    assert_eq!(out.opcode, op::CSExpressEmotionPacket);
    assert_eq!(out.body, vec![1, 2, 3, 0, 0, 0, 12, 0, 0, 0]);
    let out = cs::express_emotion(7, 0x01_0000, 62);
    assert_eq!(out.body, vec![7, 0, 0, 0, 0, 1, 62, 0, 0, 0]);
}

#[test]
fn the_position_uses_the_packed_world_encoding() {
    let m = MoveUnit {
        pos: (-1234.5, 16000.25, 123.5),
        ..base()
    };
    let out = cs::move_unit(&m);
    assert_eq!(&out.body[9..18], &encode_position(-1234.5, 16000.25, 123.5));
}

#[test]
fn yaw_towards_follows_the_server_compass() {
    let o = (0.0, 0.0);
    assert_eq!(cs::yaw_towards(o, (0.0, 1.0)), 0.0);
    assert!((cs::yaw_towards(o, (-1.0, 0.0)) - FRAC_PI_2).abs() < 1e-6);
    assert!((cs::yaw_towards(o, (1.0, 0.0)) + FRAC_PI_2).abs() < 1e-6);
    assert!((cs::yaw_towards(o, (0.0, -1.0)) - PI).abs() < 1e-6);
    assert!((cs::yaw_towards(o, (-1.0, 1.0)) - FRAC_PI_4).abs() < 1e-6);
    assert!((cs::yaw_towards(o, (1.0, 1.0)) + FRAC_PI_4).abs() < 1e-6);
    // Same point: no direction.
    assert_eq!(cs::yaw_towards((5.0, 5.0), (5.0, 5.0)), 0.0);
    // The result is relative to the start, not the origin.
    assert!((cs::yaw_towards((100.0, 50.0), (99.0, 50.0)) - FRAC_PI_2).abs() < 1e-5);
}

#[test]
fn the_direction_byte_matches_the_server_compass_values() {
    let o = (0.0, 0.0);
    let dir = |to| cs::yaw_to_direction(cs::yaw_towards(o, to));
    assert_eq!(dir((0.0, 1.0)), 0);
    assert_eq!(dir((-1.0, 0.0)), 32);
    assert_eq!(dir((1.0, 0.0)), -32);
    assert_eq!(dir((0.0, -1.0)), 64);
    assert_eq!(dir((-1.0, 1.0)), 16);
    assert_eq!(dir((1.0, 1.0)), -16);
    // One step is 2.8125 degrees; rounding, not truncation.
    let step = (2.0 * PI) / 128.0;
    assert_eq!(cs::yaw_to_direction(step * 0.49), 0);
    assert_eq!(cs::yaw_to_direction(step * 0.51), 1);
    assert_eq!(cs::yaw_to_direction(-step * 0.51), -1);
    // Angles outside (-pi, pi] and the whole circle fold into -42..=85 like the server's.
    assert_eq!(cs::yaw_to_direction(2.0 * PI), 0);
    assert_eq!(cs::yaw_to_direction(-PI), 64);
    assert_eq!(cs::yaw_to_direction(PI), 64);
    for i in -300..300 {
        let d = cs::yaw_to_direction(i as f32 * 0.037);
        assert!((-42..=85).contains(&d), "{i}: {d}");
    }
}

#[test]
fn facing_a_target_writes_the_direction_into_rotation_z() {
    let from = (10.0, 10.0);
    let m = MoveUnit {
        yaw: cs::yaw_towards(from, (9.0, 10.0)), // west
        ..base()
    };
    // Rotation z is the third of the three rotation bytes after the position and velocity.
    assert_eq!(cs::move_unit(&m).body[9 + 9 + 6 + 2] as i8, 32);
}

#[test]
fn gliding_is_an_air_update_in_the_glide_stance() {
    // The 1.2 gliding stance is 4 (`skills/glider.rs:71`); the flight is an airborne update
    // (`JumpPhase::Air`: no ground actor flag) with the descent in vel.z. The server reads and
    // relays the stance byte unchanged (`models/movement.rs:417`, `packets/c2g_world.rs:224`).
    let m = MoveUnit {
        pos: (100.0, 200.0, 80.0),
        vel: (9.0, 0.0, -3.0),
        moving: true,
        stance: cs::STANCE_GLIDE,
        jump: JumpPhase::Air,
        ..base()
    };
    check(
        &m,
        expected(
            0x02,
            (100.0, 200.0, 80.0),
            [wire(9.0), 0, wire(-3.0)],
            0,
            [0, 127, 0],
            4,
            0,
            0,
        ),
    );
}
