//! Byte-exact tests of the auction packets: every expected body is written out by hand from the
//! server's writers and readers (`E/` = `aaemu-game/src/systems/economy/`), not produced by the
//! builders under test.

use super::*;
use crate::sc::{self, ServerEvent};
use crate::sc_activity::ActivityEvent;

/// The auctioneer object id 0x010203 as a `bc` (three bytes, little endian).
const NPC: [u8; 3] = [0x03, 0x02, 0x01];
/// A second one, 0x00AABB.
const NPC2: [u8; 3] = [0xBB, 0xAA, 0x00];

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// A `str`: `u16` length, then the bytes.
fn st(text: &str) -> Vec<u8> {
    let mut v = (text.len() as u16).to_le_bytes().to_vec();
    v.extend_from_slice(text.as_bytes());
    v
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn le64(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

#[track_caller]
fn check(out: crate::cs::Out, opcode: u16, body: &[u8]) {
    assert_eq!(out.level, LEVEL_GAME);
    assert_eq!(out.opcode, opcode, "opcode of {}", out.name());
    assert_eq!(out.body, body, "body of {}", out.name());
}

/// `Item.Write` (`models/item.rs:1061-1082`): template, id, grade, flags, count, detail type, the
/// detail body of `detail.1` bytes and the 33-byte tail.
fn item(template: u32, id: u64, grade: u8, flags: u8, count: i32, detail: (u8, usize)) -> Vec<u8> {
    if template == 0 {
        return le32(0).to_vec();
    }
    let mut v = le32(template).to_vec();
    v.extend_from_slice(&le64(id));
    v.extend_from_slice(&[grade, flags]);
    v.extend_from_slice(&count.to_le_bytes());
    v.push(detail.0);
    v.extend(std::iter::repeat_n(0xEE, detail.1));
    // create time, lifespan, made unit, world, unsecure time, unpack time
    v.extend_from_slice(&[0x11; 8]);
    v.extend_from_slice(&[0x22; 4]);
    v.extend_from_slice(&[0x33; 4]);
    v.push(1);
    v.extend_from_slice(&[0x44; 8]);
    v.extend_from_slice(&[0x55; 8]);
    v
}

/// `AuctionLot.Write` (`E/auction.rs:128-150`).
fn lot_bytes(
    id: u64,
    dur: u8,
    item: &[u8],
    seller: (u32, &str),
    start: i32,
    buyout: i32,
    bidder: (u8, u32, &str, i32),
) -> Vec<u8> {
    cat(&[
        &le64(id),
        &[dur],
        item,
        &[1],
        &le32(seller.0),
        &st(seller.1),
        &start.to_le_bytes(),
        &buyout.to_le_bytes(),
        &1_700_000_000i64.to_le_bytes(),
        &[bidder.0],
        &le32(bidder.1),
        &st(bidder.2),
        &bidder.3.to_le_bytes(),
        &0i32.to_le_bytes(),
    ])
}

fn sample_lot() -> Lot {
    Lot {
        id: 77,
        duration: duration::H48,
        template: 3411,
        item_id: 900_001,
        grade: 0,
        flags: 0,
        count: 20,
        world: 1,
        client_id: 5,
        client_name: "Seller".into(),
        start: 700,
        buyout: 900,
        post_date: 1_700_000_000,
        bid_world: 255,
        bidder_id: 0,
        bidder_name: "".into(),
        bid_money: 0,
        extra: 0,
    }
}

// ---- requests --------------------------------------------------------------------------------

#[test]
fn post_is_byte_exact() {
    // CSAuctionPostPacket (E/auction_packets.rs:67): bc, bc, u64 item, i32 start, i32 buyout, u8 duration
    check(
        post(
            0x010203,
            0x00AABB,
            0x1122_3344_5566_7788,
            700,
            900,
            duration::H48,
        ),
        0xB7,
        &cat(&[
            &NPC,
            &NPC2,
            &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
            &700i32.to_le_bytes(),
            &900i32.to_le_bytes(),
            &[3],
        ]),
    );
}

#[test]
fn search_is_byte_exact() {
    // AuctionSearch.Read (E/auction.rs:208): str keyword, bool exact, u8 grade, 3 x u8 category,
    // i32 page, u32 client id, i32 filter, u8 world, u8 min level, u8 max level, u8 sort kind, u8 order
    let q = Search {
        keyword: "Copper Ore",
        exact: true,
        grade: 1,
        categories: [7, 24, 36],
        page: 2,
        client_id: 0,
        levels: [1, 9],
        sort: [sort_kind::DIRECT_PRICE, sort_order::ASC],
    };
    check(
        search(0x010203, 0x010203, &q),
        0xB8,
        &cat(&[
            &NPC,
            &NPC,
            &st("Copper Ore"),
            &[1, 1, 7, 24, 36],
            &2i32.to_le_bytes(),
            &le32(0),
            &0i32.to_le_bytes(),
            &[0, 1, 9, 6, 0],
        ]),
    );
    // the own lots: no keyword, the character id
    let own = Search {
        client_id: 0x0000_1234,
        ..Search::default()
    };
    check(
        search(0x010203, 0x00AABB, &own),
        0xB8,
        &cat(&[
            &NPC,
            &NPC2,
            &st(""),
            &[0, 0, 0, 0, 0],
            &0i32.to_le_bytes(),
            &le32(0x1234),
            &0i32.to_le_bytes(),
            &[0, 0, 0, 0, 0],
        ]),
    );
}

/// The lot the server reads in bid and cancel (`ClientAuctionLot::read`, E/auction.rs:330): the
/// item is an empty one.
fn client_lot_bytes() -> Vec<u8> {
    cat(&[
        &le64(77),
        &[3],
        &le32(0),
        &[1],
        &le32(5),
        &st("Seller"),
        &700i32.to_le_bytes(),
        &900i32.to_le_bytes(),
        &1_700_000_000i64.to_le_bytes(),
        &[255],
        &le32(0),
        &st(""),
        &0i32.to_le_bytes(),
        &0i32.to_le_bytes(),
    ])
}

#[test]
fn bid_is_byte_exact() {
    // CSBidAuctionPacket (E/auction_packets.rs:151): bc, bc, AuctionLot, AuctionBid
    // (AuctionBid.Read, E/auction.rs:165: u64 lot, u8 world, u32 bidder, str name, i32 money)
    check(
        bid(0x010203, 0x010203, &sample_lot(), 0x4321, "Buyer", 900),
        0xB9,
        &cat(&[
            &NPC,
            &NPC,
            &client_lot_bytes(),
            &le64(77),
            &[1],
            &le32(0x4321),
            &st("Buyer"),
            &900i32.to_le_bytes(),
        ]),
    );
}

#[test]
fn cancel_and_the_small_requests_are_byte_exact() {
    // CSCancelAuctionPacket (E/auction_packets.rs:195): bc, bc, AuctionLot
    check(
        cancel(0x010203, 0x010203, &sample_lot()),
        0xBA,
        &cat(&[&NPC, &NPC, &client_lot_bytes()]),
    );
    // CSAuctionMyBidListPacket (E/auction_packets.rs:233): bc, bc, i32 page
    check(
        my_bid_list(0x010203, 0x00AABB, 3),
        0xBB,
        &cat(&[&NPC, &NPC2, &3i32.to_le_bytes()]),
    );
    // CSAuctionLowestPricePacket (E/auction_packets.rs:271): bc, bc, u32 template, u8 grade
    check(
        lowest_price(0x010203, 0x010203, 3411, 2),
        0xBC,
        &cat(&[&NPC, &NPC, &le32(3411), &[2]]),
    );
}

#[test]
fn a_bid_lot_never_carries_the_item_itself() {
    // The server reads the item of the lot with `WireItem::read`; a bare 0 template ends it. The
    // builder must never write the item (it would need the detail bytes of the item).
    let body = bid(1, 1, &sample_lot(), 1, "B", 1).body;
    let item_at = 6 + 8 + 1; // two bc, lot id, duration
    assert_eq!(&body[item_at..item_at + 4], &[0, 0, 0, 0]);
}

// ---- answers ---------------------------------------------------------------------------------

#[test]
fn a_posted_lot_is_read_through_its_item() {
    // SCAuctionPosted (E/auction_packets.rs:315) is one AuctionLot; the item carries an equipment
    // detail body (55 bytes) the parser has to skip
    let body = lot_bytes(
        77,
        3,
        &item(3411, 900_001, 2, 0x01, 20, (1, 55)),
        (5, "Seller"),
        700,
        900,
        (255, 0, "", 0),
    );
    let Some(AuctionEvent::Posted(lot)) = parse(1, 0x12E, &body).unwrap() else {
        panic!("not a posted lot");
    };
    assert_eq!(
        *lot,
        Lot {
            grade: 2,
            flags: 1,
            ..sample_lot()
        }
    );
    assert_eq!(lot.buyout_unit(), Some(45));
    assert!(!lot.has_bid());
    // a lot without items (the server lost the item) is read as an empty item
    let body = lot_bytes(
        9,
        0,
        &item(0, 0, 0, 0, 0, (0, 0)),
        (5, "S"),
        0,
        0,
        (255, 0, "", 0),
    );
    let Some(AuctionEvent::Posted(lot)) = parse(1, 0x12E, &body).unwrap() else {
        panic!("not a lot");
    };
    assert_eq!(
        (lot.id, lot.template, lot.count, lot.buyout_unit()),
        (9, 0, 0, None)
    );
}

#[test]
fn a_searched_page_with_lots_an_error_and_the_time() {
    // SCAuctionSearched (E/auction_packets.rs:335): i32 page, i32 count, lots, i16 error, i64 time
    let a = lot_bytes(
        1,
        3,
        &item(3411, 11, 0, 0, 5, (0, 0)),
        (5, "Ann"),
        10,
        100,
        (255, 0, "", 0),
    );
    let b = lot_bytes(
        2,
        2,
        &item(100, 12, 3, 0, 1, (1, 55)),
        (6, "Bob"),
        0,
        0,
        (1, 8, "Cy", 50),
    );
    let body = cat(&[
        &1i32.to_le_bytes(),
        &2i32.to_le_bytes(),
        &a,
        &b,
        &0i16.to_le_bytes(),
        &1_700_000_123i64.to_le_bytes(),
    ]);
    let Some(AuctionEvent::Searched {
        page,
        lots,
        error,
        server_time,
    }) = parse(1, 0x12F, &body).unwrap()
    else {
        panic!("not a page");
    };
    assert_eq!((page, error, server_time), (1, 0, 1_700_000_123));
    assert_eq!(lots.len(), 2);
    assert_eq!((lots[0].id, lots[0].template, lots[0].count), (1, 3411, 5));
    assert_eq!(lots[0].buyout_unit(), Some(20));
    assert_eq!(
        (lots[1].template, lots[1].grade, lots[1].buyout),
        (100, 3, 0)
    );
    assert!(lots[1].has_bid() && &*lots[1].bidder_name == "Cy" && lots[1].bid_money == 50);
    assert_eq!(lots[1].buyout_unit(), None);
}

#[test]
fn an_empty_page_and_a_corrupt_one() {
    // no lots at all comes as page 0, count 0 (E/auction.rs:1090-1099)
    let empty = cat(&[
        &0i32.to_le_bytes(),
        &0i32.to_le_bytes(),
        &0i16.to_le_bytes(),
        &5i64.to_le_bytes(),
    ]);
    assert!(matches!(
        parse(1, 0x12F, &empty).unwrap(),
        Some(AuctionEvent::Searched { page: 0, ref lots, .. }) if lots.is_empty()
    ));
    // a count that cannot fit the body is an error, not an allocation
    let huge = cat(&[&0i32.to_le_bytes(), &i32::MAX.to_le_bytes(), &[0; 10]]);
    assert!(parse(1, 0x12F, &huge).is_err());
    let negative = cat(&[&0i32.to_le_bytes(), &(-1i32).to_le_bytes(), &[0; 10]]);
    assert!(parse(1, 0x12F, &negative).is_err());
    // a truncated lot is an error
    let mut cut = lot_bytes(
        1,
        0,
        &item(3411, 1, 0, 0, 1, (0, 0)),
        (1, "A"),
        0,
        0,
        (255, 0, "", 0),
    );
    cut.truncate(cut.len() - 3);
    assert!(parse(1, 0x12E, &cut).is_err());
}

#[test]
fn bid_canceled_and_lowest_price_answers() {
    // SCAuctionBid (E/auction_packets.rs:359): AuctionBid.Write, bool is_buyout, u32 template
    let body = cat(&[
        &le64(77),
        &[1],
        &le32(0x4321),
        &st("Buyer"),
        &900i32.to_le_bytes(),
        &[1],
        &le32(3411),
    ]);
    assert_eq!(
        parse(1, 0x131, &body).unwrap(),
        Some(AuctionEvent::Bid {
            lot_id: 77,
            bidder_id: 0x4321,
            bidder_name: "Buyer".into(),
            money: 900,
            buyout: true,
            template: 3411,
        })
    );
    // SCAuctionCanceled (E/auction_packets.rs:376) is one lot
    let body = lot_bytes(
        77,
        3,
        &item(3411, 900_001, 0, 0, 20, (0, 0)),
        (5, "Seller"),
        700,
        900,
        (255, 0, "", 0),
    );
    let Some(AuctionEvent::Canceled(lot)) = parse(1, 0x132, &body).unwrap() else {
        panic!("not a cancel");
    };
    assert_eq!(*lot, sample_lot());
    // SCAuctionLowestPrice (E/auction_packets.rs:394): u32 template, u8 grade, i32 money
    let body = cat(&[&le32(3411), &[0], &1500i32.to_le_bytes()]);
    assert_eq!(
        parse(1, 0x130, &body).unwrap(),
        Some(AuctionEvent::LowestPrice {
            template: 3411,
            grade: 0,
            money: 1500
        })
    );
}

#[test]
fn other_packets_and_levels_are_not_auction_events() {
    assert_eq!(parse(1, 0x133, &[]).unwrap(), None);
    assert_eq!(parse(1, 0x001, &[0; 40]).unwrap(), None);
    // only the plain game level
    let body = cat(&[&le32(3411), &[0], &1500i32.to_le_bytes()]);
    assert_eq!(parse(2, 0x130, &body).unwrap(), None);
}

#[test]
fn the_server_event_parser_routes_the_answers() {
    let body = cat(&[&le32(3411), &[0], &1500i32.to_le_bytes()]);
    let ev = sc::parse(1, 0x130, &body).unwrap();
    assert!(matches!(
        ev,
        Some(ServerEvent::Activity(ActivityEvent::Auction(a)))
            if matches!(*a, AuctionEvent::LowestPrice { money: 1500, .. })
    ));
}

// ---- the fee ---------------------------------------------------------------------------------

#[test]
fn the_listing_fee_follows_the_server() {
    // The percentage is rounded, with a minimum charge and an upper cap.
    assert_eq!(listing_fee(900, duration::H48), 36);
    assert_eq!(listing_fee(945, duration::H48), 38);
    assert_eq!(listing_fee(1000, duration::H6), 10);
    assert_eq!(listing_fee(1000, duration::H12), 20);
    assert_eq!(listing_fee(1000, duration::H24), 30);
    assert_eq!(listing_fee(1000, duration::H48), 40);
    assert_eq!(listing_fee(0, duration::H48), 1);
    assert_eq!(listing_fee(1, duration::H6), 1);
    assert_eq!(listing_fee(100_000_000, duration::H48), MAX_LISTING_FEE);
    assert_eq!(listing_fee(i32::MAX, duration::H6), MAX_LISTING_FEE);
}

#[test]
fn auction_mail_types() {
    for t in 13..=17 {
        assert!(mail_type::is_auction(t));
    }
    for t in [1, 12, 18, 19] {
        assert!(!mail_type::is_auction(t));
    }
}
