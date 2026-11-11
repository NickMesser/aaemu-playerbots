//! Byte-exact tests of the travel `cs` builders: skill objects, the teleport book, instances
//! and boats.
//!
//! Every expected body is written out by hand in the order the server reads it (the `file:line`
//! of each read is in the builder's doc comment, paths below
//! `AAEmu-1.2-Rust/rust/crates/aaemu-game/src/`), not produced by the builder.

use aa_proto::cs::{self, Out, SkillObject};
use aa_proto::game::LEVEL_GAME;
use aa_proto::opcodes::cs as op;

#[track_caller]
fn game(out: Out, opcode: u16, body: &[u8]) {
    assert_eq!(out.level, LEVEL_GAME, "level of {}", out.name());
    assert_eq!(out.opcode, opcode, "opcode of {}", out.name());
    assert_eq!(out.body, body, "body of {}", out.name());
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// `fx64` of 1.0 / -2.0: `(v * 4096) << 32` as a little-endian `i64`.
const X_1: [u8; 8] = [0, 0, 0, 0, 0, 0x10, 0, 0];
const Y_M2: [u8; 8] = [0, 0, 0, 0, 0, 0xE0, 0xFF, 0xFF];
/// 3.5f32.
const F_3_5: [u8; 4] = [0, 0, 0x60, 0x40];

/// `CSStartSkill` header: skill `0x01020304` (little endian) and a unit caster / unit target
/// (bc 0x0A0B0C and 0x111213), as `start_skill_unit` writes them.
const HEAD_UNIT: [u8; 12] = [
    0x04, 0x03, 0x02, 0x01, // skill
    0x00, 0x0C, 0x0B, 0x0A, // caster unit
    0x00, 0x13, 0x12, 0x11, // target unit
];

// ---- skill objects --------------------------------------------------------------------------

#[test]
fn flag_is_the_object_type() {
    // systems/skills/types.rs:371 `skill_object_type`
    let flags = [
        (SkillObject::None, 0),
        (
            SkillObject::PortalInfo {
                kind: 0,
                id: 0,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            1,
        ),
        (
            SkillObject::SavePortalInfo {
                id: 0,
                name: String::new(),
            },
            2,
        ),
        (SkillObject::Text(String::new()), 3),
        (
            SkillObject::Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            4,
        ),
        (SkillObject::Unk5(0), 5),
        (SkillObject::Unk6(String::new()), 6),
        (
            SkillObject::ItemGradeEnchantingSupport {
                id: 0,
                support_item_id: 0,
                auto_use_aa_point: false,
            },
            7,
        ),
    ];
    for (object, flag) in flags {
        assert_eq!(object.flag(), flag, "{object:?}");
    }
    assert_eq!(SkillObject::default(), SkillObject::None);
}

#[test]
fn start_skill_unit_with_each_skill_object() {
    // SkillObject::read, systems/skills/types.rs:417-446.
    let cases: Vec<(SkillObject, Vec<u8>)> = vec![
        (SkillObject::None, vec![0]),
        (
            SkillObject::PortalInfo {
                kind: 2,
                id: -3,
                x: 1.0,
                y: -2.0,
                z: 3.5,
            },
            cat(&[&[1, 2], &[0xFD, 0xFF, 0xFF, 0xFF], &X_1, &Y_M2, &F_3_5]),
        ),
        (
            SkillObject::SavePortalInfo {
                id: -2,
                name: "Home".into(),
            },
            cat(&[&[2], &[0xFE, 0xFF, 0xFF, 0xFF], &[4, 0], b"Home"]),
        ),
        (SkillObject::Text("Hi".into()), cat(&[&[3], &[2, 0], b"Hi"])),
        (
            SkillObject::Position {
                x: 1.0,
                y: -2.0,
                z: 3.5,
            },
            cat(&[&[4], &X_1, &Y_M2, &F_3_5]),
        ),
        (SkillObject::Unk5(-1), vec![5, 0xFF, 0xFF, 0xFF, 0xFF]),
        (SkillObject::Unk6("x".into()), cat(&[&[6], &[1, 0], b"x"])),
        (
            SkillObject::ItemGradeEnchantingSupport {
                id: 0x0102_0304,
                support_item_id: 0x1122_3344_5566_7788,
                auto_use_aa_point: true,
            },
            cat(&[
                &[7],
                &[0x04, 0x03, 0x02, 0x01],
                &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
                &[1],
            ]),
        ),
    ];
    for (object, tail) in cases {
        game(
            cs::start_skill_unit_with(0x0102_0304, 0x0A_0B0C, 0x11_1213, &object),
            op::CSStartSkillPacket,
            &cat(&[&HEAD_UNIT, &tail]),
        );
    }
}

#[test]
fn the_plain_builders_still_send_no_skill_object() {
    // The new `_with` variants are the general form; the old names keep flag 0.
    assert_eq!(
        cs::start_skill_unit(1, 2, 3),
        cs::start_skill_unit_with(1, 2, 3, &SkillObject::None)
    );
    assert_eq!(
        cs::start_skill_doodad(1, 2, 3),
        cs::start_skill_doodad_with(1, 2, 3, &SkillObject::None)
    );
    assert_eq!(
        cs::start_skill_position(1, 2, (1.0, 2.0, 3.0), 0.5),
        cs::start_skill_position_with(1, 2, (1.0, 2.0, 3.0), 0.5, &SkillObject::None)
    );
    assert_eq!(
        cs::start_skill_item(1, 2, 3, 4, 5),
        cs::start_skill_item_with(1, 2, 3, 4, 5, &SkillObject::None)
    );
}

#[test]
fn start_skill_doodad_and_position_with_an_object() {
    // target type 4 (doodad) then flag 3 (text)
    game(
        cs::start_skill_doodad_with(
            0x0102_0304,
            0x0A_0B0C,
            0x11_1213,
            &SkillObject::Text("Hi".into()),
        ),
        op::CSStartSkillPacket,
        &cat(&[
            &[0x04, 0x03, 0x02, 0x01],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[0x04, 0x13, 0x12, 0x11],
            &[3, 2, 0],
            b"Hi",
        ]),
    );
    // target type 1 (position: fx64 x, fx64 y, f32 z, f32 rot, bc, bc) then flag 5
    game(
        cs::start_skill_position_with(
            0x0102_0304,
            0x0A_0B0C,
            (1.0, -2.0, 3.5),
            2.0,
            &SkillObject::Unk5(7),
        ),
        op::CSStartSkillPacket,
        &cat(&[
            &[0x04, 0x03, 0x02, 0x01],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[0x01],
            &X_1,
            &Y_M2,
            &F_3_5,
            &[0, 0, 0, 0x40],
            &[0, 0, 0, 0, 0, 0],
            &[5, 7, 0, 0, 0],
        ]),
    );
}

#[test]
fn start_skill_item_with_an_object() {
    // item caster (type 2: bc, u64 item, u32 template, u8, u32), unit target, flag 3
    game(
        cs::start_skill_item_with(
            0x0102_0304,
            0x0A_0B0C,
            0x1122_3344_5566_7788,
            0x99AA_BBCC,
            0x11_1213,
            &SkillObject::Text("Hi".into()),
        ),
        op::CSStartSkillPacket,
        &cat(&[
            &[0x04, 0x03, 0x02, 0x01],
            &[0x02, 0x0C, 0x0B, 0x0A],
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
            &[0xCC, 0xBB, 0xAA, 0x99],
            &[0],
            &[0, 0, 0, 0],
            &[0x00, 0x13, 0x12, 0x11],
            &[3, 2, 0],
            b"Hi",
        ]),
    );
}

// ---- teleport book ---------------------------------------------------------------------------

#[test]
fn skill_and_item_ids() {
    // 11215 = 0x2BCF, 16841 = 0x41C9, 11216 = 0x2BD0, 13863 = 0x3627, 4045 = 0x0FCD.
    assert_eq!(cs::SKILL_RECORD_PORTAL.to_le_bytes(), [0xCF, 0x2B, 0, 0]);
    assert_eq!(cs::SKILL_RENAME_PORTAL.to_le_bytes(), [0xC9, 0x41, 0, 0]);
    assert_eq!(cs::SKILL_OPEN_PORTAL.to_le_bytes(), [0xD0, 0x2B, 0, 0]);
    assert_eq!(cs::SKILL_PASSENGER_SEAT.to_le_bytes(), [0x27, 0x36, 0, 0]);
    assert_eq!(cs::ITEM_TELEPORT_BOOK.to_le_bytes(), [0xCD, 0x0F, 0, 0]);
    assert_eq!((cs::PORTAL_KIND_DISTRICT, cs::PORTAL_KIND_PRIVATE), (1, 2));
}

#[test]
fn record_portal_casts_11215_on_self_with_a_new_entry() {
    // CSStartSkill: skill, unit caster me, unit target me, flag 2, SavePortalInfo(id 0, name).
    game(
        cs::record_portal(0x0A_0B0C, "Home"),
        op::CSStartSkillPacket,
        &cat(&[
            &[0xCF, 0x2B, 0, 0],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[2],
            &[0, 0, 0, 0],
            &[4, 0],
            b"Home",
        ]),
    );
}

#[test]
fn rename_portal_casts_16841_with_the_entry_id() {
    game(
        cs::rename_portal(0x0A_0B0C, 0x1001, "Barn"),
        op::CSStartSkillPacket,
        &cat(&[
            &[0xC9, 0x41, 0, 0],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[2],
            &[0x01, 0x10, 0, 0],
            &[4, 0],
            b"Barn",
        ]),
    );
}

#[test]
fn open_portal_casts_11216_from_the_book_with_a_portal_info_object() {
    // aaemu-testclient/src/scenario/portal.rs:271-290 sends the same layout.
    game(
        cs::open_portal(
            0x0A_0B0C,
            0x1122_3344_5566_7788,
            cs::PORTAL_KIND_PRIVATE,
            5,
            (1.0, -2.0, 3.5),
        ),
        op::CSStartSkillPacket,
        &cat(&[
            &[0xD0, 0x2B, 0, 0],
            &[0x02, 0x0C, 0x0B, 0x0A],
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
            &[0xCD, 0x0F, 0, 0],
            &[0],
            &[0, 0, 0, 0],
            &[0x00, 0x0C, 0x0B, 0x0A],
            &[1, 2],
            &[5, 0, 0, 0],
            &X_1,
            &Y_M2,
            &F_3_5,
        ]),
    );
}

#[test]
fn use_portal() {
    // CSUsePortalPacket::read, systems/npcs/portal/packets.rs:29: bc, bool.
    game(
        cs::use_portal(0x0A_0B0C, true),
        op::CSUsePortalPacket,
        &[0x0C, 0x0B, 0x0A, 1],
    );
    game(
        cs::use_portal(0x0A_0B0C, false),
        op::CSUsePortalPacket,
        &[0x0C, 0x0B, 0x0A, 0],
    );
}

#[test]
fn delete_portal() {
    // CSDeletePortalPacket::read, packets.rs:67: u8, u32.
    game(
        cs::delete_portal(cs::PORTAL_KIND_DISTRICT, 0x0102_0304),
        op::CSDeletePortalPacket,
        &[1, 0x04, 0x03, 0x02, 0x01],
    );
    game(
        cs::delete_portal(cs::PORTAL_KIND_PRIVATE, 0x1001),
        op::CSDeletePortalPacket,
        &[2, 0x01, 0x10, 0, 0],
    );
}

#[test]
fn notify_sub_zone() {
    // CSNotifySubZonePacket::read, systems/npcs/subzone.rs:420: u32.
    game(
        cs::notify_sub_zone(0x0102_0304),
        op::CSNotifySubZonePacket,
        &[0x04, 0x03, 0x02, 0x01],
    );
}

// ---- boats -----------------------------------------------------------------------------------

#[test]
fn slave_requests_are_a_timeline_id_or_an_object_id() {
    // systems/vehicles/packets.rs:289 (bind), :337 (discard), :325 (destroy): u16 tl;
    // :313 (despawn): bc slave object.
    game(cs::bind_slave(0x0102), op::CSBindSlavePacket, &[0x02, 0x01]);
    game(
        cs::discard_slave(0x0102),
        op::CSDiscardSlavePacket,
        &[0x02, 0x01],
    );
    game(
        cs::destroy_slave(0x0102),
        op::CSDestroySlavePacket,
        &[0x02, 0x01],
    );
    game(
        cs::despawn_slave(0x0A_0B0C),
        op::CSDespawnSlavePacket,
        &[0x0C, 0x0B, 0x0A],
    );
}

#[test]
fn take_passenger_seat_casts_13863_on_the_slave() {
    // CSStartSkill: skill, unit caster me (bc 1), unit target slave (bc 0x200), flag 0.
    game(
        cs::take_passenger_seat(1, 0x0200),
        op::CSStartSkillPacket,
        &[
            0x27, 0x36, 0, 0, // 13863
            0x00, 0x01, 0x00, 0x00, // caster unit 1
            0x00, 0x00, 0x02, 0x00, // target unit 0x200
            0x00, // flag
        ],
    );
}

#[test]
fn ship_request_is_a_type_4_move_with_throttle_before_steering() {
    // CSMoveUnitPacket::read (packets/c2g_world.rs:227): bc obj, u8 type; MoveType::read
    // (models/movement.rs:261): u32 time, u8 flags (bit 0x10 would add u32 + u8); then
    // ShipRequestMove::read (:617): i8 throttle, i8 steering.
    game(
        cs::ship_request(0x0A_0B0C, 0x0102_0304, 100, -50),
        op::CSMoveUnitPacket,
        &[
            0x0C, 0x0B, 0x0A, // ship
            0x04, // MoveTypeEnum.ShipRequest
            0x04, 0x03, 0x02, 0x01, // time
            0x00, // flags
            0x64, // throttle 100
            0xCE, // steering -50
        ],
    );
    game(
        cs::ship_request(1, 0, -127, 127),
        op::CSMoveUnitPacket,
        &[1, 0, 0, 4, 0, 0, 0, 0, 0, 0x81, 0x7F],
    );
}

// ---- mates (mounts) --------------------------------------------------------------------------

#[test]
fn mate_requests_name_a_timeline_id_and_a_seat() {
    // systems/vehicles/packets.rs:222 (mount), :233 (unmount): u16 tl, u8 ap, u8 reason;
    // :277 (remove mate): u16 tl. The reason is the client's AttachUnitReason.MountMateForward.
    game(
        cs::mount_mate(0x0102, 1, cs::MOUNT_REASON_FORWARD),
        op::CSMountMatePacket,
        &[0x02, 0x01, 0x01, 0x04],
    );
    game(
        cs::unmount_mate(0x0102, 1, cs::MOUNT_REASON_FORWARD),
        op::CSUnMountMatePacket,
        &[0x02, 0x01, 0x01, 0x04],
    );
    game(
        cs::remove_mate(0x0102),
        op::CSRemoveMatePacket,
        &[0x02, 0x01],
    );
}
