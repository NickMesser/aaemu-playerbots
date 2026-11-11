//! `sc_world` parsers: session, own character, inventory, items, skills, buffs, chat, errors and
//! loot. Bodies are built in the order of the server writers cited per test (AAEmu-1.2-Rust
//! `rust/crates/aaemu-game/src`).

mod sc_world_common;

use aa_proto::opcodes::{pp, sc};
use aa_proto::sc_world::{
    interesting, parse, Cooldown, EquipSlot, InvItem, ItemChange, LootItem, WorldEvent,
};
use aa_proto::PacketStream;
use sc_world_common::{
    game, no_panic_on_truncation, truncation_is_rejected, write_character_record, write_item,
    CharSpec, Equip,
};

// ---- session -------------------------------------------------------------------------------

#[test]
fn enter_world_response() {
    // packets/g2c.rs:32
    let mut s = PacketStream::new();
    s.write_i16(0)
        .write_bool(true)
        .write_u32(0xDEAD_BEEF)
        .write_u16(1250)
        .write_i64(1_700_000_000) // world time
        .write_u16(0) // pubKeySize
        .write_str(""); // pubKey
    assert_eq!(
        game(sc::X2EnterWorldResponsePacket, &s),
        WorldEvent::EnterWorldResponse {
            reason: 0,
            gm: true,
            token: 0xDEAD_BEEF,
            stream_port: 1250,
        }
    );
    truncation_is_rejected(sc::X2EnterWorldResponsePacket, s.as_bytes(), 9);
}

#[test]
fn change_state_is_a_proxy_packet() {
    // packets/proxy.rs:31
    let mut s = PacketStream::new();
    s.write_i32(3);
    assert_eq!(
        parse(2, pp::ChangeStatePacket, s.as_bytes()).unwrap(),
        Some(WorldEvent::ChangeState(3))
    );
    // The same opcode value at level 1 is the enter-world response, not a state change.
    assert!(!matches!(
        parse(1, pp::ChangeStatePacket, s.as_bytes()),
        Ok(Some(WorldEvent::ChangeState(_)))
    ));
    // Unknown levels are ignored.
    assert_eq!(parse(4, pp::ChangeStatePacket, s.as_bytes()).unwrap(), None);
    assert!(parse(2, pp::ChangeStatePacket, &[1, 2]).is_err());
}

fn mage() -> CharSpec {
    let mut c = CharSpec::new(1001, "Aria");
    c.level = 30;
    c.model_kind = 3;
    c.equip = vec![
        Equip {
            slot: 15,
            template: 40_001,
            id: 0x1_0000_0001,
            grade: 3,
            detail: 1, // equipment detail: 55 bytes
        },
        Equip {
            slot: 19,
            template: 55_550, // appearance part: template only
            id: 0,
            grade: 0,
            detail: 0,
        },
        Equip {
            slot: 26,
            template: 9_999,
            id: 77,
            grade: 1,
            detail: 3, // summon mate: 6 bytes
        },
        Equip {
            slot: 27,
            template: 8_888,
            id: 78,
            grade: 2,
            detail: 0,
        },
    ];
    c
}

fn warrior() -> CharSpec {
    let mut c = CharSpec::new(1002, "Bruno");
    c.race = 4;
    c.gender = 2;
    c.level = 1;
    c.model_kind = 0;
    c
}

#[test]
fn character_list_skips_native_records_exactly() {
    // packets/g2c.rs:131 with Character.Write (models/character.rs:449): two records per packet
    let mut s = PacketStream::new();
    s.write_bool(false).write_u8(2);
    write_character_record(&mut s, &mage());
    write_character_record(&mut s, &warrior());
    let WorldEvent::CharacterList { last, chars } = game(sc::SCCharacterListPacket, &s) else {
        panic!("not a character list");
    };
    assert!(!last);
    assert_eq!(chars.len(), 2);
    assert_eq!(chars[0].id, 1001);
    assert_eq!(&*chars[0].name, "Aria");
    assert_eq!((chars[0].race, chars[0].gender, chars[0].level), (3, 1, 30));
    // The second record starts exactly where the first ended (equipment, model params, tail).
    assert_eq!(chars[1].id, 1002);
    assert_eq!(&*chars[1].name, "Bruno");
    assert_eq!((chars[1].race, chars[1].gender, chars[1].level), (4, 2, 1));
    no_panic_on_truncation(sc::SCCharacterListPacket, s.as_bytes());
    // A truncated second record must not authorize a partial list.
    assert!(parse(1, sc::SCCharacterListPacket, &s.as_bytes()[..s.len() - 1]).is_err());
}

#[test]
fn character_list_empty_and_corrupt_counts() {
    let mut s = PacketStream::new();
    s.write_bool(true).write_u8(0);
    assert_eq!(
        game(sc::SCCharacterListPacket, &s),
        WorldEvent::CharacterList {
            last: true,
            chars: Default::default()
        }
    );
    // A count the body cannot hold is rejected before anything is allocated.
    assert!(parse(1, sc::SCCharacterListPacket, &[1, 255]).is_err());
}

#[test]
fn character_list_rejects_unknown_item_detail_type() {
    let mut c = mage();
    c.equip[0].detail = 1;
    let mut s = PacketStream::new();
    s.write_bool(true).write_u8(1);
    write_character_record(&mut s, &c);
    let mut bytes = s.into_bytes();
    // slot 15 is the first full item: patch its detail type byte (after template, id, grade,
    // flags, count) to an unsupported value.
    let template = 40_001u32.to_le_bytes();
    let at = bytes
        .windows(4)
        .position(|w| w == template)
        .expect("weapon template present");
    bytes[at + 4 + 8 + 1 + 1 + 4] = 11;
    assert!(parse(1, sc::SCCharacterListPacket, &bytes).is_err());
}

#[test]
fn character_created_and_failed() {
    // world/unit_state.rs:87 writes the whole record; id and name come first
    let mut s = PacketStream::new();
    write_character_record(&mut s, &mage());
    assert_eq!(
        game(sc::SCCreateCharacterResponsePacket, &s),
        WorldEvent::CharacterCreated {
            id: 1001,
            name: "Aria".into()
        }
    );
    // packets/world.rs:813
    let mut s = PacketStream::new();
    s.write_u8(4);
    assert_eq!(
        game(sc::SCCharacterCreationFailedPacket, &s),
        WorldEvent::CharacterCreateFailed { reason: 4 }
    );
}

#[test]
fn character_state_of_the_own_character() {
    // world/unit_state.rs:43
    let mut c = mage();
    c.hp = 5000;
    c.expedition = 77;
    c.family = 5;
    let mut s = PacketStream::new();
    s.write_u32(1) // instance id
        .write_u16(16)
        .write_raw(&[0x11; 16]) // guid
        .write_i32(0); // rwd
    write_character_record(&mut s, &c);
    s.write_f32(0.0)
        .write_f32(0.0)
        .write_f32(1.5) // rotation
        .write_i32(4242) // experience
        .write_i32(100) // recoverable exp
        .write_u32(0) // penaltied exp
        .write_i32(0) // return district id
        .write_u32(0) // return district
        .write_u32(9); // resurrection district
    for _ in 0..11 {
        s.write_u32(0); // ability exp
    }
    s.write_i32(1).write_i32(2).write_i32(3); // unread mail
    s.write_u8(50) // bag slots
        .write_i16(100) // bank slots
        .write_i64(c.money)
        .write_i64(c.bank_money)
        .write_i64(0)
        .write_i64(0)
        .write_bool(false) // auto AA
        .write_i32(0) // jury point
        .write_i32(0) // jail seconds
        .write_i64(0) // bounty money
        .write_i64(0) // bounty time
        .write_i32(0) // reported as bot
        .write_i32(0) // suspected
        .write_i32(0) // total play time
        .write_i64(0) // created
        .write_u8(0); // expanded expert
    let WorldEvent::CharacterState(own) = game(sc::SCCharacterStatePacket, &s) else {
        panic!("not a character state");
    };
    assert_eq!(own.id, 1001);
    assert_eq!(&*own.name, "Aria");
    assert_eq!((own.race, own.gender, own.level), (3, 1, 30));
    assert_eq!((own.hp, own.mp), (5000, 800));
    assert_eq!((own.zone_id, own.faction_id), (7, 101));
    assert_eq!((own.expedition_id, own.family_id), (77, 5));
    assert_eq!(own.abilities, [1, 2, 11]);
    assert!((own.pos.0 - 12000.5).abs() < 0.01);
    assert!((own.pos.1 - 9000.25).abs() < 0.01);
    assert_eq!(own.pos.2, 120.0);
    assert_eq!(own.yaw, 1.5);
    assert_eq!(own.instance_id, 1);
    assert_eq!((own.exp, own.recoverable_exp), (4242, 100));
    assert_eq!(own.labor, 50);
    assert_eq!(own.money, 123_456);
    assert_eq!(own.bank_money, 777);
    assert_eq!(own.loyalty, 9);
    assert_eq!((own.bag_slots, own.bank_slots), (50, 100));
    assert_eq!(
        own.equipment.as_ref(),
        [
            EquipSlot {
                slot: 15,
                template: 40_001,
                id: 0x1_0000_0001,
                grade: 3
            },
            EquipSlot {
                slot: 19,
                template: 55_550,
                id: 0,
                grade: 0
            },
            EquipSlot {
                slot: 26,
                template: 9_999,
                id: 77,
                grade: 1
            },
            EquipSlot {
                slot: 27,
                template: 8_888,
                id: 78,
                grade: 2
            },
        ]
    );
    no_panic_on_truncation(sc::SCCharacterStatePacket, s.as_bytes());
}

#[test]
fn leave_world_packets() {
    // packets/world.rs:779, :797, :1060
    let mut s = PacketStream::new();
    s.write_i32(10_000).write_u8(2).write_bool(true);
    assert_eq!(
        game(sc::SCPrepareLeaveWorldPacket, &s),
        WorldEvent::PrepareLeaveWorld {
            time_ms: 10_000,
            target: 2,
            idle_kick: true
        }
    );
    let mut s = PacketStream::new();
    s.write_u8(1);
    assert_eq!(
        game(sc::SCLeaveWorldGrantedPacket, &s),
        WorldEvent::LeaveWorldGranted { target: 1 }
    );
    assert_eq!(
        game(sc::SCLeaveWorldCanceledPacket, &PacketStream::new()),
        WorldEvent::LeaveWorldCanceled
    );
}

#[test]
fn load_instance_and_kicked() {
    // systems/skills/packets_special.rs:170
    let mut s = PacketStream::new();
    s.write_u32(5)
        .write_u32(6)
        .write_f32(100.0)
        .write_f32(200.0)
        .write_f32(30.0)
        .write_f32(0.25)
        .write_f32(-0.5)
        .write_f32(1.25);
    assert_eq!(
        game(sc::SCLoadInstancePacket, &s),
        WorldEvent::LoadInstance {
            instance_id: 5,
            zone_id: 6,
            pos: (100.0, 200.0, 30.0),
            yaw: 1.25,
            ang_x: 0.25,
            ang_y: -0.5,
        }
    );
    // packets/g2c_misc.rs:160
    let mut s = PacketStream::new();
    s.write_u8(3).write_str("idle");
    assert_eq!(
        game(sc::SCKickedPacket, &s),
        WorldEvent::Kicked {
            reason: 3,
            msg: "idle".into()
        }
    );
}

// ---- inventory and items -------------------------------------------------------------------

#[test]
fn inventory_init() {
    // packets/world.rs:60
    let mut s = PacketStream::new();
    s.write_u32(50).write_u32(100);
    assert_eq!(
        game(sc::SCCharacterInvenInitPacket, &s),
        WorldEvent::InventoryInit {
            bag_slots: 50,
            bank_slots: 100
        }
    );
}

#[test]
fn inventory_contents_keep_slot_positions() {
    // packets/world.rs:80: type, chunk count, start chunk, then ten Item.Write / `0` entries
    let mut s = PacketStream::new();
    s.write_u8(2).write_u8(1).write_u8(2);
    for position in 0..10 {
        match position {
            0 => write_item(&mut s, 1_000, 0xA0, 0, 5, 0),
            3 => write_item(&mut s, 2_000, 0xA1, 2, 1, 1), // equipment detail
            9 => write_item(&mut s, 3_000, 0xA2, 0, 20, 2), // summon slave detail
            _ => {
                s.write_i32(0);
            }
        }
    }
    let WorldEvent::Inventory {
        slot_type,
        chunk,
        items,
    } = game(sc::SCCharacterInvenContentsPacket, &s)
    else {
        panic!("not an inventory packet");
    };
    assert_eq!((slot_type, chunk), (2, 2));
    assert_eq!(
        items.as_ref(),
        [
            InvItem {
                slot: 20,
                id: 0xA0,
                template: 1_000,
                grade: 0,
                count: 5
            },
            InvItem {
                slot: 23,
                id: 0xA1,
                template: 2_000,
                grade: 2,
                count: 1
            },
            InvItem {
                slot: 29,
                id: 0xA2,
                template: 3_000,
                grade: 0,
                count: 20
            },
        ]
    );
    no_panic_on_truncation(sc::SCCharacterInvenContentsPacket, s.as_bytes());
    // An empty chunk is a valid packet.
    let mut empty = PacketStream::new();
    empty.write_u8(3).write_u8(1).write_u8(0);
    let WorldEvent::Inventory { items, .. } = game(sc::SCCharacterInvenContentsPacket, &empty)
    else {
        panic!("not an inventory packet");
    };
    assert!(items.is_empty());
}

/// `write_padded_details` (models/item_task.rs:342): `(short)128`, then 128 bytes.
fn write_task_details(s: &mut PacketStream, detail: u8) {
    s.write_i16(128).write_u8(detail).write_raw(&[0; 127]);
}

#[test]
fn item_task_covers_every_action() {
    // systems/economy/item_packets.rs:25 with ItemTask::write (models/item_task.rs:385)
    let mut s = PacketStream::new();
    s.write_u8(9) // reason: loot
        .write_u8(17); // task count
    s.write_u8(1).write_i32(-250); // money change
    s.write_u8(2).write_i32(40); // bank money change
    s.write_u8(3).write_u8(1).write_i32(12); // game point
    s.write_u8(16).write_i32(5); // AA point
    s.write_u8(17).write_i32(6); // bank AA point
    s.write_u8(18).write_u8(1); // auto-use AA point
    s.write_u8(5) // ItemAdd: slot type, slot, template, id, grade, flags, count, details, ...
        .write_u8(2)
        .write_u8(4)
        .write_u32(7_777)
        .write_u64(0xCAFE)
        .write_u8(2)
        .write_u8(0)
        .write_i32(3);
    write_task_details(&mut s, 0);
    s.write_i64(1)
        .write_i32(2)
        .write_u32(3)
        .write_u8(1)
        .write_i64(4)
        .write_i64(5);
    s.write_u8(4) // count update
        .write_u8(2)
        .write_u8(1)
        .write_u64(0xBEEF)
        .write_i32(-2)
        .write_u32(8_000);
    s.write_u8(7) // remove
        .write_u8(2)
        .write_u8(9)
        .write_u64(0xF00D)
        .write_u32(8_100);
    s.write_u8(8) // swap slots
        .write_u8(2)
        .write_u8(1)
        .write_u8(1)
        .write_u8(15)
        .write_u64(0x11)
        .write_u64(0x22)
        .write_i32(0);
    s.write_u8(14)
        .write_u8(2)
        .write_u8(3)
        .write_u64(0x33)
        .write_u8(4); // grade change
    s.write_u8(9).write_u8(2).write_u8(5).write_u64(0x44); // update details
    write_task_details(&mut s, 3);
    s.write_u8(10) // flag bits
        .write_u8(2)
        .write_u8(6)
        .write_u64(0x55)
        .write_u8(0x04)
        .write_u64(0);
    s.write_u8(11) // security
        .write_u8(2)
        .write_u8(7)
        .write_u64(0x66)
        .write_u8(0x0C)
        .write_bool(true)
        .write_bool(false)
        .write_bool(false)
        .write_i64(10)
        .write_i64(11);
    s.write_u8(12).write_u64(0x77); // remove crafting
    s.write_u8(13).write_u8(2).write_u8(8).write_u64(0x88); // seize
    s.write_u8(6) // take back (ItemBuyback)
        .write_u8(2)
        .write_u8(2)
        .write_u32(9_001)
        .write_u64(0x99)
        .write_u8(0)
        .write_u8(0)
        .write_i32(1);
    write_task_details(&mut s, 0);
    s.write_i64(1)
        .write_i32(2)
        .write_u32(3)
        .write_u8(1)
        .write_i64(4)
        .write_i64(5);
    s.write_u8(2) // force removed
        .write_u64(0xAAAA)
        .write_u64(0xBBBB);
    s.write_u32(0); // type(id)
    let WorldEvent::ItemTask(task) = game(sc::SCItemTaskSuccessPacket, &s) else {
        panic!("not an item task");
    };
    assert_eq!(task.reason, 9);
    assert_eq!(task.force_removed.as_slice(), [0xAAAA, 0xBBBB]);
    assert_eq!(
        task.changes.as_slice(),
        [
            ItemChange::Money(-250),
            ItemChange::BankMoney(40),
            ItemChange::GamePoint {
                kind: 1,
                amount: 12
            },
            ItemChange::AaPoint(5),
            ItemChange::BankAaPoint(6),
            ItemChange::AutoUseAaPoint(1),
            ItemChange::Added {
                action: 5,
                slot_type: 2,
                slot: 4,
                template: 7_777,
                id: 0xCAFE,
                grade: 2,
                count: 3
            },
            ItemChange::CountUpdate {
                slot_type: 2,
                slot: 1,
                id: 0xBEEF,
                delta: -2,
                template: 8_000
            },
            ItemChange::Removed {
                slot_type: 2,
                slot: 9,
                id: 0xF00D,
                template: 8_100
            },
            ItemChange::Moved {
                from_slot_type: 2,
                from_slot: 1,
                to_slot_type: 1,
                to_slot: 15,
                from_id: 0x11,
                to_id: 0x22
            },
            ItemChange::GradeChange {
                slot_type: 2,
                slot: 3,
                id: 0x33,
                grade: 4
            },
            ItemChange::Updated {
                slot_type: 2,
                slot: 5,
                id: 0x44,
                gems: None
            },
            ItemChange::FlagsUpdated {
                slot_type: 2,
                slot: 6,
                id: 0x55,
                bits: 0x04
            },
            ItemChange::FlagsUpdated {
                slot_type: 2,
                slot: 7,
                id: 0x66,
                bits: 0x0C
            },
            ItemChange::RemovedCrafting(0x77),
            ItemChange::RemovedSlot {
                slot_type: 2,
                slot: 8,
                id: 0x88
            },
            ItemChange::Added {
                action: 6,
                slot_type: 2,
                slot: 2,
                template: 9_001,
                id: 0x99,
                grade: 0,
                count: 1
            },
        ]
        .as_slice()
    );
    no_panic_on_truncation(sc::SCItemTaskSuccessPacket, s.as_bytes());
}

#[test]
fn item_task_rejects_unknown_actions() {
    let mut s = PacketStream::new();
    s.write_u8(1)
        .write_u8(1)
        .write_u8(19)
        .write_i32(0)
        .write_u8(0);
    assert!(parse(1, sc::SCItemTaskSuccessPacket, s.as_bytes()).is_err());
    // A task count the body cannot hold.
    assert!(parse(1, sc::SCItemTaskSuccessPacket, &[1, 200]).is_err());
}

#[test]
fn labor_changed() {
    // systems/skills/progress.rs:46
    let mut s = PacketStream::new();
    s.write_i32(-5).write_i32(300).write_i32(12).write_u8(2);
    assert_eq!(
        game(sc::SCCharacterLaborPowerChangedPacket, &s),
        WorldEvent::LaborChanged {
            delta: -5,
            actability: 300,
            point: 12,
            step: 2
        }
    );
}

// ---- skills and buffs ----------------------------------------------------------------------

#[test]
fn skill_learned_and_upgraded() {
    // systems/skills/packets.rs:814 (skill_level_packet!)
    let mut s = PacketStream::new();
    s.write_u32(11_001).write_u8(3);
    assert_eq!(
        game(sc::SCSkillLearnedPacket, &s),
        WorldEvent::SkillLearned {
            skill: 11_001,
            level: 3,
            upgrade: false
        }
    );
    assert_eq!(
        game(sc::SCSkillUpgradedPacket, &s),
        WorldEvent::SkillLearned {
            skill: 11_001,
            level: 3,
            upgrade: true
        }
    );
}

#[test]
fn cooldowns_and_reset() {
    // systems/skills/packets.rs:483: two i32-counted lists of (id, total, remaining)
    let mut s = PacketStream::new();
    s.write_i32(2);
    s.write_u32(100).write_u32(5000).write_u32(1200);
    s.write_u32(101).write_u32(3000).write_u32(3000);
    s.write_i32(1);
    s.write_u32(9).write_u32(10_000).write_u32(10);
    let WorldEvent::Cooldowns { skills, tags } = game(sc::SCCooldownsPacket, &s) else {
        panic!("not cooldowns");
    };
    assert_eq!(
        skills.as_ref(),
        [
            Cooldown {
                id: 100,
                total_ms: 5000,
                remaining_ms: 1200
            },
            Cooldown {
                id: 101,
                total_ms: 3000,
                remaining_ms: 3000
            }
        ]
    );
    assert_eq!(
        tags.as_ref(),
        [Cooldown {
            id: 9,
            total_ms: 10_000,
            remaining_ms: 10
        }]
    );
    no_panic_on_truncation(sc::SCCooldownsPacket, s.as_bytes());
    // An empty packet and a count the body cannot hold.
    let mut empty = PacketStream::new();
    empty.write_i32(0).write_i32(0);
    assert!(matches!(
        game(sc::SCCooldownsPacket, &empty),
        WorldEvent::Cooldowns { .. }
    ));
    assert!(parse(1, sc::SCCooldownsPacket, &[0xFF, 0xFF, 0xFF, 0x7F]).is_err());

    // systems/skills/packets.rs:510
    let mut s = PacketStream::new();
    s.write_bc(0x1234)
        .write_u32(100)
        .write_u32(9)
        .write_bool(true);
    assert_eq!(
        game(sc::SCSkillCooldownResetPacket, &s),
        WorldEvent::CooldownReset {
            obj: 0x1234,
            skill: 100,
            tag: 9,
            gcd: true
        }
    );
}

/// `SkillCaster.Write` of a unit caster (systems/skills/types.rs:105).
fn write_unit_caster(s: &mut PacketStream, obj: u32) {
    s.write_u8(0).write_bc(obj);
}

#[test]
fn skill_started_with_every_target_kind() {
    // systems/skills/packets.rs:94
    let build = |target: &dyn Fn(&mut PacketStream), flags: u8, result: Option<u8>| {
        let mut s = PacketStream::new();
        s.write_u32(11_001).write_u16(77);
        write_unit_caster(&mut s, 0x0102);
        target(&mut s);
        s.write_u8(0) // skill object: none
            .write_u16(10)
            .write_u16(20)
            .write_u8(0)
            .write_u8(flags);
        if let Some(r) = result {
            s.write_u8(r);
        }
        s
    };
    let unit = |s: &mut PacketStream| {
        s.write_u8(0).write_bc(0x0304);
    };
    let s = build(&unit, 0, None);
    assert_eq!(
        game(sc::SCSkillStartedPacket, &s),
        WorldEvent::SkillStarted {
            caster_obj: 0x0102,
            skill: 11_001,
            tl: 77,
            target_obj: 0x0304,
            result: 0
        }
    );
    no_panic_on_truncation(sc::SCSkillStartedPacket, s.as_bytes());
    // A failed start carries the result byte (flag bit 1).
    let s = build(&unit, 1, Some(9));
    let WorldEvent::SkillStarted { result, .. } = game(sc::SCSkillStartedPacket, &s) else {
        panic!("not a skill start");
    };
    assert_eq!(result, 9);
    // Position target: i64 x, i64 y, f32 z, f32 rot, bc, bc.
    let position = |s: &mut PacketStream| {
        s.write_u8(1)
            .write_i64(1)
            .write_i64(2)
            .write_f32(3.0)
            .write_f32(4.0)
            .write_bc(5)
            .write_bc(6);
    };
    let s = build(&position, 0, None);
    let WorldEvent::SkillStarted { target_obj, tl, .. } = game(sc::SCSkillStartedPacket, &s) else {
        panic!("not a skill start");
    };
    assert_eq!((target_obj, tl), (0, 77));
    // Item target and doodad target.
    let item = |s: &mut PacketStream| {
        s.write_u8(3)
            .write_bc(7)
            .write_u64(8)
            .write_u32(9)
            .write_u8(1);
    };
    let s = build(&item, 0, None);
    assert!(matches!(
        game(sc::SCSkillStartedPacket, &s),
        WorldEvent::SkillStarted { target_obj: 0, .. }
    ));
    let doodad = |s: &mut PacketStream| {
        s.write_u8(4).write_bc(0x0A0B);
    };
    let s = build(&doodad, 0, None);
    assert!(matches!(
        game(sc::SCSkillStartedPacket, &s),
        WorldEvent::SkillStarted {
            target_obj: 0x0A0B,
            ..
        }
    ));
    // Item caster and a text skill object.
    let mut s = PacketStream::new();
    s.write_u32(5).write_u16(1);
    s.write_u8(2)
        .write_bc(0x0F)
        .write_u64(1)
        .write_u32(2)
        .write_u8(3)
        .write_u32(4);
    s.write_u8(0).write_bc(0x10);
    s.write_u8(3).write_str("hello");
    s.write_u16(0).write_u16(0).write_u8(0).write_u8(0);
    assert!(matches!(
        game(sc::SCSkillStartedPacket, &s),
        WorldEvent::SkillStarted {
            caster_obj: 0x0F,
            target_obj: 0x10,
            ..
        }
    ));
}

#[test]
fn skill_ended_stopped_and_casting_stopped() {
    // systems/skills/packets.rs:161, :178, :195
    let mut s = PacketStream::new();
    s.write_u16(77);
    assert_eq!(
        game(sc::SCSkillEndedPacket, &s),
        WorldEvent::SkillEnded { tl: 77 }
    );
    let mut s = PacketStream::new();
    s.write_bc(0x55).write_u32(11_001);
    assert_eq!(
        game(sc::SCSkillStoppedPacket, &s),
        WorldEvent::SkillStopped {
            obj: 0x55,
            skill: 11_001
        }
    );
    let mut s = PacketStream::new();
    s.write_u16(78).write_u32(1500);
    assert_eq!(
        game(sc::SCCastingStoppedPacket, &s),
        WorldEvent::CastingStopped {
            tl: 78,
            duration_ms: 1500
        }
    );
}

#[test]
fn buff_created_and_removed() {
    // systems/skills/packets.rs:224
    let mut s = PacketStream::new();
    write_unit_caster(&mut s, 0x0AAA);
    s.write_u32(42) // caster id
        .write_bc(0x0BBB) // owner
        .write_u32(9) // index
        .write_u32(8_000_011) // buff id
        .write_u8(25) // caster level
        .write_i16(3) // ability level
        .write_u32(11_001) // skill id
        .write_pisc(&[2, 6000 / 10, 0, 1000 / 10]);
    assert_eq!(
        game(sc::SCBuffCreatedPacket, &s),
        WorldEvent::BuffCreated {
            obj: 0x0BBB,
            index: 9,
            buff_id: 8_000_011,
            caster_obj: 0x0AAA,
            caster_level: 25,
            skill_id: 11_001,
            charge: 2,
            duration_ms: 6000,
        }
    );
    no_panic_on_truncation(sc::SCBuffCreatedPacket, s.as_bytes());
    // A long duration needs the wider pisc forms.
    let mut s = PacketStream::new();
    write_unit_caster(&mut s, 1);
    s.write_u32(0)
        .write_bc(2)
        .write_u32(3)
        .write_u32(4)
        .write_u8(1)
        .write_i16(0)
        .write_u32(0)
        .write_pisc(&[0, 3_600_000 / 10, 0, 0]);
    let WorldEvent::BuffCreated { duration_ms, .. } = game(sc::SCBuffCreatedPacket, &s) else {
        panic!("not a buff");
    };
    assert_eq!(duration_ms, 3_600_000);

    // systems/skills/packets.rs:287
    let mut s = PacketStream::new();
    s.write_bc(0x0BBB).write_u32(9);
    assert_eq!(
        game(sc::SCBuffRemovedPacket, &s),
        WorldEvent::BuffRemoved {
            obj: 0x0BBB,
            index: 9
        }
    );
}

// ---- chat, errors, loot --------------------------------------------------------------------

#[test]
fn chat_message() {
    // packets/world.rs:710
    let mut s = PacketStream::new();
    s.write_i16(-3) // whisper
        .write_i16(0)
        .write_u32(0)
        .write_bc(0x1234)
        .write_u32(1001)
        .write_u8(0)
        .write_u8(3)
        .write_u32(101)
        .write_str("GM Aria")
        .write_str("hello there")
        .write_i32(0)
        .write_i32(0);
    assert_eq!(
        game(sc::SCChatMessagePacket, &s),
        WorldEvent::Chat {
            kind: -3,
            sender_obj: 0x1234,
            sender_id: 1001,
            name: "GM Aria".into(),
            msg: "hello there".into()
        }
    );
    // System text has no sender.
    let mut s = PacketStream::new();
    s.write_i16(-2)
        .write_i16(0)
        .write_u32(0)
        .write_bc(0)
        .write_u32(0)
        .write_u8(0)
        .write_u8(0)
        .write_u32(0)
        .write_str("")
        .write_str("server notice")
        .write_i32(0)
        .write_i32(0);
    assert_eq!(
        game(sc::SCChatMessagePacket, &s),
        WorldEvent::Chat {
            kind: -2,
            sender_obj: 0,
            sender_id: 0,
            name: "".into(),
            msg: "server notice".into()
        }
    );
    no_panic_on_truncation(sc::SCChatMessagePacket, s.as_bytes());
}

#[test]
fn error_message() {
    // packets/world.rs:758
    let mut s = PacketStream::new();
    s.write_i16(265)
        .write_i16(265)
        .write_u32(7)
        .write_bool(true);
    assert_eq!(
        game(sc::SCErrorMsgPacket, &s),
        WorldEvent::ErrorMsg {
            err: 265,
            err2: 265,
            kind: 7,
            notify: true
        }
    );
    truncation_is_rejected(sc::SCErrorMsgPacket, s.as_bytes(), 9);
}

#[test]
fn error_message_detail_reason() {
    // systems/economy/craft.rs `error2(148, reason, 0, false)`: the craft-stop reason is the
    // second error field.
    let mut s = PacketStream::new();
    s.write_i16(148)
        .write_i16(722)
        .write_u32(0)
        .write_bool(false);
    assert_eq!(
        game(sc::SCErrorMsgPacket, &s),
        WorldEvent::ErrorMsg {
            err: 148,
            err2: 722,
            kind: 0,
            notify: false
        }
    );
}

#[test]
fn lootable_state() {
    // systems/economy/loot_packets.rs:223
    let mut s = PacketStream::new();
    s.write_u16(0)
        .write_u16(1)
        .write_bc(0x2222)
        .write_u8(0)
        .write_bool(true);
    assert_eq!(
        game(sc::SCLootableStatePacket, &s),
        WorldEvent::LootableState {
            owner_obj: 0x2222,
            owner_type: 1,
            has_loot: true
        }
    );
}

#[test]
fn loot_bag_decodes_the_fake_item_ids() {
    // systems/economy/loot_packets.rs:88; ids are (ObjId << 32) + (OwnerType << 16) + index
    // (systems/economy/looting.rs:239)
    let id = |index: u64| (0x2222u64 << 32) + (1 << 16) + index;
    let mut s = PacketStream::new();
    s.write_u8(2);
    write_item(&mut s, 5_001, id(1), 0, 3, 0);
    write_item(&mut s, 5_002, id(2), 1, 1, 1);
    s.write_bool(true);
    let WorldEvent::LootBag {
        owner_obj,
        owner_type,
        items,
        loot_all,
    } = game(sc::SCLootBagDataPacket, &s)
    else {
        panic!("not a loot bag");
    };
    assert_eq!((owner_obj, owner_type, loot_all), (0x2222, 1, true));
    assert_eq!(
        items.as_slice(),
        [
            LootItem {
                index: 1,
                template: 5_001,
                count: 3
            },
            LootItem {
                index: 2,
                template: 5_002,
                count: 1
            }
        ]
    );
    no_panic_on_truncation(sc::SCLootBagDataPacket, s.as_bytes());
    // An empty bag.
    let mut s = PacketStream::new();
    s.write_u8(0).write_bool(false);
    assert_eq!(
        game(sc::SCLootBagDataPacket, &s),
        WorldEvent::LootBag {
            owner_obj: 0,
            owner_type: 0,
            items: Default::default(),
            loot_all: false
        }
    );
}

#[test]
fn loot_took_and_failed() {
    // systems/economy/loot_packets.rs:108, :135
    let mut s = PacketStream::new();
    s.write_u32(5_001)
        .write_u16(2)
        .write_u16(1)
        .write_bc(0x2222)
        .write_u8(0)
        .write_i32(4);
    assert_eq!(
        game(sc::SCLootItemTookPacket, &s),
        WorldEvent::LootTook {
            template: 5_001,
            index: 2,
            count: 4,
            owner_type: 1,
            owner_obj: 0x2222
        }
    );
    let mut s = PacketStream::new();
    s.write_i16(22)
        .write_u16(2)
        .write_u16(1)
        .write_bc(0x2222)
        .write_u8(0)
        .write_u32(5_001);
    assert_eq!(
        game(sc::SCLootItemFailedPacket, &s),
        WorldEvent::LootFailed {
            err: 22,
            index: 2,
            owner_type: 1,
            owner_obj: 0x2222,
            template: 5_001
        }
    );
}

// ---- zone conflicts ------------------------------------------------------------------------

#[test]
fn conflict_zone_state() {
    use aa_proto::sc_world::conflict_state as cs;
    // packets/world.rs:346: u16 zone group, u8 state, i64 end time
    let mut s = PacketStream::new();
    s.write_u16(7).write_u8(cs::WAR).write_i64(1_700_000_123);
    assert_eq!(
        s.as_bytes(),
        [
            7, 0, // zone group
            6, // state
            0x7B, 0xF1, 0x53, 0x65, 0, 0, 0, 0, // end time, little endian
        ]
    );
    assert_eq!(
        game(sc::SCConflictZoneStatePacket, &s),
        WorldEvent::ConflictZoneState {
            zone_group: 7,
            state: cs::WAR,
            end_unix_s: 1_700_000_123,
        }
    );
    truncation_is_rejected(sc::SCConflictZoneStatePacket, s.as_bytes(), 11);

    // a state the table does not know is passed on as it is, and so is no end time at all
    let mut s = PacketStream::new();
    s.write_u16(0xFFFF).write_u8(200).write_i64(0);
    assert_eq!(
        game(sc::SCConflictZoneStatePacket, &s),
        WorldEvent::ConflictZoneState {
            zone_group: 0xFFFF,
            state: 200,
            end_unix_s: 0,
        }
    );
    // the packet is a game packet only
    assert_eq!(
        parse(2, sc::SCConflictZoneStatePacket, s.as_bytes()).unwrap(),
        None
    );
}

#[test]
fn unit_equipments_changed() {
    // systems/economy/item_packets.rs:64: bc obj, u8 count, then per slot u8 slot and Item.Write
    // (the empty item is the 4-byte zero template)
    let mut s = PacketStream::new();
    s.write_bc(0x0400).write_u8(2);
    s.write_u8(26);
    write_item(&mut s, 28_091, 0x1122_3344_5566, 2, 1, 1);
    s.write_u8(15).write_i32(0);
    let ev = game(sc::SCUnitEquipmentsChangedPacket, &s);
    assert_eq!(
        ev,
        WorldEvent::UnitEquipmentsChanged {
            obj: 0x0400,
            slots: smallvec::smallvec![(26, 28_091), (15, 0)],
        }
    );
    // a pack put down (the slot is emptied) is the single empty form
    let mut s = PacketStream::new();
    s.write_bc(0x0400).write_u8(1).write_u8(26).write_i32(0);
    assert_eq!(
        game(sc::SCUnitEquipmentsChangedPacket, &s),
        WorldEvent::UnitEquipmentsChanged {
            obj: 0x0400,
            slots: smallvec::smallvec![(26, 0)],
        }
    );
    // nothing changed
    let mut s = PacketStream::new();
    s.write_bc(7).write_u8(0);
    assert_eq!(
        game(sc::SCUnitEquipmentsChangedPacket, &s),
        WorldEvent::UnitEquipmentsChanged {
            obj: 7,
            slots: smallvec::smallvec![],
        }
    );
    // an item of an unknown detail type is a malformed packet, a count that cannot fit too
    let mut s = PacketStream::new();
    s.write_bc(0x0400).write_u8(1).write_u8(26);
    s.write_u32(5)
        .write_u64(1)
        .write_u8(0)
        .write_u8(0)
        .write_i32(1)
        .write_u8(77);
    assert!(parse(1, sc::SCUnitEquipmentsChangedPacket, s.as_bytes()).is_err());
    let mut s = PacketStream::new();
    s.write_bc(0x0400).write_u8(200);
    assert!(parse(1, sc::SCUnitEquipmentsChangedPacket, s.as_bytes()).is_err());
    // the packet is a game packet only
    let mut s = PacketStream::new();
    s.write_bc(7).write_u8(0);
    assert_eq!(
        parse(2, sc::SCUnitEquipmentsChangedPacket, s.as_bytes()).unwrap(),
        None
    );
}

#[test]
fn pvp_points() {
    use aa_proto::sc_world::pvp_points;
    // packets/g2c_misc.rs:104 (body checked in the server's own test: [5,0,0, 2, 10,0,0,0]):
    // bc unit, u8 kind, i32 point
    let mut s = PacketStream::new();
    s.write_bc(5).write_u8(2).write_i32(10);
    assert_eq!(s.as_bytes(), [5, 0, 0, 2, 10, 0, 0, 0]);
    assert_eq!(
        game(sc::SCUnitPvPPointsChangedPacket, &s),
        WorldEvent::PvpPoints {
            obj: 5,
            kind: 2,
            points: 10
        }
    );
    let mut s = PacketStream::new();
    s.write_bc(0x0400)
        .write_u8(pvp_points::HONOR)
        .write_i32(-1234);
    assert_eq!(
        game(sc::SCUnitPvPPointsChangedPacket, &s),
        WorldEvent::PvpPoints {
            obj: 0x0400,
            kind: 0,
            points: -1234
        }
    );
    truncation_is_rejected(sc::SCUnitPvPPointsChangedPacket, s.as_bytes(), 8);
    assert_eq!(pvp_points::HOSTILE_KILLS, 1);
}

#[test]
fn conflict_states_have_the_server_values() {
    use aa_proto::sc_world::conflict_state as cs;
    // data/world_data.rs:189 (`ZoneConflictType`)
    assert_eq!(
        [
            cs::TENSION,
            cs::DANGER,
            cs::DISPUTE,
            cs::UNREST,
            cs::CRISIS,
            cs::CONFLICT,
            cs::WAR,
            cs::PEACE
        ],
        [0, 1, 2, 3, 4, 5, 6, 7]
    );
    for state in 0..=255u8 {
        assert_eq!(cs::at_war(state), state == 5 || state == 6, "state {state}");
    }
    assert_eq!(cs::name(cs::TENSION), "tension");
    assert_eq!(cs::name(cs::WAR), "war");
    assert_eq!(cs::name(cs::PEACE), "peace");
    assert_eq!(cs::name(8), "unknown");
}

// ---- tower defence events (rifts) ----------------------------------------------------------

/// `TowerDefKey` (systems/instances/packets.rs:101): `u32 definition, u16 zone group`.
fn write_event_key(s: &mut PacketStream, def: u32, zone_group: u16) {
    s.write_u32(def).write_u16(zone_group);
}

#[test]
fn tower_def_start_and_end() {
    use aa_proto::sc_event::EventId;
    // systems/instances/tests.rs `tower_def_packets_are_byte_exact`: key 12 / 5, zone 129
    let mut s = PacketStream::new();
    write_event_key(&mut s, 12, 5);
    s.write_u32(129);
    assert_eq!(s.as_bytes(), [12, 0, 0, 0, 5, 0, 129, 0, 0, 0]);
    let id = EventId {
        def: 12,
        zone_group: 5,
    };
    assert_eq!(
        game(sc::SCTowerDefStartPacket, &s),
        WorldEvent::TowerDefStart { id, zone_key: 129 }
    );
    assert_eq!(
        game(sc::SCTowerDefEndPacket, &s),
        WorldEvent::TowerDefEnd { id, zone_key: 129 }
    );
    truncation_is_rejected(sc::SCTowerDefStartPacket, s.as_bytes(), 10);
    truncation_is_rejected(sc::SCTowerDefEndPacket, s.as_bytes(), 10);
    // the packets are game packets only
    assert_eq!(
        parse(2, sc::SCTowerDefStartPacket, s.as_bytes()).unwrap(),
        None
    );
}

#[test]
fn tower_def_wave_start() {
    use aa_proto::sc_event::EventId;
    // systems/instances/tests.rs: key 12 / 5, zone 129, step 3
    let bytes = [12, 0, 0, 0, 5, 0, 129, 0, 0, 0, 3, 0, 0, 0];
    let mut s = PacketStream::new();
    write_event_key(&mut s, 12, 5);
    s.write_u32(129).write_u32(3);
    assert_eq!(s.as_bytes(), bytes);
    assert_eq!(
        game(sc::SCTowerDefWaveStartPacket, &s),
        WorldEvent::TowerDefWave {
            id: EventId {
                def: 12,
                zone_group: 5
            },
            zone_key: 129,
            step: 3,
        }
    );
    truncation_is_rejected(sc::SCTowerDefWaveStartPacket, s.as_bytes(), 14);
}

#[test]
fn tower_def_list() {
    use aa_proto::sc_event::{EventId, TowerDefInfo};
    // systems/instances/tests.rs `tower_def_packets_are_byte_exact`: one entry, the target at
    // (100.5, 2000.25, 30) with object id 0x0A0B0C in wave 4
    let mut s = PacketStream::new();
    s.write_u8(1);
    write_event_key(&mut s, 12, 5);
    s.write_u32(129)
        .write_u32(2)
        .write_bc(0x0A0B0C)
        .write_i64(((100.5f32 * 4096.0) as i64) << 32)
        .write_i64(((2000.25f32 * 4096.0) as i64) << 32)
        .write_f32(30.0)
        .write_u32(4);
    assert_eq!(s.len(), 1 + 41);
    let info = TowerDefInfo {
        id: EventId {
            def: 12,
            zone_group: 5,
        },
        zone_key: 129,
        spot_id: 2,
        target_obj: 0x0A0B0C,
        pos: (100.5, 2000.25, 30.0),
        step: 4,
    };
    assert!(info.has_target());
    assert_eq!(
        game(sc::SCTowerDefListPacket, &s),
        WorldEvent::TowerDefList(vec![info].into_boxed_slice())
    );
    truncation_is_rejected(sc::SCTowerDefListPacket, s.as_bytes(), 42);

    // an event without a live target: object id and position are zero
    let mut s = PacketStream::new();
    s.write_u8(2);
    write_event_key(&mut s, 3, 20);
    s.write_u32(257)
        .write_u32(9846)
        .write_bc(0)
        .write_i64(0)
        .write_i64(0)
        .write_f32(0.0)
        .write_u32(0);
    write_event_key(&mut s, 13, 20);
    s.write_u32(257)
        .write_u32(14335)
        .write_bc(77)
        .write_i64(((-12.5f32 * 4096.0) as i64) << 32)
        .write_i64(((3.0f32 * 4096.0) as i64) << 32)
        .write_f32(-1.5)
        .write_u32(1);
    let WorldEvent::TowerDefList(list) = game(sc::SCTowerDefListPacket, &s) else {
        panic!("not a list");
    };
    assert_eq!(list.len(), 2);
    assert!(!list[0].has_target());
    assert_eq!(list[0].pos, (0.0, 0.0, 0.0));
    assert_eq!((list[0].id.def, list[0].spot_id), (3, 9846));
    assert_eq!(list[1].target_obj, 77);
    assert_eq!(list[1].pos, (-12.5, 3.0, -1.5));
    assert_eq!(list[1].step, 1);
    truncation_is_rejected(sc::SCTowerDefListPacket, s.as_bytes(), 1 + 2 * 41);

    // no events at all (what a character gets after an event ended)
    let mut s = PacketStream::new();
    s.write_u8(0);
    assert_eq!(
        game(sc::SCTowerDefListPacket, &s),
        WorldEvent::TowerDefList(Vec::new().into_boxed_slice())
    );
}

#[test]
fn tower_def_list_count_is_checked_against_the_body() {
    // systems/instances/tests.rs `tower_def_list_count_and_payload_are_bounded...`: 41 bytes each
    for supplied in [0u8, 100] {
        let mut s = PacketStream::new();
        s.write_u8(supplied);
        for id in 1..=u32::from(supplied) {
            s.write_u32(id).write_raw(&[0; 37]);
        }
        assert_eq!(s.len(), 1 + usize::from(supplied) * 41);
        let WorldEvent::TowerDefList(list) = game(sc::SCTowerDefListPacket, &s) else {
            panic!("not a list");
        };
        assert_eq!(list.len(), usize::from(supplied));
    }
    // a count the body cannot hold is rejected before anything is allocated for it
    let mut s = PacketStream::new();
    s.write_u8(200).write_raw(&[0; 41]);
    assert!(parse(1, sc::SCTowerDefListPacket, s.as_bytes()).is_err());
}

// ---- dispatcher ----------------------------------------------------------------------------

#[test]
fn interesting_matches_what_parse_handles() {
    for level in [0u8, 1, 2, 3, 4] {
        for opcode in 0..0x1000u16 {
            // An empty body is rejected or yields an event for a handled packet and `None` for an
            // ignored one, so the two views must agree.
            let handled = !matches!(parse(level, opcode, &[]), Ok(None));
            assert_eq!(
                interesting(level, opcode),
                handled,
                "level {level} opcode {opcode:#x}"
            );
        }
    }
    // 0xfff marks opcodes the 1.2 client does not have; it must never be handled.
    assert!(!interesting(1, 0xfff));
    assert!(interesting(1, sc::SCUnitStatePacket));
    assert!(!interesting(1, sc::SCTimeOfDayPacket));
    assert!(interesting(2, pp::ChangeStatePacket));
    assert!(!interesting(2, pp::PingPacket));
}

#[test]
fn ignored_packets_do_not_read_their_body() {
    // A frequent packet bots ignore: it must parse to `None` whatever the body.
    assert_eq!(parse(1, sc::SCTimeOfDayPacket, &[]).unwrap(), None);
    // A handled packet with a short body is a malformed packet, never a silent `None`.
    assert!(parse(1, sc::SCUnitDamagedPacket, &[1, 2, 3]).is_err());
}

#[test]
fn events_stay_small() {
    // Hundreds of bots pass these around; keep the enum within one cache line (64 bytes today).
    assert!(
        std::mem::size_of::<WorldEvent>() <= 64,
        "WorldEvent is {} bytes",
        std::mem::size_of::<WorldEvent>()
    );
}

#[test]
fn random_bodies_never_panic() {
    // xorshift64: deterministic noise for every handled opcode at both levels.
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for level in [1u8, 2] {
        for opcode in 0..0x400u16 {
            if !interesting(level, opcode) {
                continue;
            }
            for _ in 0..300 {
                let len = (next() % 400) as usize;
                let body: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                let _ = parse(level, opcode, &body);
            }
        }
    }
}
