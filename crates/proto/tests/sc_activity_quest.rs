//! `sc_activity` quest and duel packets: every body is built in the order the AAEmu server
//! writes it (writer citations are in the variant docs of `ActivityEvent`).

use aa_proto::opcodes::sc;
use aa_proto::sc_activity::{duel_det, parse, quest_status, ActivityEvent, QuestRow};
use aa_proto::stream::MarshalError;
use aa_proto::PacketStream;

/// `Quest.Write` (G/models/char_data.rs:458-482).
#[allow(clippy::too_many_arguments)]
fn write_quest_row(
    s: &mut PacketStream,
    id: i64,
    template: u32,
    status: u8,
    objectives: [i32; 5],
    left_time: i32,
    component: u32,
    acceptor_type: u8,
    acceptor_id: u32,
) {
    s.write_i64(id).write_u32(template).write_u8(status);
    for o in objectives {
        s.write_i32(o);
    }
    s.write_bool(false) // isCheckSet
        .write_bc(0) // ObjId
        .write_u32(0) // type(id)
        .write_bc(0) // ObjId
        .write_bc(0) // ObjId
        .write_i32(left_time)
        .write_u32(component)
        .write_i64(0) // DoodadId
        .write_i64(1_700_000_000) // acceptTime
        .write_u8(acceptor_type)
        .write_u32(acceptor_id);
}

fn row_a() -> QuestRow {
    QuestRow {
        id: 0x1_0000_0001,
        template: 4007,
        status: quest_status::PROGRESS,
        objectives: [3, 0, 0, 0, 0],
        left_time_ms: -1,
        component: 0,
        acceptor_type: 1,
        acceptor_id: 123,
    }
}

fn row_b() -> QuestRow {
    QuestRow {
        id: 12,
        template: 5001,
        status: quest_status::READY,
        objectives: [1, 2, 3, 4, 5],
        left_time_ms: 60_000,
        component: 77,
        acceptor_type: 2,
        acceptor_id: 9,
    }
}

fn put_row(s: &mut PacketStream, r: &QuestRow) {
    write_quest_row(
        s,
        r.id,
        r.template,
        r.status,
        r.objectives,
        r.left_time_ms,
        r.component,
        r.acceptor_type,
        r.acceptor_id,
    );
}

fn event(opcode: u16, s: &PacketStream) -> ActivityEvent {
    parse(1, opcode, s.as_bytes())
        .expect("layout matches")
        .expect("opcode is handled")
}

#[test]
fn quests_lists_the_rows() {
    // SCQuestsPacket: i32 n, Quest.Write x n (G/packets/world.rs:123).
    let mut s = PacketStream::new();
    s.write_i32(2);
    put_row(&mut s, &row_a());
    put_row(&mut s, &row_b());
    assert_eq!(s.len(), 4 + 2 * 76);
    assert_eq!(
        event(sc::SCQuestsPacket, &s),
        ActivityEvent::Quests {
            list: vec![row_a(), row_b()]
        }
    );
}

#[test]
fn quests_rejects_bad_counts() {
    for n in [-1, i32::MAX, 1] {
        let mut s = PacketStream::new();
        s.write_i32(n);
        assert!(
            parse(1, sc::SCQuestsPacket, s.as_bytes()).is_err(),
            "count {n} without rows"
        );
    }
    let mut s = PacketStream::new();
    s.write_i32(0);
    assert_eq!(
        event(sc::SCQuestsPacket, &s),
        ActivityEvent::Quests { list: vec![] }
    );
}

#[test]
fn completed_quests_expand_the_block_masks() {
    // SCCompletedQuestsPacket: i32 n, (u16 block id, 8 byte mask) x n (G/packets/world.rs:142).
    let mut s = PacketStream::new();
    s.write_i32(3);
    // block 1: bits 0 and 63 -> quests 64 and 127 (little-endian bit order, bit_get in
    // G/systems/quests/engine.rs:2286)
    s.write_u16(1).write_raw(&[0x01, 0, 0, 0, 0, 0, 0, 0x80]);
    // block 0: empty
    s.write_u16(0).write_raw(&[0; 8]);
    // block 4096: bits 5 and 8 -> quests 262149 and 262152
    s.write_u16(4096).write_raw(&[0x20, 0x01, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        event(sc::SCCompletedQuestsPacket, &s),
        ActivityEvent::CompletedQuests {
            ids: vec![64, 127, 262_149, 262_152]
        }
    );
}

#[test]
fn completed_quests_reject_a_count_beyond_the_body() {
    let mut s = PacketStream::new();
    s.write_i32(2);
    s.write_u16(1).write_raw(&[0xff; 8]);
    assert!(parse(1, sc::SCCompletedQuestsPacket, s.as_bytes()).is_err());
}

#[test]
fn quest_started() {
    // SCQuestContextStartedPacket: Quest.Write, u32 component (G/systems/quests/packets.rs:35).
    let mut s = PacketStream::new();
    put_row(&mut s, &row_a());
    s.write_u32(400_701);
    assert_eq!(
        event(sc::SCQuestContextStartedPacket, &s),
        ActivityEvent::QuestStarted {
            quest: Box::new(row_a()),
            component: 400_701,
        }
    );
}

#[test]
fn quest_updated() {
    // SCQuestContextUpdatedPacket: Quest.Write, u32 component, 4 x i32 (packets.rs:65).
    let mut s = PacketStream::new();
    put_row(&mut s, &row_b());
    s.write_u32(500_102)
        .write_i32(1)
        .write_i32(-2)
        .write_i32(3)
        .write_i32(4);
    assert_eq!(
        event(sc::SCQuestContextUpdatedPacket, &s),
        ActivityEvent::QuestUpdated {
            quest: Box::new(row_b()),
            component: 500_102,
            p: [1, -2, 3, 4],
        }
    );
}

#[test]
fn quest_completed() {
    // SCQuestContextCompletedPacket: u32 quest, 8 byte block mask, u32 component (packets.rs:87).
    let mut s = PacketStream::new();
    s.write_u32(4007)
        .write_raw(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10])
        .write_u32(0);
    assert_eq!(
        event(sc::SCQuestContextCompletedPacket, &s),
        ActivityEvent::QuestCompleted {
            quest_id: 4007,
            component: 0,
            block_mask: 0x1000_0000_0000_0000,
        }
    );
}

#[test]
fn quest_reset() {
    // SCQuestContextResetPacket: u32 quest, 8 byte block mask, u32 block id (packets.rs:107).
    let mut s = PacketStream::new();
    s.write_u32(4007)
        .write_raw(&[0x01, 0x02, 0, 0, 0, 0, 0, 0])
        .write_u32(62);
    assert_eq!(
        event(sc::SCQuestContextResetPacket, &s),
        ActivityEvent::QuestReset {
            quest_id: 4007,
            block_id: 62,
            block_mask: 0x0201,
        }
    );
}

#[test]
fn quest_failed() {
    // SCQuestContextFailedPacket: u32 quest, u8 err (packets.rs:145).
    let mut s = PacketStream::new();
    s.write_u32(4007).write_u8(12);
    assert_eq!(
        event(sc::SCQuestContextFailedPacket, &s),
        ActivityEvent::QuestFailed {
            quest_id: 4007,
            err: 12,
        }
    );
}

#[test]
fn quest_unit_req_failed_reads_only_the_flags_that_are_set() {
    // SCQuestUnitReqFailedPacket: u32 quest, bc unit, flags, then kind (0x01), message id
    // (0x02) and value (0x04) in that order (packets.rs:177-206).
    let mut s = PacketStream::new();
    s.write_u32(1137).write_bc(0x0A_0B0C).write_u8(0);
    assert_eq!(
        event(sc::SCQuestUnitReqFailedPacket, &s),
        ActivityEvent::QuestUnitReqFailed {
            quest_id: 1137,
            unit_obj: 0x0A_0B0C,
            kind: 0,
            message_id: 0,
            value: 0,
        }
    );

    let mut s = PacketStream::new();
    s.write_u32(1137).write_bc(0x0A_0B0C).write_u8(0x05);
    s.write_u8(0x25) // precomplete quest context
        .write_u32(1136);
    assert_eq!(
        event(sc::SCQuestUnitReqFailedPacket, &s),
        ActivityEvent::QuestUnitReqFailed {
            quest_id: 1137,
            unit_obj: 0x0A_0B0C,
            kind: 0x25,
            message_id: 0,
            value: 1136,
        }
    );

    let mut s = PacketStream::new();
    s.write_u32(7).write_bc(9).write_u8(0x07);
    s.write_u8(3).write_u16(0x1234).write_u32(0x5678_9ABC);
    assert_eq!(
        event(sc::SCQuestUnitReqFailedPacket, &s),
        ActivityEvent::QuestUnitReqFailed {
            quest_id: 7,
            unit_obj: 9,
            kind: 3,
            message_id: 0x1234,
            value: 0x5678_9ABC,
        }
    );

    // Only the message id.
    let mut s = PacketStream::new();
    s.write_u32(7).write_bc(9).write_u8(0x02).write_u16(42);
    assert_eq!(
        event(sc::SCQuestUnitReqFailedPacket, &s),
        ActivityEvent::QuestUnitReqFailed {
            quest_id: 7,
            unit_obj: 9,
            kind: 0,
            message_id: 42,
            value: 0,
        }
    );

    // A part the flags promise but the body does not have is refused.
    let mut s = PacketStream::new();
    s.write_u32(7).write_bc(9).write_u8(0x04).write_u16(1);
    assert!(parse(1, sc::SCQuestUnitReqFailedPacket, s.as_bytes()).is_err());
}

#[test]
fn quest_rewarded_by_mail() {
    // SCQuestRewardedByMailPacket: u8 n, u32 x n (packets.rs:161).
    let mut s = PacketStream::new();
    s.write_u8(2).write_u32(4007).write_u32(4008);
    match event(sc::SCQuestRewardedByMailPacket, &s) {
        ActivityEvent::QuestRewardedByMail { quests } => {
            assert_eq!(quests.as_slice(), [4007, 4008])
        }
        other => panic!("unexpected {other:?}"),
    }
    // A count beyond the body is refused.
    let mut s = PacketStream::new();
    s.write_u8(3).write_u32(1);
    assert!(parse(1, sc::SCQuestRewardedByMailPacket, s.as_bytes()).is_err());
}

#[test]
fn doodad_quest_accept() {
    // SCDoodadQuestAcceptPacket: bc doodad, u32 quest (packets.rs:181).
    let mut s = PacketStream::new();
    s.write_bc(0x0A_0B0C).write_u32(4007);
    assert_eq!(
        event(sc::SCDoodadQuestAcceptPacket, &s),
        ActivityEvent::DoodadQuestAccept {
            doodad_obj: 0x0A_0B0C,
            quest_id: 4007,
        }
    );
}

#[test]
fn duel_challenged() {
    // SCDuelChallengedPacket: u32 challenger (G/systems/social/packets/duel.rs:12).
    let mut s = PacketStream::new();
    s.write_u32(0x0102_0304);
    assert_eq!(
        event(sc::SCDuelChallengedPacket, &s),
        ActivityEvent::DuelChallenged {
            challenger_char_id: 0x0102_0304
        }
    );
}

#[test]
fn duel_start_countdown_has_no_body() {
    assert_eq!(
        parse(1, sc::SCDuelStartCountdownPacket, &[]).unwrap(),
        Some(ActivityEvent::DuelStartCountdown)
    );
}

#[test]
fn duel_started() {
    // SCDuelStartedPacket: bc challenger, bc challenged (duel.rs:42).
    let mut s = PacketStream::new();
    s.write_bc(0x11).write_bc(0x22_2222);
    assert_eq!(
        event(sc::SCDuelStartedPacket, &s),
        ActivityEvent::DuelStarted {
            challenger_obj: 0x11,
            challenged_obj: 0x22_2222,
        }
    );
}

#[test]
fn duel_ended() {
    // SCDuelEndedPacket: u32, u32, bc, bc, u8 det; the byte-exact body of the server's own test
    // (duel.rs:112-128).
    let body = [1u8, 0, 0, 0, 2, 0, 0, 0, 1, 2, 3, 0x10, 0, 0, 3];
    assert_eq!(
        parse(1, sc::SCDuelEndedPacket, &body).unwrap(),
        Some(ActivityEvent::DuelEnded {
            winner_id: 1,
            loser_id: 2,
            winner_obj: 0x03_0201,
            loser_obj: 0x10,
            det: duel_det::DRAW,
        })
    );
    let mut s = PacketStream::new();
    s.write_u32(7)
        .write_u32(8)
        .write_bc(70)
        .write_bc(80)
        .write_u8(duel_det::SURRENDER);
    assert_eq!(
        event(sc::SCDuelEndedPacket, &s),
        ActivityEvent::DuelEnded {
            winner_id: 7,
            loser_id: 8,
            winner_obj: 70,
            loser_obj: 80,
            det: duel_det::SURRENDER,
        }
    );
}

#[test]
fn duel_state() {
    // SCDuelStatePacket: bc unit, bc flag (duel.rs:49).
    let mut s = PacketStream::new();
    s.write_bc(0x11).write_bc(0);
    assert_eq!(
        event(sc::SCDuelStatePacket, &s),
        ActivityEvent::DuelState {
            obj: 0x11,
            flag_obj: 0,
        }
    );
}

#[test]
fn truncated_bodies_fail_and_never_panic() {
    let mut quest = PacketStream::new();
    put_row(&mut quest, &row_a());
    quest.write_u32(1);
    let mut list = PacketStream::new();
    list.write_i32(1);
    put_row(&mut list, &row_a());
    let mut ended = PacketStream::new();
    ended
        .write_u32(1)
        .write_u32(2)
        .write_bc(3)
        .write_bc(4)
        .write_u8(1);
    let mut completed = PacketStream::new();
    completed.write_u32(1).write_raw(&[0; 8]).write_u32(0);
    let samples: [(u16, &PacketStream); 4] = [
        (sc::SCQuestContextStartedPacket, &quest),
        (sc::SCQuestsPacket, &list),
        (sc::SCDuelEndedPacket, &ended),
        (sc::SCQuestContextCompletedPacket, &completed),
    ];
    for (opcode, full) in samples {
        assert!(parse(1, opcode, full.as_bytes()).unwrap().is_some());
        for end in 0..full.len() {
            let r = parse(1, opcode, &full.as_bytes()[..end]);
            assert!(
                matches!(r, Err(MarshalError::EndOfStream { .. })),
                "opcode {opcode:#x} cut at {end}: {r:?}"
            );
        }
    }
}
