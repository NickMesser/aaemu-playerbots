//! Byte-exact tests of the `cs` packet builders.
//!
//! Every expected body is written out by hand from the layout the server's `read()` expects (the
//! `file:line` of each read is in the builder's doc comment), not produced by the builder.

use aa_proto::cs::{self, Out};
use aa_proto::game::{LEVEL_GAME, LEVEL_PROXY};
use aa_proto::opcodes::{cs as op, pp};

#[track_caller]
fn check(out: Out, level: u8, opcode: u16, body: &[u8]) {
    assert_eq!(out.level, level, "level of {}", out.name());
    assert_eq!(out.opcode, opcode, "opcode of {}", out.name());
    assert_eq!(out.body, body, "body of {}", out.name());
}

#[track_caller]
fn game(out: Out, opcode: u16, body: &[u8]) {
    check(out, LEVEL_GAME, opcode, body);
}

fn zeros(n: usize) -> Vec<u8> {
    vec![0; n]
}

/// `fx64` of 1.0 / -2.0: `(v * 4096) << 32` as a little-endian `i64`.
const X_1: [u8; 8] = [0, 0, 0, 0, 0, 0x10, 0, 0];
const Y_M2: [u8; 8] = [0, 0, 0, 0, 0, 0xE0, 0xFF, 0xFF];
/// 3.5f32 and 2.0f32 and 1.0f32.
const F_3_5: [u8; 4] = [0, 0, 0x60, 0x40];
const F_2_0: [u8; 4] = [0, 0, 0, 0x40];
const F_1_0: [u8; 4] = [0, 0, 0x80, 0x3F];

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

// ---- session --------------------------------------------------------------------------------

#[test]
fn enter_world() {
    let mut body = cat(&[
        &[0; 8],
        &[0x0D, 0x0C, 0x0B, 0x0A],
        &[0x04, 0x03, 0x02, 0x01],
    ]);
    body.extend(zeros(4 + 1 + 8)); // zone, tb, revision
    assert_eq!(body.len(), 29);
    game(
        cs::enter_world(0x0A0B_0C0D, 0x0102_0304),
        op::X2EnterWorldPacket,
        &body,
    );
}

#[test]
fn finish_state_is_a_proxy_packet() {
    check(
        cs::finish_state(5),
        LEVEL_PROXY,
        pp::FinishStatePacket,
        &[5, 0, 0, 0],
    );
    check(
        cs::finish_state(-1),
        LEVEL_PROXY,
        pp::FinishStatePacket,
        &[0xFF; 4],
    );
}

#[test]
fn ping_repeats_the_clock_three_times() {
    let ms = [0x05, 0x04, 0x03, 0x02, 0x01, 0, 0, 0];
    let body = cat(&[&ms, &ms, &[0x05, 0x04, 0x03, 0x02]]);
    check(cs::ping(0x01_0203_0405), LEVEL_PROXY, pp::PingPacket, &body);
}

#[test]
fn list_characters() {
    game(cs::list_characters(), op::CSListCharacterPacket, &[0; 6]);
}

#[test]
fn create_character_with_the_default_face_block() {
    let out = cs::create_character(&cs::CreateCharacter {
        name: "Bob",
        race: 1,
        gender: 2,
        body_items: [0, 1, 2, 3, 4, 5, 6],
        ability1: 1,
    });
    let mut face = vec![3u8]; // UnitCustomModelType.Face
    face.extend(zeros(12 + 20 + 32 + 12 + 4 + 20)); // colors/model, decal, 4 assets, maps, weight, colors
    face.extend([0x80, 0x00]); // modifier: 128 bytes
    face.extend(zeros(128));
    assert_eq!(face.len(), 231);
    let body = cat(&[
        &[3, 0, b'B', b'o', b'b'],
        &[1, 2],
        &[0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0],
        &[4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0],
        &face,
        &[1, 11, 11, 1],
    ]);
    game(out, op::CSCreateCharacterPacket, &body);
}

#[test]
fn select_character() {
    game(
        cs::select_character(0x0403_0201),
        op::CSSelectCharacterPacket,
        &[1, 2, 3, 4, 0, 0],
    );
}

#[test]
fn spawn_character_sets_every_visual_option() {
    game(
        cs::spawn_character(),
        op::CSSpawnCharacterPacket,
        &[0x1F, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1],
    );
}

#[test]
fn notify_packets_are_empty() {
    game(cs::notify_in_game(), op::CSNotifyInGamePacket, &[]);
    game(
        cs::notify_in_game_completed(),
        op::CSNotifyInGameCompletedPacket,
        &[],
    );
}

#[test]
fn leave_world_targets() {
    game(
        cs::leave_world(cs::LEAVE_QUIT),
        op::CSLeaveWorldPacket,
        &[0],
    );
    game(
        cs::leave_world(cs::LEAVE_CHARACTER_SELECT),
        op::CSLeaveWorldPacket,
        &[1],
    );
    game(
        cs::leave_world(cs::LEAVE_SERVER_SELECT),
        op::CSLeaveWorldPacket,
        &[2],
    );
}

#[test]
fn instance_loaded_is_empty() {
    game(cs::instance_loaded(), op::CSInstanceLoadedPacket, &[]);
}

#[test]
fn teleport_ended_carries_the_position_and_an_identity_orientation() {
    let body = cat(&[&X_1, &Y_M2, &F_3_5, &[0; 12], &F_1_0]);
    assert_eq!(body.len(), 36);
    game(
        cs::teleport_ended(1.0, -2.0, 3.5),
        op::CSTeleportEndedPacket,
        &body,
    );
}

// ---- combat ---------------------------------------------------------------------------------

#[test]
fn start_skill_unit_on_unit() {
    game(
        cs::start_skill_unit(0x0102_0304, 0x0A_0B0C, 0x11_1213),
        op::CSStartSkillPacket,
        &[4, 3, 2, 1, 0, 0x0C, 0x0B, 0x0A, 0, 0x13, 0x12, 0x11, 0],
    );
}

#[test]
fn start_skill_unit_on_doodad() {
    game(
        cs::start_skill_doodad(0x0102_0304, 0x0A_0B0C, 0x11_1213),
        op::CSStartSkillPacket,
        &[4, 3, 2, 1, 0, 0x0C, 0x0B, 0x0A, 4, 0x13, 0x12, 0x11, 0],
    );
}

#[test]
fn start_skill_unit_on_position() {
    let body = cat(&[
        &[100, 0, 0, 0, 0, 1, 0, 0, 1], // skill, unit caster, position target type
        &X_1,
        &Y_M2,
        &F_3_5,
        &F_2_0,
        &[0; 6], // two bc
        &[0],    // flag
    ]);
    game(
        cs::start_skill_position(100, 1, (1.0, -2.0, 3.5), 2.0),
        op::CSStartSkillPacket,
        &body,
    );
}

#[test]
fn a_cannon_uses_a_mount_caster_and_position_target() {
    let body = cat(&[
        &10_473u32.to_le_bytes(),
        &[3, 0x0C, 0x0B, 0x0A],
        &16u32.to_le_bytes(),
        &[1],
        &X_1,
        &Y_M2,
        &F_3_5,
        &[0; 4],
        &[0; 6],
        &[0],
    ]);
    game(
        cs::start_mount_skill_position(10_473, 0x0A0B0C, 16, (1.0, -2.0, 3.5)),
        op::CSStartSkillPacket,
        &body,
    );
}

#[test]
fn start_skill_from_an_item() {
    let body = cat(&[
        &[7, 0, 0, 0],
        &[2, 1, 0, 0], // item caster, bc 1
        &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
        &[0xCC, 0xBB, 0xAA, 0x99],
        &[0],
        &[0, 0, 0, 0],
        &[0, 2, 0, 0], // unit target, bc 2
        &[0],
    ]);
    game(
        cs::start_skill_item(7, 1, 0x1122_3344_5566_7788, 0x99AA_BBCC, 2),
        op::CSStartSkillPacket,
        &body,
    );
}

#[test]
fn stop_casting() {
    game(
        cs::stop_casting(0x0102, 0x0304, 0x05_0607),
        op::CSStopCastingPacket,
        &[2, 1, 4, 3, 7, 6, 5],
    );
}

#[test]
fn small_skill_packets() {
    game(
        cs::change_target(0x03_0201),
        op::CSChangeTargetPacket,
        &[1, 2, 3],
    );
    game(cs::change_target(0), op::CSChangeTargetPacket, &[0, 0, 0]);
    game(
        cs::learn_skill(0x0102_0304),
        op::CSLearnSkillPacket,
        &[4, 3, 2, 1],
    );
    game(
        cs::swap_ability(0x03_0201, 3, 7),
        op::CSSwapAbilityPacket,
        &[1, 2, 3, 3, 7, 0],
    );
    game(cs::resurrect(true), op::CSResurrectCharacterPacket, &[1]);
    game(cs::resurrect(false), op::CSResurrectCharacterPacket, &[0]);
    game(cs::set_force_attack(true), op::CSSetForceAttackPacket, &[1]);
}

#[test]
fn npc_interaction() {
    game(
        cs::interact_npc(0x03_0201, true),
        op::CSInteractNPCPacket,
        &[1, 2, 3, 1],
    );
    let body = cat(&[
        &[1, 2, 3, 1, 2, 3], // npc twice
        &[0; 4],             // extra info
        &[0; 4],             // pick id
        &[1],                // mouse button
        &[0; 4],             // modifier keys
    ]);
    assert_eq!(body.len(), 19);
    game(
        cs::start_interaction(0x03_0201),
        op::CSStartInteractionPacket,
        &body,
    );
}

// ---- loot -----------------------------------------------------------------------------------

#[test]
fn loot() {
    game(
        cs::loot_open_bag(0x03_0201, true),
        op::CSLootOpenBagPacket,
        &[1, 2, 3, 0, 0, 0, 1],
    );
    game(
        cs::loot_item(2, cs::LOOT_OWNER_UNIT, 0x03_0201),
        op::CSLootItemPacket,
        &[2, 0, 1, 0, 1, 2, 3, 0, 0, 0, 0],
    );
    game(
        cs::loot_close_bag(0, cs::LOOT_OWNER_UNIT, 0x03_0201),
        op::CSLootCloseBagPacket,
        &[0, 0, 1, 0, 1, 2, 3, 0],
    );
}

// ---- chat -----------------------------------------------------------------------------------

#[test]
fn chat() {
    let whisper = cat(&[
        &[0xFD, 0xFF], // -3
        &[0, 0],
        &[0; 4],
        &[3, 0, b'B', b'o', b'b'],
        &[2, 0, b'h', b'i'],
        &[0],
        &[0; 4],
    ]);
    game(
        cs::chat(cs::chat_type::WHISPER, "Bob", "hi"),
        op::CSSendChatMessagePacket,
        &whisper,
    );
    let say = cat(&[
        &[0, 0, 0, 0],
        &[0; 4],
        &[0, 0],
        &[2, 0, b'/', b'x'],
        &[0],
        &[0; 4],
    ]);
    // the deliberate GM path is the only one that keeps a leading slash
    game(
        cs::gm_command("/x").expect("a command line"),
        op::CSSendChatMessagePacket,
        &say,
    );
    assert_eq!(cs::chat_type::SHOUT, 1);
    assert_eq!(cs::chat_type::PARTY, 4);
    // the server's ChatType numbers (systems/social/chat.rs)
    assert_eq!(cs::chat_type::TRADE, 2);
    assert_eq!(cs::chat_type::GROUP_FIND, 3);
    assert_eq!(cs::chat_type::REGION, 6);
    assert_eq!(cs::chat_type::ALLY, 14);
}

/// The message string of a built chat packet (the body is `i16, i16, i32, str target, str msg, ..`).
fn chat_message(out: &aa_proto::cs::Out) -> String {
    let b = &out.body;
    let tlen = u16::from_le_bytes([b[8], b[9]]) as usize;
    let at = 10 + tlen;
    let mlen = u16::from_le_bytes([b[at], b[at + 1]]) as usize;
    String::from_utf8(b[at + 2..at + 2 + mlen].to_vec()).unwrap()
}

#[test]
fn chat_never_carries_a_command() {
    for text in [
        "/x",
        "  /x",
        "\0/x",
        "\0\0 \t/gold add self 999",
        "\u{200b}/x",
        "\u{feff}\u{ad}/x",
        "\\x",
        "\u{2044}x",
        "\u{ff0f}shutdown",
        "/",
    ] {
        for kind in [
            cs::chat_type::SAY,
            cs::chat_type::WHISPER,
            cs::chat_type::SHOUT,
            cs::chat_type::TRADE,
            cs::chat_type::GROUP_FIND,
            cs::chat_type::PARTY,
            cs::chat_type::REGION,
            cs::chat_type::ALLY,
        ] {
            let out = cs::chat(kind, "Bob", text);
            let sent = chat_message(&out);
            assert!(!sent.is_empty(), "{text:?}");
            assert!(!cs::is_command_like(&sent), "{text:?} -> {sent:?}");
            assert!(!sent.chars().any(char::is_control), "{text:?} -> {sent:?}");
            assert!(!sent.starts_with('/'), "{text:?} -> {sent:?}");
            // and the strict builder refuses the same lines
            assert!(cs::try_chat(kind, "Bob", text).is_none(), "{text:?}");
        }
    }
    // the neutralised text keeps what followed the slash
    assert_eq!(
        chat_message(&cs::chat(0, "", "/hello there")),
        "hello there"
    );
    // ordinary lines are untouched (apart from the trim)
    assert_eq!(
        chat_message(&cs::chat(0, "", "  hi / there ")),
        "hi / there"
    );
    assert_eq!(chat_message(&cs::chat(0, "", "a/b")), "a/b");
    assert!(cs::try_chat(0, "", "hi").is_some());
    // overlong and multi-line text is cut or flattened, never sent as it was
    let long = "x".repeat(500);
    assert_eq!(chat_message(&cs::chat(0, "", &long)).chars().count(), 200);
    assert_eq!(chat_message(&cs::chat(0, "", "a\nb")), "ab");
    assert!(cs::try_chat(0, "", &long).is_none());
}

#[test]
fn gm_command_accepts_only_clean_slash_lines() {
    assert!(cs::gm_command("/level Bob 5").is_some());
    for bad in [
        "level Bob 5",
        " /level",
        "\0/level",
        "/level\n/gold add self 1",
        "/level\0",
        "\u{ff0f}level",
        "",
    ] {
        assert!(cs::gm_command(bad).is_none(), "{bad:?}");
    }
    assert!(cs::gm_command(&format!("/{}", "x".repeat(200))).is_none());
}

// ---- quests ---------------------------------------------------------------------------------

#[test]
fn quests() {
    game(
        cs::start_quest_context(0x0102_0304, 0x0A_0B0C, 0, 0x0D0E_0F10),
        op::CSStartQuestContextPacket,
        &[
            4, 3, 2, 1, 0x0C, 0x0B, 0x0A, 0, 0, 0, 0x10, 0x0F, 0x0E, 0x0D,
        ],
    );
    game(
        cs::quest_talk_made(0x0A_0B0C, 1000, 2000, 3000),
        op::CSQuestTalkMadePacket,
        &[
            0x0C, 0x0B, 0x0A, 0xE8, 3, 0, 0, 0xD0, 7, 0, 0, 0xB8, 0x0B, 0, 0,
        ],
    );
    game(
        cs::complete_quest_context(1000, 0x0A_0B0C, 0, -1),
        op::CSCompleteQuestContextPacket,
        &[
            0xE8, 3, 0, 0, 0x0C, 0x0B, 0x0A, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF,
        ],
    );
    game(
        cs::drop_quest_context(1000),
        op::CSDropQuestContextPacket,
        &[0xE8, 3, 0, 0],
    );
}

// ---- duel -----------------------------------------------------------------------------------

#[test]
fn duel() {
    game(
        cs::challenge_duel(0x0102_0304),
        op::CSChallengeDuelPacket,
        &[4, 3, 2, 1],
    );
    game(
        cs::start_duel(0x0102_0304, cs::DUEL_ACCEPT),
        op::CSStartDuelPacket,
        &[4, 3, 2, 1, 0, 0],
    );
    // 507 = 0x01FB
    game(
        cs::start_duel(0x0102_0304, cs::DUEL_DECLINE),
        op::CSStartDuelPacket,
        &[4, 3, 2, 1, 0xFB, 0x01],
    );
}

// ---- trade ----------------------------------------------------------------------------------

#[test]
fn trade() {
    game(
        cs::can_start_trade(0x03_0201),
        op::CSCanStartTradePacket,
        &[1, 2, 3],
    );
    game(
        cs::start_trade(0x03_0201),
        op::CSStartTradePacket,
        &[1, 2, 3],
    );
    game(
        cs::cannot_start_trade(0x03_0201, -1),
        op::CSCannotStartTradePacket,
        &[1, 2, 3, 0xFF, 0xFF, 0xFF, 0xFF],
    );
    game(
        cs::putup_trade_item(2, 5, 3),
        op::CSPutupTradeItemPacket,
        &[2, 5, 3, 0, 0, 0],
    );
    game(
        cs::takedown_trade_item(2, 5),
        op::CSTakedownTradeItemPacket,
        &[2, 5],
    );
    game(
        cs::putup_trade_money(0x0102_0304),
        op::CSPutupTradeMoneyPacket,
        &[4, 3, 2, 1],
    );
    game(cs::trade_lock(true), op::CSTradeLockPacket, &[1]);
    game(cs::trade_lock(false), op::CSTradeLockPacket, &[0]);
    game(cs::trade_ok(), op::CSTradeOkPacket, &[]);
    game(cs::cancel_trade(7), op::CSCancelTradePacket, &[7, 0, 0, 0]);
}

// ---- team -----------------------------------------------------------------------------------

#[test]
fn team() {
    game(
        cs::invite_to_team(0, true, "Bob"),
        op::CSInviteToTeamPacket,
        &[0, 0, 0, 0, 1, 3, 0, b'B', b'o', b'b'],
    );
    game(
        cs::reply_to_join_team(5, true, 0x0A0B_0C0D, false, "Al", false),
        op::CSReplyToJoinTeamPacket,
        &[
            5, 0, 0, 0, 1, 0x0D, 0x0C, 0x0B, 0x0A, 0, 2, 0, b'A', b'l', 0,
        ],
    );
    game(
        cs::ask_risky_team_action(5, 0x0A0B_0C0D, cs::RISKY_KICK),
        op::CSAskRiskyTeamActionPacket,
        &[5, 0, 0, 0, 0x0D, 0x0C, 0x0B, 0x0A, 1],
    );
    assert_eq!((cs::RISKY_LEAVE, cs::RISKY_DISMISS), (0, 2));
    game(
        cs::make_team_owner(5, 0x0A0B_0C0D),
        op::CSMakeTeamOwnerPacket,
        &[5, 0, 0, 0, 0x0D, 0x0C, 0x0B, 0x0A],
    );
    game(
        cs::set_team_member_role(5, 0x0A0B_0C0D, cs::team_role::HEALER),
        op::CSSetTeamMemberRolePacket,
        &[5, 0, 0, 0, 0x0D, 0x0C, 0x0B, 0x0A, 2],
    );
    assert_eq!(
        (
            cs::team_role::TANK,
            cs::team_role::HEALER,
            cs::team_role::ATTACKER
        ),
        (1, 2, 3)
    );
}

// ---- housing --------------------------------------------------------------------------------

#[test]
fn housing() {
    let tax = cat(&[&[4, 3, 2, 1], &X_1, &Y_M2, &F_3_5]);
    game(
        cs::construct_house_tax(0x0102_0304, (1.0, -2.0, 3.5)),
        op::CSConstructHouseTaxPacket,
        &tax,
    );
    let create = cat(&[
        &[4, 3, 2, 1],
        &X_1,
        &Y_M2,
        &F_3_5,
        &F_2_0, // z rotation
        &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
        &[0xFF; 4], // money -1
        &[0x0A, 0, 0, 0],
        &[1],
    ]);
    game(
        cs::create_house(&cs::CreateHouse {
            design_id: 0x0102_0304,
            pos: (1.0, -2.0, 3.5),
            z_rot: 2.0,
            item_id: 0x1122_3344_5566_7788,
            money: -1,
            ht: 10,
            auto_use_aa_point: true,
        }),
        op::CSCreateHousePacket,
        &create,
    );
    game(
        cs::request_house_tax(0x0102),
        op::CSRequestHouseTaxPacket,
        &[2, 1],
    );
}

// ---- economy --------------------------------------------------------------------------------

#[test]
fn economy() {
    game(
        cs::execute_craft(0x0102_0304, 0x0A_0B0C, 3),
        op::CSExecuteCraft,
        &[4, 3, 2, 1, 0x0C, 0x0B, 0x0A, 3, 0, 0, 0],
    );
    game(
        cs::sell_backpack_goods(0x03_0201),
        op::CSSellBackpackGoodsPacket,
        &[1, 2, 3],
    );
    game(cs::list_mail(), op::CSListMailPacket, &[]);
    game(
        cs::read_mail(true, 0x0102_0304_0506_0708),
        op::CSReadMailPacket,
        &[1, 8, 7, 6, 5, 4, 3, 2, 1],
    );
    game(
        cs::take_attachment_sequentially(-2),
        op::CSTakeAttachmentSequentially,
        &[0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
    );
}

#[test]
fn farming() {
    // an Onion Seed bought three at a time from NPC 0x030201
    let mut body = cat(&[&[1, 2, 3], &[0, 0, 0], &[0, 0, 0, 0], &[1, 0]]);
    body.extend([0x6F, 0x3D, 0, 0, 0, 3, 0, 0, 0, 0]);
    body.push(0); // use_aa_point
    game(
        cs::buy_items(
            0x03_0201,
            &[cs::BuyItem {
                item: 15_727,
                grade: 0,
                count: 3,
                currency: cs::SHOP_MONEY,
            }],
        ),
        op::CSBuyItemsPacket,
        &body,
    );
    // no goods: still a well-formed packet
    game(
        cs::buy_items(0x03_0201, &[]),
        op::CSBuyItemsPacket,
        &cat(&[&[1, 2, 3], &[0, 0, 0], &[0, 0, 0, 0], &[0, 0], &[0]]),
    );
    // plant doodad 2259 at (1, -2, 3.5), rotated 2.0, scale 1.0, with the bag item 0x0102030405060708
    game(
        cs::create_doodad(2259, (1.0, -2.0, 3.5), 2.0, 1.0, 0x0102_0304_0506_0708),
        op::CSCreateDoodadPacket,
        &cat(&[
            &[0xD3, 0x08, 0, 0],
            &X_1,
            &Y_M2,
            &F_3_5,
            &F_2_0,
            &F_1_0,
            &[8, 7, 6, 5, 4, 3, 2, 1],
        ]),
    );
    game(
        cs::request_common_farm_list(),
        op::CSRequestCommonFarmList,
        &[],
    );
}

#[test]
fn opcodes_have_names() {
    assert_eq!(cs::spawn_character().name(), "CSSpawnCharacterPacket");
    assert_eq!(cs::ping(0).name(), "PingPacket");
    assert_eq!(cs::execute_craft(1, 2, 3).name(), "CSExecuteCraft");
}
