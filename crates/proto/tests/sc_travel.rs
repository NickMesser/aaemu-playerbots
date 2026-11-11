//! `sc_travel` parsers: the teleport book and recall districts, instance loads and boats.
//! Bodies are built in the order of the server writer cited per test (AAEmu-1.2-Rust
//! `rust/crates/aaemu-game/src`, abbreviated `G/`), independently of the parser under test.

mod sc_world_common;

use aa_proto::opcodes::sc;
use aa_proto::sc::{self as sc_mod, ServerEvent};
use aa_proto::sc_travel::{attach_point, attach_reason, MySlaveInfo, PortalEntry, ShipMotion};
use aa_proto::sc_world::{parse, parse_ship, WorldEvent};
use aa_proto::PacketStream;
use sc_world_common::{game, no_panic_on_truncation, truncation_is_rejected};

/// `PortalWire.write` (G/packets/world.rs:228).
fn write_portal(s: &mut PacketStream, id: u32, name: &str, zone: u32, x: f32, y: f32, z: f32) {
    s.write_u32(id)
        .write_str(name)
        .write_u32(zone)
        .write_f32(x)
        .write_f32(y)
        .write_f32(z)
        .write_f32(90.0); // z rot
}

fn entry(id: u32, name: &str, zone: u32, off_x: f32, off_y: f32, z: f32) -> PortalEntry {
    PortalEntry {
        id,
        name: name.into(),
        zone_id: zone,
        off_x,
        off_y,
        z,
        z_rot: 90.0,
    }
}

// ---- portals ---------------------------------------------------------------------------------

#[test]
fn portal_book() {
    // SCCharacterPortalsPacket, G/packets/world.rs:248: i32 count, PortalWire[count].
    let mut s = PacketStream::new();
    s.write_i32(2);
    write_portal(&mut s, 7, "Home", 6, 100.5, 200.25, 30.0);
    write_portal(&mut s, 9, "", 0, 1.0, 2.0, 3.0);
    let WorldEvent::PortalBook(list) = game(sc::SCCharacterPortalsPacket, &s) else {
        panic!("not a portal book");
    };
    assert_eq!(
        &*list,
        &[
            entry(7, "Home", 6, 100.5, 200.25, 30.0),
            entry(9, "", 0, 1.0, 2.0, 3.0)
        ]
    );
    // count, a 30-byte entry ("Home") and the smallest entry on the wire (26 bytes)
    assert_eq!(s.len(), 4 + 30 + 26);
    truncation_is_rejected(sc::SCCharacterPortalsPacket, s.as_bytes(), s.len());
}

#[test]
fn portal_book_with_a_single_new_entry_and_an_empty_list() {
    // G/systems/npcs/portal/mod.rs:486: a freshly recorded location arrives alone.
    let mut s = PacketStream::new();
    s.write_i32(1);
    write_portal(&mut s, 12, "TcSpot", 6, 0.5, 0.25, 99.0);
    let WorldEvent::PortalBook(list) = game(sc::SCCharacterPortalsPacket, &s) else {
        panic!("not a portal book");
    };
    assert_eq!(&*list, &[entry(12, "TcSpot", 6, 0.5, 0.25, 99.0)]);
    let mut s = PacketStream::new();
    s.write_i32(0);
    assert_eq!(
        game(sc::SCCharacterPortalsPacket, &s),
        WorldEvent::PortalBook(Box::default())
    );
}

#[test]
fn portal_counts_are_validated_before_allocating() {
    let mut s = PacketStream::new();
    s.write_i32(-1);
    assert!(parse(1, sc::SCCharacterPortalsPacket, s.as_bytes()).is_err());
    let mut s = PacketStream::new();
    s.write_i32(i32::MAX);
    assert!(parse(1, sc::SCCharacterPortalsPacket, s.as_bytes()).is_err());
    // two entries announced, one present
    let mut s = PacketStream::new();
    s.write_i32(2);
    write_portal(&mut s, 1, "A", 0, 0.0, 0.0, 0.0);
    assert!(parse(1, sc::SCCharacterReturnDistrictsPacket, s.as_bytes()).is_err());
}

#[test]
fn return_districts_carry_the_bound_point() {
    // SCCharacterReturnDistrictsPacket, G/packets/world.rs:268: count, PortalWire[], u32 bound.
    let mut s = PacketStream::new();
    s.write_i32(2);
    write_portal(&mut s, 1, "Harpa", 15, 10.0, 20.0, 30.0);
    write_portal(&mut s, 2, "Marianople", 16, 11.0, 21.0, 31.0);
    s.write_u32(2);
    let WorldEvent::ReturnDistricts { portals, bound } =
        game(sc::SCCharacterReturnDistrictsPacket, &s)
    else {
        panic!("not return districts");
    };
    assert_eq!(bound, 2);
    assert_eq!(portals.len(), 2);
    assert_eq!(portals[0], entry(1, "Harpa", 15, 10.0, 20.0, 30.0));
    assert_eq!(&*portals[1].name, "Marianople");
    // The bound point is the entry after the list: a body without it is rejected.
    truncation_is_rejected(sc::SCCharacterReturnDistrictsPacket, s.as_bytes(), s.len());
}

#[test]
fn world_xy_adds_the_zone_origin_cell() {
    // G/systems/npcs/portal/mod.rs:343: the wire x is `world x - origin x * 1024`.
    let e = entry(1, "A", 6, 100.5, -24.25, 0.0);
    assert_eq!(
        e.world_xy(13.0, 15.0),
        (100.5 + 13.0 * 1024.0, -24.25 + 15.0 * 1024.0)
    );
    assert_eq!(e.world_xy(0.0, 0.0), (100.5, -24.25));
}

#[test]
fn portal_saved_deleted_and_used() {
    // SCPortalInfoSavedPacket, G/systems/npcs/portal/packets.rs:297: one PortalWire.
    let mut s = PacketStream::new();
    write_portal(&mut s, 7, "Barn", 6, 5.0, 6.0, 7.0);
    assert_eq!(
        game(sc::SCPortalInfoSavedPacket, &s),
        WorldEvent::PortalSaved(Box::new(entry(7, "Barn", 6, 5.0, 6.0, 7.0)))
    );
    truncation_is_rejected(sc::SCPortalInfoSavedPacket, s.as_bytes(), s.len());
    // SCPortalDeletedPacket, :281: u8 type, i32 id.
    let mut s = PacketStream::new();
    s.write_u8(2).write_i32(0x1001);
    assert_eq!(
        game(sc::SCPortalDeletedPacket, &s),
        WorldEvent::PortalDeleted {
            kind: 2,
            id: 0x1001
        }
    );
    truncation_is_rejected(sc::SCPortalDeletedPacket, s.as_bytes(), 5);
    // SCUnitPortalUsedPacket, :313: bc gate (the server test body is 0a0b0c).
    let mut s = PacketStream::new();
    s.write_bc(0x0A_0B0C);
    assert_eq!(
        game(sc::SCUnitPortalUsedPacket, &s),
        WorldEvent::PortalUsed { gate: 0x0A_0B0C }
    );
    truncation_is_rejected(sc::SCUnitPortalUsedPacket, s.as_bytes(), 3);
}

// ---- instances -------------------------------------------------------------------------------

#[test]
fn processing_instance() {
    // SCProcessingInstancePacket, G/systems/instances/packets.rs:397: i32 zone key.
    let mut s = PacketStream::new();
    s.write_i32(236);
    assert_eq!(
        game(sc::SCProcessingInstancePacket, &s),
        WorldEvent::ProcessingInstance { zone_id: 236 }
    );
    truncation_is_rejected(sc::SCProcessingInstancePacket, s.as_bytes(), 4);
}

#[test]
fn load_instance_exposes_id_zone_position_and_all_three_angles() {
    // SCLoadInstancePacket, G/systems/skills/packets_special.rs:155: u32 instance, u32 zone key,
    // f32 x, y, z, f32 ang x, y, z (radians).
    let mut s = PacketStream::new();
    s.write_u32(0x1234)
        .write_u32(236)
        .write_f32(1500.5)
        .write_f32(-20.25)
        .write_f32(98.0)
        .write_f32(0.0)
        .write_f32(0.0)
        .write_f32(-1.5);
    assert_eq!(
        game(sc::SCLoadInstancePacket, &s),
        WorldEvent::LoadInstance {
            instance_id: 0x1234,
            zone_id: 236,
            pos: (1500.5, -20.25, 98.0),
            yaw: -1.5,
            ang_x: 0.0,
            ang_y: 0.0,
        }
    );
    truncation_is_rejected(sc::SCLoadInstancePacket, s.as_bytes(), s.len());
}

// ---- slaves ----------------------------------------------------------------------------------

#[test]
fn slave_created() {
    // SCSlaveCreatedPacket, G/systems/vehicles/packets.rs:107: bc owner, u16 tl, bc slave,
    // bool hide, i64 unk, str creator.
    let mut s = PacketStream::new();
    s.write_bc(0x400)
        .write_u16(321)
        .write_bc(0x500)
        .write_bool(true)
        .write_i64(0)
        .write_str("Captain");
    assert_eq!(
        game(sc::SCSlaveCreatedPacket, &s),
        WorldEvent::SlaveCreated {
            owner_obj: 0x400,
            tl: 321,
            slave_obj: 0x500,
            hide_effect: true,
            creator: "Captain".into(),
        }
    );
    truncation_is_rejected(sc::SCSlaveCreatedPacket, s.as_bytes(), s.len());
}

#[test]
fn my_slave_uses_fixed_point_coordinates_and_unscaled_hit_points() {
    // SCMySlavePacket, G/systems/vehicles/packets.rs:147: bc obj, u16 tl, str name,
    // u32 template, i32 hp, i32 max hp, fx64 x, fx64 y, f32 z.
    let mut s = PacketStream::new();
    s.write_bc(0x500)
        .write_u16(321)
        .write_str("Clipper")
        .write_u32(63)
        .write_i32(4_000)
        .write_i32(5_000)
        .write_i64((10_000 * 4096) << 32)
        .write_i64((-2_000 * 4096) << 32)
        .write_f32(99.5);
    assert_eq!(
        game(sc::SCMySlavePacket, &s),
        WorldEvent::MySlave(Box::new(MySlaveInfo {
            obj: 0x500,
            tl: 321,
            name: "Clipper".into(),
            template: 63,
            hp: 4_000,
            max_hp: 5_000,
            pos: (10_000.0, -2_000.0, 99.5),
        }))
    );
    truncation_is_rejected(sc::SCMySlavePacket, s.as_bytes(), s.len());
}

#[test]
fn slave_bound_attached_detached_despawned_and_removed() {
    // SCSlaveBoundPacket, G/systems/vehicles/packets.rs:140: u32 master (character id),
    // bc slave.
    let mut s = PacketStream::new();
    s.write_u32(1001).write_bc(0x500);
    assert_eq!(
        game(sc::SCSlaveBoundPacket, &s),
        WorldEvent::SlaveBound {
            master_id: 1001,
            slave_obj: 0x500
        }
    );
    truncation_is_rejected(sc::SCSlaveBoundPacket, s.as_bytes(), 7);
    // SCUnitAttachedPacket, G/packets/world_objects.rs:239: bc child, u8 point, bc parent,
    // u8 reason. The server test pins frame 0x076 with [2,0,0, 1, 3,0,0, 7].
    let mut s = PacketStream::new();
    s.write_raw(&[2, 0, 0, 1, 3, 0, 0, 7]);
    assert_eq!(
        game(sc::SCUnitAttachedPacket, &s),
        WorldEvent::UnitAttached {
            child: 2,
            point: attach_point::DRIVER,
            parent: 3,
            reason: 7
        }
    );
    truncation_is_rejected(sc::SCUnitAttachedPacket, s.as_bytes(), 8);
    // SCUnitDetachedPacket, G/systems/vehicles/packets.rs:97: bc obj, u8 reason.
    let mut s = PacketStream::new();
    s.write_bc(0x400).write_u8(attach_reason::SLAVE_BINDING);
    assert_eq!(
        game(sc::SCUnitDetachedPacket, &s),
        WorldEvent::UnitDetached {
            obj: 0x400,
            reason: 5
        }
    );
    truncation_is_rejected(sc::SCUnitDetachedPacket, s.as_bytes(), 4);
    // SCSlaveDespawnPacket, :133: bc obj.
    let mut s = PacketStream::new();
    s.write_bc(0x500);
    assert_eq!(
        game(sc::SCSlaveDespawnPacket, &s),
        WorldEvent::SlaveDespawned { obj: 0x500 }
    );
    truncation_is_rejected(sc::SCSlaveDespawnPacket, s.as_bytes(), 3);
    // SCSlaveRemovedPacket, :126: bc obj, u16 tl.
    let mut s = PacketStream::new();
    s.write_bc(0x400).write_u16(321);
    assert_eq!(
        game(sc::SCSlaveRemovedPacket, &s),
        WorldEvent::SlaveRemoved {
            obj: 0x400,
            tl: 321
        }
    );
    truncation_is_rejected(sc::SCSlaveRemovedPacket, s.as_bytes(), 5);
}

// ---- ship movement ---------------------------------------------------------------------------

/// Quaternion shorts of a yaw-only rotation, as the server encodes them
/// (G/systems/ai/physics/replication.rs:186: `q / 0.00003052`, `w >= 0`).
fn yaw_shorts(yaw: f32) -> [i16; 3] {
    let (s, c) = (yaw * 0.5).sin_cos();
    let sign = if c < 0.0 { -1.0 } else { 1.0 };
    [0, 0, ((sign * s) / 0.000_030_52).round() as i16]
}

/// `SCOneUnitMovementPacket` of a `ShipMove` (G/models/movement.rs:715 + `MoveType::write` +
/// `ShipMove::write` :589): bc obj, u8 3, u32 time, u8 flags, [u32 sc type, u8 phase], position,
/// i16 vel x3, i16 rot x3, f32 ang vel x3, i8 steering, i8 throttle, u16 zone, u8 stuck.
#[allow(clippy::too_many_arguments)]
fn ship_move(
    obj: u32,
    flags: u8,
    pos: (f32, f32, f32),
    vel: [i16; 3],
    rot: [i16; 3],
    steering: i8,
    throttle: i8,
    zone: u16,
) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_bc(obj)
        .write_u8(3)
        .write_u32(123_456)
        .write_u8(flags);
    if flags & 0x10 != 0 {
        s.write_u32(9).write_u8(1);
    }
    s.write_position(pos.0, pos.1, pos.2);
    for v in vel {
        s.write_i16(v);
    }
    for r in rot {
        s.write_i16(r);
    }
    s.write_f32(0.0)
        .write_f32(0.25)
        .write_f32(-0.5)
        .write_i8(steering)
        .write_i8(throttle)
        .write_u16(zone)
        .write_u8(0);
    s
}

fn ship(s: &PacketStream) -> ShipMotion {
    let ev = parse_ship(1, sc::SCOneUnitMovementPacket, s.as_bytes())
        .expect("body parses")
        .expect("a ship move");
    let WorldEvent::ShipMoved(m) = ev else {
        panic!("not a ship move: {ev:?}");
    };
    *m
}

#[test]
fn ship_move_carries_heading_speed_and_helm() {
    let s = ship_move(
        0x0500,
        0,
        (10_000.5, 9_500.25, 100.0),
        [0, 1092, 0],
        yaw_shorts(0.7),
        -20,
        60,
        0x0101,
    );
    let m = ship(&s);
    assert_eq!(m.obj, 0x0500);
    assert_eq!(m.time, 123_456);
    assert!((m.pos.0 - 10_000.5).abs() < 0.01 && (m.pos.1 - 9_500.25).abs() < 0.01);
    assert!((m.pos.2 - 100.0).abs() < 0.01);
    // 1092 shorts = 1 m/s north (G/systems/ai/physics/replication.rs:281 test `ship_velocity_shorts_use_the_client_scale`)
    assert!((m.velocity[1] - 1.0).abs() < 1e-3, "{:?}", m.velocity);
    assert!(m.velocity[0].abs() < 1e-6);
    assert!((m.speed - 1.0).abs() < 1e-3);
    assert!((m.yaw - 0.7).abs() < 2e-3, "yaw {}", m.yaw);
    assert_eq!(m.ang_vel, [0.0, 0.25, -0.5]);
    assert_eq!(
        (m.steering, m.throttle, m.zone, m.stuck),
        (-20, 60, 0x0101, false)
    );
    // The same packet still parses to the unit move existing consumers track.
    let Some(ServerEvent::World(WorldEvent::UnitMoved { obj, kind, pos, .. })) =
        sc_mod::parse(1, sc::SCOneUnitMovementPacket, s.as_bytes()).unwrap()
    else {
        panic!("ship moves must keep parsing to UnitMoved");
    };
    assert_eq!((obj, kind), (0x0500, 3));
    assert!((pos.0 - 10_000.5).abs() < 0.01);
    // and `sc::parse_ship` wraps the same event
    let Some(ServerEvent::World(WorldEvent::ShipMoved(w))) =
        sc_mod::parse_ship(1, sc::SCOneUnitMovementPacket, s.as_bytes()).unwrap()
    else {
        panic!("sc::parse_ship must return the ship move");
    };
    assert_eq!(*w, m);
    no_panic_on_truncation(sc::SCOneUnitMovementPacket, s.as_bytes());
    for end in 0..s.len() {
        let prefix = &s.as_bytes()[..end];
        assert!(
            parse_ship(1, sc::SCOneUnitMovementPacket, prefix).is_err(),
            "prefix of {end} bytes parsed"
        );
    }
}

#[test]
fn ship_yaw_uses_the_bot_convention_for_every_heading() {
    // yaw 0 = bow toward +Y, positive counter-clockwise (G/systems/ai/physics/ship.rs:239
    // `bow`, cs::MoveUnit docs). Includes headings near a half turn, where `w` is tiny.
    for yaw in [
        0.0f32,
        0.7,
        -0.7,
        std::f32::consts::FRAC_PI_2,
        -std::f32::consts::FRAC_PI_2,
        2.5,
        -2.5,
        3.1,
        -3.1,
        3.13,
    ] {
        let m = ship(&ship_move(
            1,
            0,
            (1.0, 2.0, 3.0),
            [0; 3],
            yaw_shorts(yaw),
            0,
            0,
            0,
        ));
        let mut diff = (m.yaw - yaw).rem_euclid(std::f32::consts::TAU);
        if diff > std::f32::consts::PI {
            diff -= std::f32::consts::TAU;
        }
        // 16-bit quaternion shorts leave `w` coarse near a half turn (about 0.7 degrees there).
        let tolerance = if yaw.abs() > 3.0 { 0.02 } else { 3e-3 };
        assert!(
            diff.abs() < tolerance,
            "yaw {yaw}: parsed {} (diff {diff})",
            m.yaw
        );
    }
}

#[test]
fn forward_speed_follows_the_bow_not_the_axes() {
    // Bow toward -X (yaw +pi/2) moving toward -X at 3 m/s: ahead; toward +X: astern.
    let yaw = std::f32::consts::FRAC_PI_2;
    let mut m = ship(&ship_move(
        1,
        0,
        (0.0, 0.0, 0.0),
        [0; 3],
        yaw_shorts(yaw),
        0,
        0,
        0,
    ));
    m.velocity = [-3.0, 0.0, 0.0];
    assert!((m.forward_speed() - 3.0).abs() < 1e-2);
    m.velocity = [3.0, 0.0, 0.0];
    assert!((m.forward_speed() + 3.0).abs() < 1e-2);
    // A velocity short is 30/32767 m/s: the maximum is about 30 m/s.
    let m = ship(&ship_move(
        1,
        0,
        (0.0, 0.0, 0.0),
        [3, 4, 0],
        yaw_shorts(0.0),
        0,
        0,
        0,
    ));
    assert!((m.speed - 5.0 * 30.0 / 32767.0).abs() < 1e-6);
}

#[test]
fn ship_move_with_an_sc_type_block_shifts_the_position() {
    // MoveTypeFlags.HAS_SC_TYPE_AND_PHASE (0x10): u32 + u8 before the position.
    let s = ship_move(
        0x0501,
        0x12,
        (2_000.0, 3_000.0, 99.0),
        [0; 3],
        yaw_shorts(0.0),
        0,
        0,
        0,
    );
    let m = ship(&s);
    assert!((m.pos.0 - 2_000.0).abs() < 0.01 && (m.pos.1 - 3_000.0).abs() < 0.01);
    assert_eq!(m.obj, 0x0501);
}

#[test]
fn parse_ship_ignores_other_moves_and_packets_without_reading_them() {
    // other move types of SCOneUnitMovement
    for kind in [0u8, 1, 2, 4, 5, 9] {
        let mut s = PacketStream::new();
        s.write_bc(1).write_u8(kind).write_raw(&[0; 60]);
        assert_eq!(
            parse_ship(1, sc::SCOneUnitMovementPacket, s.as_bytes()).unwrap(),
            None,
            "move type {kind}"
        );
    }
    // other opcodes and levels are not even looked at
    assert_eq!(parse_ship(1, sc::SCUnitStatePacket, &[]).unwrap(), None);
    assert_eq!(
        parse_ship(2, sc::SCOneUnitMovementPacket, &[]).unwrap(),
        None
    );
    assert_eq!(
        sc_mod::parse_ship(1, sc::SCTimeOfDayPacket, &[])
            .unwrap()
            .map(|_| ()),
        None
    );
}

#[test]
fn unit_movement_batches_are_not_parsed() {
    // SCUnitMovementsPacket (0x06b) carries no move type per entry (G/models/movement.rs:726),
    // so a ship cannot be recognised in it: neither parser handles it.
    let mut s = PacketStream::new();
    s.write_u16(1).write_bc(5).write_raw(&[0; 60]);
    assert_eq!(
        parse(1, sc::SCUnitMovementsPacket, s.as_bytes()).unwrap(),
        None
    );
    assert_eq!(
        parse_ship(1, sc::SCUnitMovementsPacket, s.as_bytes()).unwrap(),
        None
    );
}

#[test]
fn mate_spawned_has_ten_skill_slots() {
    // SCMateSpawnedPacket writer, G/systems/vehicles/packets.rs:579-592: tl u16, id u32,
    // item id u64, user state u8, experience i32, mileage i32, spawn delay u32, then the skills
    // and 10 - skills zero i32 slots.
    let mut s = PacketStream::new();
    s.write_u16(0x0102)
        .write_u32(7)
        .write_u64(0x1122_3344_5566_7788)
        .write_u8(1)
        .write_i32(300)
        .write_i32(5)
        .write_u32(0)
        .write_u32(11)
        .write_u32(12);
    for _ in 0..8 {
        s.write_i32(0);
    }
    let WorldEvent::MateSpawned(m) = game(sc::SCMateSpawnedPacket, &s) else {
        panic!("not a mate spawn");
    };
    assert_eq!(m.tl, 0x0102);
    assert_eq!(m.id, 7);
    assert_eq!(m.item_id, 0x1122_3344_5566_7788);
    assert_eq!((m.user_state, m.experience, m.mileage), (1, 300, 5));
    assert_eq!(m.spawn_delay_ms, 0);
    assert_eq!(&*m.skills, &[11, 12, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(s.len(), 2 + 4 + 8 + 1 + 4 + 4 + 4 + 10 * 4);
    truncation_is_rejected(sc::SCMateSpawnedPacket, s.as_bytes(), s.len());
}
