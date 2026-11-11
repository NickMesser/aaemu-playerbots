//! `sc_world` parsers: units, movement, deaths, doodads. Bodies are built in the order of the
//! server writers cited per test (AAEmu-1.2-Rust `rust/crates/aaemu-game/src`).

mod sc_world_common;

use aa_proto::opcodes::sc;
use aa_proto::sc_world::{
    movement_direction_radians, parse, unit_state_yaw_radians, unit_type, BuffInfo,
    CommonFarmEntry, DoodadInfo, UnitInfo, WorldEvent,
};
use aa_proto::PacketStream;
use sc_world_common::{game, no_panic_on_truncation, write_item, write_model_params};

// ---- SCUnitStatePacket builders (world/unit_state.rs) --------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    Full,
    Slave,
    Npc,
    BodyOnly,
}

#[derive(Clone, Copy)]
struct EquipItem {
    slot: usize,
    template: u32,
    id: u64,
    grade: u8,
    detail: u8,
}

/// `write_inventory_equip` (world/unit_state.rs:330) and the character loop of
/// `write_unit_state` (:121).
fn write_equipment(s: &mut PacketStream, layout: Layout, items: &[EquipItem]) {
    for index in 0..28usize {
        let item = items.iter().find(|i| i.slot == index);
        if (19..=25).contains(&index) && layout != Layout::Slave {
            s.write_u32(item.map_or(0, |i| i.template));
            continue;
        }
        if index == 27 {
            if layout == Layout::BodyOnly {
                return;
            }
            match item {
                Some(i) => write_item(s, i.template, i.id, i.grade, 1, i.detail),
                None => {
                    s.write_i32(0);
                }
            }
            continue;
        }
        match (layout, item) {
            (Layout::BodyOnly, _) => {}
            (Layout::Full | Layout::Slave, Some(i)) => {
                write_item(s, i.template, i.id, i.grade, 1, i.detail)
            }
            (Layout::Npc, Some(i)) => {
                s.write_u32(i.template).write_u64(i.id).write_u8(i.grade);
            }
            (_, None) => {
                s.write_i32(0);
            }
        }
    }
}

/// `Buff.write_net` (models/buff.rs:146); `item_caster` selects the longer caster form.
fn write_net_buff(s: &mut PacketStream, index: u32, buff_id: u32, item_caster: bool, charge: i32) {
    s.write_u32(index).write_u32(buff_id);
    if item_caster {
        s.write_u8(2)
            .write_bc(0x77)
            .write_u64(5)
            .write_u32(6)
            .write_u8(7)
            .write_u32(8);
    } else {
        s.write_u8(0).write_bc(0x77);
    }
    s.write_u32(42) // caster id
        .write_u8(30) // caster level
        .write_i16(2) // ability level
        .write_i32(60_000) // duration
        .write_u32(1_000) // elapsed
        .write_u32(500) // tick
        .write_i32(0) // tick index
        .write_i32(1) // stack
        .write_i32(charge)
        .write_u32(0); // cooldown skill
}

type BuffSpec = (u32, u32, bool, i32);

/// The three buff lists closing `SCUnitStatePacket` (good, bad, hidden).
fn write_buff_lists(s: &mut PacketStream, lists: [&[BuffSpec]; 3]) {
    for list in lists {
        s.write_u8(list.len() as u8);
        for &(index, id, item, charge) in list {
            write_net_buff(s, index, id, item, charge);
        }
    }
}

/// `Unit.ModelPosture` (world/unit_state.rs:375).
fn write_posture(s: &mut PacketStream, posture: u8, has_loot: bool) {
    s.write_u8(posture).write_bool(has_loot);
    match posture {
        1 => {
            for _ in 0..8 {
                s.write_bool(true);
            }
        }
        4 => {
            s.write_u32(1234).write_bool(true);
        }
        7 => {
            s.write_u32(0)
                .write_f32(0.0)
                .write_i32(0)
                .write_bool(false)
                .write_bool(false);
        }
        8 => {
            s.write_f32(0.5).write_f32(1.5);
        }
        _ => {}
    }
}

/// Everything a non-character `SCUnitStatePacket` writes after the kind-specific header.
struct NetUnit {
    owner_name: &'static str,
    pos: (f32, f32, f32),
    level: u8,
    layout: Layout,
    equipment: Vec<EquipItem>,
    model_kind: u8,
    hp: i32,
    mp: i32,
    /// AttachPoint1 bytes.
    attach1: Vec<u8>,
    /// Bonding bytes (`-1` or a bc object id).
    bonding: Vec<u8>,
    posture: u8,
    has_loot: bool,
    faction: u32,
    expedition: u32,
    yaw: i8,
    buffs: [Vec<BuffSpec>; 3],
}

impl NetUnit {
    fn new() -> Self {
        Self {
            owner_name: "",
            pos: (11_000.5, 8_500.25, 140.0),
            level: 12,
            layout: Layout::Npc,
            equipment: Vec::new(),
            model_kind: 2,
            hp: 3_000,
            mp: 400,
            attach1: vec![255],
            bonding: vec![0xFF],
            posture: 0,
            has_loot: false,
            faction: 115,
            expedition: 0,
            yaw: 32,
            buffs: [Vec::new(), Vec::new(), Vec::new()],
        }
    }
}

/// `write_net_unit` (world/unit_state.rs:413): header bytes come from the caller.
fn net_unit_state(
    obj: u32,
    name: &str,
    ty: u8,
    header: &dyn Fn(&mut PacketStream),
    u: &NetUnit,
) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_bc(obj).write_str(name).write_u8(ty);
    header(&mut s);
    s.write_str(u.owner_name);
    s.write_position(u.pos.0, u.pos.1, u.pos.2)
        .write_f32(1.0) // scale
        .write_u8(u.level)
        .write_u32(555); // model id
    write_equipment(&mut s, u.layout, &u.equipment);
    write_model_params(&mut s, u.model_kind);
    s.write_bc(0)
        .write_i32(u.hp * 100)
        .write_i32(u.mp * 100)
        .write_raw(&u.attach1)
        .write_raw(&u.bonding);
    write_posture(&mut s, u.posture, u.has_loot);
    s.write_u8(0) // active weapon
        .write_u8(0) // learned skill count
        .write_i32(0) // learned buff count
        .write_i8(1)
        .write_i8(2)
        .write_i8(u.yaw)
        .write_u8(9) // race / gender
        .write_pisc(&[0, 0, 0, 0])
        .write_pisc(&[i64::from(u.faction), i64::from(u.expedition), 0, 0])
        .write_pisc(&[0, 0])
        .write_u16(0x2000);
    write_buff_lists(&mut s, [&u.buffs[0], &u.buffs[1], &u.buffs[2]]);
    s
}

fn unit_info(s: &PacketStream) -> UnitInfo {
    match game(sc::SCUnitStatePacket, s) {
        WorldEvent::UnitState(u) => *u,
        other => panic!("not a unit state: {other:?}"),
    }
}

#[test]
fn npc_unit_state() {
    let mut u = NetUnit::new();
    u.owner_name = "Summoner";
    u.layout = Layout::Npc;
    u.equipment = vec![
        EquipItem {
            slot: 15,
            template: 4_100,
            id: 0x1234,
            grade: 2,
            detail: 0,
        },
        EquipItem {
            slot: 19,
            template: 7,
            id: 0,
            grade: 0,
            detail: 0,
        },
        EquipItem {
            slot: 27,
            template: 4_200,
            id: 0x5678,
            grade: 1,
            detail: 1,
        },
    ];
    u.posture = 4;
    u.has_loot = true;
    u.faction = 115;
    u.expedition = 3;
    u.buffs[0] = vec![(1, 8_000_001, false, 0), (2, 8_000_002, true, 3)];
    u.buffs[1] = vec![(3, 5_000, false, 0)];
    let s = net_unit_state(
        0x0ABC,
        "Wolf",
        unit_type::NPC,
        &|s| {
            s.write_bc(0x0ABC)
                .write_u32(1_234)
                .write_u32(1001)
                .write_u8(0);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!(info.obj, 0x0ABC);
    assert_eq!(info.unit_type, unit_type::NPC);
    assert_eq!(info.id, 1_234);
    assert_eq!(&*info.name, "Wolf");
    assert_eq!(&*info.owner_name, "Summoner");
    assert_eq!(info.owner_id, 1001);
    assert!((info.pos.0 - 11_000.5).abs() < 0.01 && (info.pos.1 - 8_500.25).abs() < 0.01);
    assert!((info.pos.2 - 140.0).abs() < 0.01);
    assert_eq!(info.level, 12);
    assert_eq!((info.hp, info.mp), (3_000, 400));
    assert!(!info.dead);
    assert!(info.has_loot);
    assert_eq!((info.faction_id, info.expedition_id), (115, 3));
    assert_eq!(info.yaw, 32);
    assert!(info.skills.is_empty());
    assert_eq!(
        info.buffs.as_ref(),
        [
            BuffInfo {
                list: 0,
                index: 1,
                buff_id: 8_000_001,
                duration_ms: 60_000,
                charge: 0
            },
            BuffInfo {
                list: 0,
                index: 2,
                buff_id: 8_000_002,
                duration_ms: 60_000,
                charge: 3
            },
            BuffInfo {
                list: 1,
                index: 3,
                buff_id: 5_000,
                duration_ms: 60_000,
                charge: 0
            },
        ]
    );
    assert!(info.complete);
    no_panic_on_truncation(sc::SCUnitStatePacket, s.as_bytes());
    // The required part (up to the level) fails cleanly when cut short.
    assert!(parse(1, sc::SCUnitStatePacket, &s.as_bytes()[..12]).is_err());
}

#[test]
fn dead_npc_has_no_hit_points() {
    let mut u = NetUnit::new();
    u.hp = 0;
    u.has_loot = true;
    let s = net_unit_state(
        1,
        "Corpse",
        unit_type::NPC,
        &|s| {
            s.write_bc(1).write_u32(9).write_u32(0).write_u8(0);
        },
        &u,
    );
    let info = unit_info(&s);
    assert!(info.dead && info.has_loot && info.complete);
    assert_eq!(info.hp, 0);
}

#[test]
fn slave_unit_state() {
    // header: db id, tl id, template, summoner; Slave equipment keeps full items in 19..=25
    let mut u = NetUnit::new();
    u.layout = Layout::Slave;
    u.owner_name = "Captain";
    u.equipment = vec![
        EquipItem {
            slot: 20,
            template: 6_000,
            id: 9,
            grade: 1,
            detail: 0,
        },
        EquipItem {
            slot: 5,
            template: 6_001,
            id: 10,
            grade: 1,
            detail: 2,
        },
    ];
    u.attach1 = vec![3, 0x01, 0x02, 0x00]; // attach point 3 + owner object (bc)
    u.bonding = vec![0x0A, 0x0B, 0x00]; // bonding object id (bc)
    u.posture = 8;
    let s = net_unit_state(
        0x0200,
        "Cart",
        unit_type::SLAVE,
        &|s| {
            s.write_u32(88)
                .write_u16(321)
                .write_u32(3_000)
                .write_u32(1001);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!(info.unit_type, unit_type::SLAVE);
    assert_eq!((info.id, info.tl_id, info.owner_id), (3_000, 321, 1001));
    assert_eq!(&*info.owner_name, "Captain");
    assert_eq!(info.level, 12);
    assert_eq!(info.hp, 3_000);
    assert_eq!(info.faction_id, 115);
    assert!(info.complete);
    // Unattached slave: attach point -1 and no bonding.
    let mut u = NetUnit::new();
    u.layout = Layout::Slave;
    u.attach1 = vec![0xFF];
    let s = net_unit_state(
        0x0201,
        "Boat",
        unit_type::SLAVE,
        &|s| {
            s.write_u32(89).write_u16(322).write_u32(3_001).write_u32(0);
        },
        &u,
    );
    let info = unit_info(&s);
    assert!(info.complete);
    assert_eq!(info.faction_id, 115);
}

#[test]
fn house_mate_transfer_and_shipyard() {
    // Housing: tl id, template, build step; HouseState posture carries eight door/window flags.
    let mut u = NetUnit::new();
    u.layout = Layout::Full;
    u.owner_name = "Owner";
    u.posture = 1;
    u.faction = 161;
    let s = net_unit_state(
        0x0300,
        "House",
        unit_type::HOUSING,
        &|s| {
            s.write_u16(7).write_u32(8_800).write_i16(0);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!(info.unit_type, unit_type::HOUSING);
    assert_eq!((info.id, info.tl_id), (8_800, 7));
    assert_eq!(&*info.owner_name, "Owner");
    assert_eq!(info.faction_id, 161);
    assert!(info.complete);

    // Mate: tl id, template, owner.
    let mut u = NetUnit::new();
    u.layout = Layout::Full;
    let s = net_unit_state(
        0x0301,
        "Horse",
        unit_type::MATE,
        &|s| {
            s.write_u16(9).write_u32(7_001).write_u32(1002);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!((info.id, info.tl_id, info.owner_id), (7_001, 9, 1002));
    assert!(info.complete);

    // Transfer: only the seven appearance templates; attach point 255 has no object.
    let mut u = NetUnit::new();
    u.layout = Layout::BodyOnly;
    u.equipment = vec![EquipItem {
        slot: 21,
        template: 33,
        id: 0,
        grade: 0,
        detail: 0,
    }];
    u.attach1 = vec![255];
    let s = net_unit_state(
        0x0302,
        "Carriage",
        unit_type::TRANSFER,
        &|s| {
            s.write_u16(10).write_u32(6_001);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!((info.id, info.tl_id), (6_001, 10));
    assert!(info.complete);

    // Transfer with an attach point: a bonding object follows.
    let mut u = NetUnit::new();
    u.layout = Layout::BodyOnly;
    u.attach1 = vec![2, 0x44, 0x03, 0x00];
    let s = net_unit_state(
        0x0303,
        "Carriage",
        unit_type::TRANSFER,
        &|s| {
            s.write_u16(11).write_u32(6_002);
        },
        &u,
    );
    assert!(unit_info(&s).complete);

    // Shipyard: u64 id, template.
    let mut u = NetUnit::new();
    u.layout = Layout::BodyOnly;
    let s = net_unit_state(
        0x0304,
        "Shipyard",
        unit_type::SHIPYARD,
        &|s| {
            s.write_u64(5).write_u32(9_001);
        },
        &u,
    );
    let info = unit_info(&s);
    assert_eq!(info.id, 9_001);
    assert!(info.complete);
}

/// `write_unit_state` (world/unit_state.rs:108) of a character; `visual` = visual options present.
fn character_state(visual: bool, bonded: bool) -> PacketStream {
    character_state_with(visual, bonded, &[])
}

/// A character unit state whose equipment also holds `extra` items.
fn character_state_with(visual: bool, bonded: bool, extra: &[EquipItem]) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_bc(0x0400)
        .write_str("Aria")
        .write_u8(0)
        .write_u32(1001)
        .write_i64(0)
        .write_str("");
    s.write_position(12_000.5, 9_000.25, 120.0)
        .write_f32(1.0)
        .write_u8(30)
        .write_u32(555);
    let mut items = vec![
        EquipItem {
            slot: 15,
            template: 40_001,
            id: 0x77,
            grade: 3,
            detail: 1,
        },
        EquipItem {
            slot: 19,
            template: 55_550,
            id: 0,
            grade: 0,
            detail: 0,
        },
    ];
    items.extend_from_slice(extra);
    write_equipment(&mut s, Layout::Full, &items);
    write_model_params(&mut s, 3);
    s.write_bc(0)
        .write_i32(1_500 * 100)
        .write_i32(600 * 100)
        .write_u8(255); // attach point 1
    if bonded {
        s.write_u8(2) // BondDoodad: attach point, owner bc, kind, space, spot
            .write_bc(0x0910)
            .write_u8(1)
            .write_i32(2)
            .write_i32(0);
    } else {
        s.write_i8(-1);
    }
    s.write_u8(0).write_bool(false); // posture, isLooted
    s.write_u8(0); // active weapon
    s.write_u8(3); // learned skills
    for (id, level) in [(11_001u32, 1u8), (11_002, 2), (11_003, 3)] {
        s.write_u32(id).write_u8(level);
    }
    s.write_i32(2).write_u32(701).write_u32(702); // passive buffs
    s.write_i8(0).write_i8(0).write_i8(-20);
    s.write_u8(0x31) // race / gender
        .write_pisc(&[0, 0, 4, 0])
        .write_pisc(&[101, 77, 0, 0])
        .write_pisc(&[300, 5])
        .write_u16(0);
    for i in 0..10 {
        s.write_i32(i * 10).write_u8(255); // ability exp + order
    }
    s.write_u8(3).write_u8(1).write_u8(2).write_u8(3); // active abilities
    s.write_bc(0);
    if !visual {
        return s; // the server truncates here (no visual options / Stp)
    }
    s.write_raw(&[1, 2, 3, 4, 5, 6])
        .write_bool(true)
        .write_bool(false)
        .write_bool(false)
        .write_bool(false);
    s.write_i32(1); // premium
    for _ in 0..6 {
        s.write_i32(0); // pStat
    }
    write_buff_lists(
        &mut s,
        [
            &[(1, 8_000_011, false, 0), (2, 8_000_012, false, 0)],
            &[],
            &[(3, 9_000, false, 1)],
        ],
    );
    s
}

#[test]
fn character_unit_state_reaches_every_field() {
    let s = character_state(true, false);
    let info = unit_info(&s);
    assert_eq!(info.unit_type, unit_type::CHARACTER);
    assert_eq!(info.id, 1001);
    assert_eq!(&*info.name, "Aria");
    assert!(info.owner_name.is_empty());
    assert_eq!(info.level, 30);
    assert_eq!((info.hp, info.mp), (1_500, 600));
    assert_eq!((info.faction_id, info.expedition_id), (101, 77));
    assert_eq!(info.yaw, -20);
    assert_eq!(
        info.skills.as_ref(),
        [(11_001, 1), (11_002, 2), (11_003, 3)]
    );
    assert_eq!(info.buffs.len(), 3);
    assert_eq!(info.buffs[0].buff_id, 8_000_011);
    assert_eq!(info.buffs[2].list, 2);
    assert_eq!(info.buffs[2].charge, 1);
    assert!(info.complete);
    no_panic_on_truncation(sc::SCUnitStatePacket, s.as_bytes());
}

#[test]
fn a_carried_pack_is_the_backpack_slot_of_a_character() {
    // no pack: the slot is empty
    assert_eq!(unit_info(&character_state(true, false)).backpack, 0);
    // models/container.rs:73: the backpack is equipment slot 26; a trade pack's item is a full
    // `Item.Write`, whatever the layout of the slots before it (here a weapon in slot 15)
    let pack = EquipItem {
        slot: 26,
        template: 28_091,
        id: 0x99,
        grade: 1,
        detail: 1,
    };
    let s = character_state_with(true, false, &[pack]);
    let info = unit_info(&s);
    assert_eq!(info.backpack, 28_091);
    // the rest of the packet is still read
    assert_eq!((info.faction_id, info.expedition_id), (101, 77));
    assert_eq!(info.buffs.len(), 3);
    assert!(info.complete);
    no_panic_on_truncation(sc::SCUnitStatePacket, s.as_bytes());
    // the pack of a bonded character, and one in a truncated packet
    assert_eq!(
        unit_info(&character_state_with(true, true, &[pack])).backpack,
        28_091
    );
    let cut = character_state_with(false, false, &[pack]);
    let info = unit_info(&cut);
    assert_eq!(info.backpack, 28_091);
    assert!(!info.complete);
}

#[test]
fn only_characters_have_a_backpack() {
    // an NPC with a template in slot 26 and a slave with an item there: not a carried pack
    let mut s = PacketStream::new();
    s.write_bc(0x0555).write_str("Guard").write_u8(1);
    s.write_bc(0x0555)
        .write_u32(0x1234)
        .write_u32(0)
        .write_u8(0);
    s.write_str("");
    s.write_position(100.0, 200.0, 30.0)
        .write_f32(1.0)
        .write_u8(10)
        .write_u32(0);
    write_equipment(
        &mut s,
        Layout::Npc,
        &[EquipItem {
            slot: 26,
            template: 28_091,
            id: 5,
            grade: 1,
            detail: 0,
        }],
    );
    write_model_params(&mut s, 0);
    s.write_bc(0).write_i32(100).write_i32(100);
    assert_eq!(unit_info(&s).backpack, 0);
}

#[test]
fn character_unit_state_with_a_bond_doodad() {
    let s = character_state(true, true);
    let info = unit_info(&s);
    assert_eq!(info.skills.len(), 3);
    assert_eq!(info.faction_id, 101);
    assert!(info.complete);
}

#[test]
fn truncated_character_unit_state_keeps_what_was_read() {
    // The server sends a character without visual options truncated after the abilities.
    let s = character_state(false, false);
    let info = unit_info(&s);
    assert_eq!(info.id, 1001);
    assert_eq!((info.hp, info.mp), (1_500, 600));
    assert_eq!((info.faction_id, info.expedition_id), (101, 77));
    assert_eq!(info.skills.len(), 3);
    assert!(info.buffs.is_empty());
    assert!(!info.complete);
}

#[test]
fn unit_state_of_an_unknown_kind_is_ignored() {
    let mut s = PacketStream::new();
    s.write_bc(1).write_str("Gimmick").write_u8(200);
    assert_eq!(parse(1, sc::SCUnitStatePacket, s.as_bytes()).unwrap(), None);
}

// ---- removal, movement, points --------------------------------------------------------------

#[test]
fn units_removed() {
    // packets/world.rs:566
    let mut s = PacketStream::new();
    s.write_u16(3)
        .write_bc(1)
        .write_bc(0x00FF_FFFF)
        .write_bc(77);
    let WorldEvent::UnitsRemoved { objs } = game(sc::SCUnitsRemovedPacket, &s) else {
        panic!("not a removal");
    };
    assert_eq!(objs.as_slice(), [1, 0x00FF_FFFF, 77]);
    no_panic_on_truncation(sc::SCUnitsRemovedPacket, s.as_bytes());
    // A count larger than the body is rejected before anything is read.
    assert!(parse(1, sc::SCUnitsRemovedPacket, &[0xFF, 0xFF, 1, 0, 0]).is_err());
}

/// `write_sc_one_unit_movement` (models/movement.rs:715) of a unit move.
fn unit_move(obj: u32, flags: u8, stance: u8) -> PacketStream {
    let mut s = PacketStream::new();
    s.write_bc(obj).write_u8(1).write_u32(1234).write_u8(flags);
    if flags & 0x10 != 0 {
        s.write_u32(5).write_u8(2);
    }
    s.write_position(10_000.5, 9_500.0, 100.0)
        .write_i16(1)
        .write_i16(2)
        .write_i16(3)
        .write_i8(4)
        .write_i8(5)
        .write_i8(-40)
        .write_i8(6)
        .write_i8(7)
        .write_i8(8)
        .write_u8(stance)
        .write_u8(0) // alertness
        .write_u8(0); // actor flags
    s
}

#[test]
fn unit_movement() {
    let s = unit_move(0x0102, 0x02, 1);
    let WorldEvent::UnitMoved {
        obj,
        kind,
        pos,
        yaw,
        stance,
    } = game(sc::SCOneUnitMovementPacket, &s)
    else {
        panic!("not a movement");
    };
    assert_eq!((obj, kind, yaw, stance), (0x0102, 1, -40, 1));
    assert!((pos.0 - 10_000.5).abs() < 0.01 && (pos.1 - 9_500.0).abs() < 0.01);
    assert!((pos.2 - 100.0).abs() < 0.01);
    // The optional sc type / phase block shifts the position.
    let s = unit_move(0x0103, 0x12, 0);
    let WorldEvent::UnitMoved { pos, yaw, .. } = game(sc::SCOneUnitMovementPacket, &s) else {
        panic!("not a movement");
    };
    assert!((pos.0 - 10_000.5).abs() < 0.01);
    assert_eq!(yaw, -40);
    no_panic_on_truncation(sc::SCOneUnitMovementPacket, s.as_bytes());
}

#[test]
fn vehicle_ship_and_transfer_moves_carry_a_position() {
    for kind in [2u8, 3, 5] {
        let mut s = PacketStream::new();
        s.write_bc(0x0500).write_u8(kind).write_u32(1).write_u8(0);
        s.write_position(10_000.0, 9_000.0, 50.0);
        s.write_raw(&[0; 40]); // kind-specific rest, not read
        let WorldEvent::UnitMoved {
            obj, kind: k, pos, ..
        } = game(sc::SCOneUnitMovementPacket, &s)
        else {
            panic!("not a movement");
        };
        assert_eq!((obj, k), (0x0500, kind));
        assert!((pos.0 - 10_000.0).abs() < 0.01 && (pos.1 - 9_000.0).abs() < 0.01);
    }
    // Ship requests and the default type are not positions bots track.
    for kind in [0u8, 4, 9] {
        let mut s = PacketStream::new();
        s.write_bc(1).write_u8(kind).write_raw(&[0; 40]);
        assert_eq!(
            parse(1, sc::SCOneUnitMovementPacket, s.as_bytes()).unwrap(),
            None
        );
    }
}

#[test]
fn yaw_helpers_use_the_two_server_conventions() {
    assert_eq!(unit_state_yaw_radians(0), 0.0);
    assert!((unit_state_yaw_radians(127) - std::f32::consts::TAU).abs() < 1e-5);
    // models/movement.rs:1197: 32 steps are a quarter turn.
    assert!((movement_direction_radians(32) - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    assert!((movement_direction_radians(-64) + std::f32::consts::PI).abs() < 1e-5);
}

#[test]
fn teleport_unit() {
    // systems/skills/packets_special.rs:201
    let mut s = PacketStream::new();
    s.write_u8(14)
        .write_i16(0)
        .write_position(15_000.0, 11_000.5, 77.0)
        .write_f32(77.0);
    let WorldEvent::TeleportUnit { reason, error, pos } = game(sc::SCTeleportUnitPacket, &s) else {
        panic!("not a teleport");
    };
    assert_eq!((reason, error), (14, 0));
    assert!((pos.0 - 15_000.0).abs() < 0.01 && (pos.1 - 11_000.5).abs() < 0.01);
}

#[test]
fn unit_points_are_divided_by_one_hundred() {
    // packets/world.rs:544
    let mut s = PacketStream::new();
    s.write_bc(0x0102)
        .write_i32(1_234 * 100)
        .write_i32(56 * 100);
    assert_eq!(
        game(sc::SCUnitPointsPacket, &s),
        WorldEvent::UnitPoints {
            obj: 0x0102,
            hp: 1_234,
            mp: 56
        }
    );
}

#[test]
fn unit_damaged_reports_caster_and_target() {
    // systems/skills/packets.rs:334-389: CastAction::Skill, SkillCaster::Unit, the two object
    // ids, crime state, two PISC groups, the hit tail.
    let mut s = PacketStream::new();
    s.write_u8(0)
        .write_u32(14_001)
        .write_u16(7) // CastAction::Skill
        .write_u8(0)
        .write_bc(0x0102) // SkillCaster::Unit
        .write_bc(0x0102)
        .write_bc(0x0304)
        .write_u8(0) // crime state
        .write_pisc(&[1_000, 5, 0])
        .write_pisc(&[0, 0, 9])
        .write_u8(0)
        .write_u16(288 | 1)
        .write_u8(1)
        .write_u8(1);
    assert_eq!(
        game(sc::SCUnitDamagedPacket, &s),
        WorldEvent::UnitDamaged {
            caster: 0x0102,
            target: 0x0304,
        }
    );
    no_panic_on_truncation(sc::SCUnitDamagedPacket, s.as_bytes());
    // An item caster carries the item fields after the object id.
    let mut s = PacketStream::new();
    s.write_u8(0)
        .write_u32(14_001)
        .write_u16(7)
        .write_u8(2)
        .write_bc(0x0102)
        .write_u64(0x55)
        .write_u32(0x66)
        .write_u8(1)
        .write_u32(0x77)
        .write_bc(0x0102)
        .write_bc(0x0304)
        .write_u8(0)
        .write_pisc(&[1, 0, 0])
        .write_pisc(&[0, 0, 0])
        .write_u8(0)
        .write_u16(288)
        .write_u8(1)
        .write_u8(1);
    assert_eq!(
        game(sc::SCUnitDamagedPacket, &s),
        WorldEvent::UnitDamaged {
            caster: 0x0102,
            target: 0x0304,
        }
    );
    // An unknown cast action is a malformed packet, not a hit.
    let mut s = PacketStream::new();
    s.write_u8(9).write_raw(&[0; 40]);
    assert!(parse(1, sc::SCUnitDamagedPacket, s.as_bytes()).is_err());
}

#[test]
fn unit_death_with_and_without_a_killer() {
    // systems/skills/packets.rs:407
    let mut s = PacketStream::new();
    s.write_bc(0x0102)
        .write_u8(1) // KillReason.Damage
        .write_i32(30_000)
        .write_i32(450)
        .write_u8(10)
        .write_bc(0x0999)
        .write_u8(0)
        .write_u16(0)
        .write_u8(0)
        .write_u8(0)
        .write_u8(0)
        .write_str("Wolf");
    assert_eq!(
        game(sc::SCUnitDeathPacket, &s),
        WorldEvent::UnitDeath {
            obj: 0x0102,
            killer: 0x0999,
            respawn_wait_ms: 30_000,
            lost_exp: 450,
            reason: 1
        }
    );
    let mut s = PacketStream::new();
    s.write_bc(0x0103)
        .write_u8(8)
        .write_i32(0)
        .write_i32(0)
        .write_u8(0)
        .write_bc(0);
    assert_eq!(
        game(sc::SCUnitDeathPacket, &s),
        WorldEvent::UnitDeath {
            obj: 0x0103,
            killer: 0,
            respawn_wait_ms: 0,
            lost_exp: 0,
            reason: 8
        }
    );
}

#[test]
fn resurrected_target_exp_level_combat_and_faction() {
    // systems/skills/packets_special.rs:65
    let mut s = PacketStream::new();
    s.write_bc(0x0102)
        .write_position(13_000.0, 10_000.0, 55.0)
        .write_f32(2.5);
    let WorldEvent::Resurrected { obj, pos, yaw } = game(sc::SCCharacterResurrectedPacket, &s)
    else {
        panic!("not a resurrection");
    };
    assert_eq!((obj, yaw), (0x0102, 2.5));
    assert!((pos.0 - 13_000.0).abs() < 0.01);

    // packets/world_objects.rs:219
    let mut s = PacketStream::new();
    s.write_bc(0x0102).write_bc(0x0888);
    assert_eq!(
        game(sc::SCTargetChangedPacket, &s),
        WorldEvent::TargetChanged {
            obj: 0x0102,
            target: 0x0888
        }
    );

    // systems/skills/packets.rs:872
    let mut s = PacketStream::new();
    s.write_bc(0x0102).write_i32(-300).write_bool(false);
    assert_eq!(
        game(sc::SCExpChangedPacket, &s),
        WorldEvent::ExpChanged {
            obj: 0x0102,
            delta: -300
        }
    );

    // systems/skills/progress.rs:106
    let mut s = PacketStream::new();
    s.write_bc(0x0102).write_u8(31);
    assert_eq!(
        game(sc::SCLevelChangedPacket, &s),
        WorldEvent::LevelChanged {
            obj: 0x0102,
            level: 31
        }
    );

    // systems/skills/packets.rs:628 (bc_packet!)
    let mut s = PacketStream::new();
    s.write_bc(0x0102);
    assert_eq!(
        game(sc::SCCombatEngagedPacket, &s),
        WorldEvent::CombatEngaged { obj: 0x0102 }
    );
    assert_eq!(
        game(sc::SCCombatClearedPacket, &s),
        WorldEvent::CombatCleared { obj: 0x0102 }
    );

    // systems/social/packets/faction.rs:13
    let mut s = PacketStream::new();
    s.write_bc(0x0102)
        .write_str("Aria")
        .write_u32(101)
        .write_u32(161)
        .write_bool(true);
    assert_eq!(
        game(sc::SCUnitFactionChangedPacket, &s),
        WorldEvent::FactionChanged {
            obj: 0x0102,
            old: 101,
            faction: 161,
            temp: true
        }
    );
}

// ---- doodads ---------------------------------------------------------------------------------

/// `Doodad.write` (models/doodad.rs:247).
fn write_doodad(s: &mut PacketStream, obj: u32, template: u32, phase: u32) {
    s.write_bc(obj)
        .write_u32(template)
        .write_bc(0x0A0A) // owner obj
        .write_bc(0) // parent obj
        .write_u8(0) // attach point
        .write_position(14_000.5, 9_100.25, 88.0)
        .write_i16(0)
        .write_i16(0)
        .write_i16(100)
        .write_f32(1.0) // scale
        .write_bool(true) // has loot item
        .write_u32(phase)
        .write_u32(1001) // owner id
        .write_u64(0) // ucc id
        .write_u32(0) // item template
        .write_u32(0) // type2
        .write_u32(0) // time left
        .write_i64(0) // plant time
        .write_u32(3) // quest glow
        .write_i32(0) // family
        .write_i32(-1) // puzzle group
        .write_u8(0) // owner type
        .write_u32(0) // owner db id
        .write_i32(0); // data
}

fn doodad(obj: u32, template: u32, phase: u32) -> DoodadInfo {
    DoodadInfo {
        obj,
        template,
        pos: (14_000.5, 9_100.25, 88.0),
        owner_obj: 0x0A0A,
        parent_obj: 0,
        phase,
        owner_id: 1001,
        quest_glow: 3,
        has_loot: true,
    }
}

fn approx(a: DoodadInfo, b: DoodadInfo) -> bool {
    (a.pos.0 - b.pos.0).abs() < 0.01
        && (a.pos.1 - b.pos.1).abs() < 0.01
        && (a.pos.2 - b.pos.2).abs() < 0.01
        && DoodadInfo {
            pos: (0.0, 0.0, 0.0),
            ..a
        } == DoodadInfo {
            pos: (0.0, 0.0, 0.0),
            ..b
        }
}

#[test]
fn doodad_created_single_and_multi() {
    // single: the bare Doodad.Write record
    let mut s = PacketStream::new();
    write_doodad(&mut s, 0x0601, 2_001, 7);
    assert_eq!(s.len(), 91);
    let WorldEvent::DoodadsCreated(list) = game(sc::SCDoodadCreatedPacket, &s) else {
        panic!("not doodads");
    };
    assert_eq!(list.len(), 1);
    assert!(approx(list[0], doodad(0x0601, 2_001, 7)));
    no_panic_on_truncation(sc::SCDoodadCreatedPacket, s.as_bytes());

    // multi (packets/world_objects.rs:32): u8 count + records
    let mut s = PacketStream::new();
    s.write_u8(3);
    write_doodad(&mut s, 0x0601, 2_001, 1);
    write_doodad(&mut s, 0x0602, 2_002, 2);
    write_doodad(&mut s, 0x0603, 2_003, 3);
    let WorldEvent::DoodadsCreated(list) = game(sc::SCDoodadsCreatedPacket, &s) else {
        panic!("not doodads");
    };
    assert_eq!(list.len(), 3);
    assert!(approx(list[2], doodad(0x0603, 2_003, 3)));
    no_panic_on_truncation(sc::SCDoodadsCreatedPacket, s.as_bytes());
    // A count larger than the body is rejected.
    assert!(parse(1, sc::SCDoodadsCreatedPacket, &[30, 0, 0]).is_err());
}

#[test]
fn doodad_removed_single_and_multi() {
    // packets/world_objects.rs:54: bc + `false`
    let mut s = PacketStream::new();
    s.write_bc(0x0601).write_bool(false);
    let WorldEvent::DoodadsRemoved(list) = game(sc::SCDoodadRemovedPacket, &s) else {
        panic!("not a removal");
    };
    assert_eq!(list.as_slice(), [0x0601]);

    // packets/world_objects.rs:72: u16 count, last, then (bc, false) per doodad
    let mut s = PacketStream::new();
    s.write_u16(2).write_bool(true);
    s.write_bc(0x0601).write_bool(false);
    s.write_bc(0x0602).write_bool(false);
    let WorldEvent::DoodadsRemoved(list) = game(sc::SCDoodadsRemovedPacket, &s) else {
        panic!("not a removal");
    };
    assert_eq!(list.as_slice(), [0x0601, 0x0602]);
    no_panic_on_truncation(sc::SCDoodadsRemovedPacket, s.as_bytes());
}

#[test]
fn doodad_phase_changed() {
    // systems/npcs/doodad/packets.rs:77
    let mut s = PacketStream::new();
    s.write_bc(0x0601)
        .write_u32(9)
        .write_u32(0)
        .write_i32(-1)
        .write_u32(0);
    assert_eq!(
        game(sc::SCDoodadPhaseChangedPacket, &s),
        WorldEvent::DoodadPhase {
            obj: 0x0601,
            phase: 9
        }
    );
}

#[test]
fn common_farm_list() {
    // systems/npcs/public_farm.rs (SCResponseCommonFarmListPacket::write): i32 count twice, then
    // per crop u32 farm type, u32 template, u32 time left, u32 func group, a position, i64 plant time
    let mut s = PacketStream::new();
    s.write_i32(2).write_i32(2);
    for (kind, template, left, phase, x, plant) in [
        (
            1u32,
            2259u32,
            540_000u32,
            4456u32,
            12_543.5f32,
            1_700_000_000i64,
        ),
        (2, 413, 0, 7, 12_600.25, 1_700_000_100),
    ] {
        s.write_u32(kind)
            .write_u32(template)
            .write_u32(left)
            .write_u32(phase)
            .write_position(x, 15_390.0, 120.5)
            .write_i64(plant);
    }
    let WorldEvent::CommonFarmList(list) = game(sc::SCResponseCommonFarmListPacket, &s) else {
        panic!("not a farm list");
    };
    assert_eq!(list.len(), 2);
    let a: CommonFarmEntry = list[0];
    assert_eq!(
        (a.farm_type, a.template, a.time_left, a.phase),
        (1, 2259, 540_000, 4456)
    );
    assert!((a.pos.0 - 12_543.5).abs() < 0.01 && (a.pos.1 - 15_390.0).abs() < 0.01);
    assert!((a.pos.2 - 120.5).abs() < 0.01);
    assert_eq!(a.plant_time, 1_700_000_000);
    assert_eq!((list[1].template, list[1].phase), (413, 7));
    no_panic_on_truncation(sc::SCResponseCommonFarmListPacket, s.as_bytes());
    // an empty list, and a count the body cannot hold
    let mut s = PacketStream::new();
    s.write_i32(0).write_i32(0);
    let WorldEvent::CommonFarmList(list) = game(sc::SCResponseCommonFarmListPacket, &s) else {
        panic!("not a farm list");
    };
    assert!(list.is_empty());
    let mut s = PacketStream::new();
    s.write_i32(9).write_i32(9);
    assert!(parse(1, sc::SCResponseCommonFarmListPacket, s.as_bytes()).is_err());
}

#[test]
fn mutated_unit_states_never_panic() {
    let bases = [
        character_state(true, false),
        character_state(true, true),
        character_state(false, false),
        net_unit_state(
            1,
            "Wolf",
            unit_type::NPC,
            &|s| {
                s.write_bc(1).write_u32(2).write_u32(3).write_u8(0);
            },
            &NetUnit::new(),
        ),
    ];
    for base in &bases {
        let bytes = base.as_bytes();
        for at in 0..bytes.len() {
            for value in [0x00, 0x01, 0x7F, 0x80, 0xFF] {
                let mut mutated = bytes.to_vec();
                mutated[at] = value;
                let _ = parse(1, sc::SCUnitStatePacket, &mutated);
            }
        }
    }
}

#[test]
fn resurrection_offer() {
    // systems/skills/packets_special.rs:47: a SkillCaster (kind byte + bc object id)
    let mut s = PacketStream::new();
    s.write_u8(0).write_bc(0x0ABC);
    assert_eq!(
        game(sc::SCNotifyResurrectionPacket, &s),
        WorldEvent::ResurrectOffer { caster_obj: 0x0ABC }
    );
}
