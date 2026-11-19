//! The auction house packets: the six `CSAuction*` requests a bot sends and the five `SCAuction*`
//! answers it reads.
//!
//! All of them are level-1 (`LEVEL_PLAIN`) packets. Paths in the doc comments are relative to
//! `AAEmu-1.2-Rust/rust/crates/aaemu-game/src/` (`E/` = `systems/economy/`).
//!
//! Every request starts with two `bc` object ids (`auctioneer_id`, `auctioneer_id2`); the server
//! accepts the packet when one of them is an NPC with the auctioneer flag within 5 m of the
//! character, or the character is interacting with such an NPC (`E/auction_packets.rs:32-47`);
//! otherwise it answers error 388 ([`ERR_AUCTIONEER_TOO_FAR`]). Most other refusals are silent.
//!
//! ```text
//! lot  = u64 id, u8 duration, Item.Write, u8 world, u32 client_id, str client_name, i32 start,
//!        i32 buyout, i64 post_date, u8 bid_world, u32 bidder_id, str bidder_name, i32 bid_money,
//!        i32 extra                                          (E/auction.rs:126-150, `AuctionLot.Write`)
//! ```
//!
//! [`parse`] turns the answers into an [`AuctionEvent`]; the request builders return an [`Out`].

use crate::game::LEVEL_GAME;
use crate::opcodes::{cs, sc};
use crate::sc_activity::read_item;
use crate::stream::{MarshalError, PacketStream};

/// `ErrorMessageType` of "no auctioneer within reach" (`E/error_message.rs:390`).
pub const ERR_AUCTIONEER_TOO_FAR: i16 = 388;
/// `ErrorMessageType` of "the listing fee cannot be paid or the item cannot be put up"
/// (`E/error_message.rs:243`).
pub const ERR_CANNOT_PUT_UP: i16 = 241;

/// Lots per result page (`E/auction.rs:44`, `PAGE_SIZE`).
pub const PAGE_SIZE: usize = 9;
/// The most the server charges for listing (copper, `E/auction.rs:41`, 100 gold).
pub const MAX_LISTING_FEE: i32 = 1_000_000;

/// The mail types of the auction results (`E/mail.rs:53-57`, `MailType`): what a bot finds in its
/// mailbox after a lot ended.
pub mod mail_type {
    /// The seller cancelled the lot: the item is attached.
    pub const OFF_CANCEL: u8 = 13;
    /// The lot sold: the seller's share (90% of the price) is attached as money.
    pub const OFF_SUCCESS: u8 = 14;
    /// The lot ended unsold (the item is attached), or a bidder was outbid (the bid is attached
    /// as money, no item).
    pub const OFF_FAIL: u8 = 15;
    /// The bidder won: the item is attached.
    pub const BID_WIN: u8 = 16;
    /// Defined by the server but never sent for auctions.
    pub const BID_FAIL: u8 = 17;

    /// Whether `t` is one of the auction result mails.
    pub fn is_auction(t: u8) -> bool {
        (OFF_CANCEL..=BID_FAIL).contains(&t)
    }
}

/// `AuctionDuration` (`E/auction.rs:47-52`): 6, 12, 24 and 48 hours.
pub mod duration {
    /// 6 hours.
    pub const H6: u8 = 0;
    /// 12 hours.
    pub const H12: u8 = 1;
    /// 24 hours.
    pub const H24: u8 = 2;
    /// 48 hours.
    pub const H48: u8 = 3;
}

/// `AuctionSearchSortKind` (`E/auction.rs:55-63`).
pub mod sort_kind {
    /// Unsorted (id order).
    pub const DEFAULT: u8 = 0;
    /// By the buyout price.
    pub const DIRECT_PRICE: u8 = 6;
}

/// `AuctionSearchSortOrder` (`E/auction.rs:66-69`).
pub mod sort_order {
    /// Ascending.
    pub const ASC: u8 = 0;
    /// Descending.
    pub const DESC: u8 = 1;
}

/// The deposit for a listing's price, using the buyout when one is offered or the starting bid.
/// The server rounds the percentage to the nearest copper and charges at least one copper.
pub fn listing_fee(price: i32, duration: u8) -> i32 {
    let rate = f64::from(i32::from(duration) + 1);
    let fee = (f64::from(price) * 0.01 * rate + 0.5).floor();
    (fee as i32).clamp(1, MAX_LISTING_FEE)
}

/// One lot as `AuctionLot.Write` writes it (`E/auction.rs:126-150`). The item is reduced to the
/// leading fields of `Item.Write`; an empty item (template 0) is a lot whose item the server lost.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Lot {
    /// Lot id.
    pub id: u64,
    /// [`duration`] value.
    pub duration: u8,
    /// Item template (0 = no item).
    pub template: u32,
    /// Item instance id.
    pub item_id: u64,
    /// Item grade.
    pub grade: u8,
    /// `ItemFlag` bits of the item.
    pub flags: u8,
    /// Stack size.
    pub count: i32,
    /// World id of the seller (1 on this server).
    pub world: u8,
    /// Seller's character id.
    pub client_id: u32,
    /// Seller's name.
    pub client_name: Box<str>,
    /// Starting bid in copper.
    pub start: i32,
    /// Buyout price in copper (0 = none).
    pub buyout: i32,
    /// Unix seconds the lot was posted.
    pub post_date: i64,
    /// World id of the highest bidder (255 = none).
    pub bid_world: u8,
    /// Highest bidder's character id (0 = none).
    pub bidder_id: u32,
    /// Highest bidder's name (empty = none).
    pub bidder_name: Box<str>,
    /// Highest bid in copper.
    pub bid_money: i32,
    /// `AuctionLot.Extra`.
    pub extra: i32,
}

impl Lot {
    /// Copper per item of the buyout price, rounded up; `None` for a lot without a buyout or
    /// without items.
    pub fn buyout_unit(&self) -> Option<u32> {
        if self.buyout <= 0 || self.count <= 0 {
            return None;
        }
        let (b, c) = (self.buyout as u32, self.count as u32);
        Some(b.div_ceil(c))
    }

    /// Whether somebody has bid on the lot.
    pub fn has_bid(&self) -> bool {
        self.bidder_id != 0 || !self.bidder_name.is_empty()
    }
}

/// The reduced `AuctionLot.Write`: reads one lot.
fn read_lot(s: &mut PacketStream) -> Result<Lot, MarshalError> {
    let id = s.read_u64()?;
    let duration = s.read_u8()?;
    let item = read_item(s)?;
    Ok(Lot {
        id,
        duration,
        template: item.template_id,
        item_id: item.id,
        grade: item.grade,
        flags: item.flags,
        count: item.count,
        world: s.read_u8()?,
        client_id: s.read_u32()?,
        client_name: s.read_string()?.into_boxed_str(),
        start: s.read_i32()?,
        buyout: s.read_i32()?,
        post_date: s.read_i64()?,
        bid_world: s.read_u8()?,
        bidder_id: s.read_u32()?,
        bidder_name: s.read_string()?.into_boxed_str(),
        bid_money: s.read_i32()?,
        extra: s.read_i32()?,
    })
}

/// The smallest lot on the wire: an empty item (`u32 0`) and two empty strings.
const MIN_LOT_LEN: usize = 8 + 1 + 4 + 1 + 4 + 2 + 4 + 4 + 8 + 1 + 4 + 2 + 4 + 4;

/// An answer of the auction house.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuctionEvent {
    /// `SCAuctionPosted` (0x12E): the lot was accepted, the fee taken, the item put in escrow.
    /// One lot (`E/auction_packets.rs:315`).
    Posted(Box<Lot>),
    /// `SCAuctionSearched` (0x12F): one result page. `i32 page, i32 count, count lots,
    /// i16 error, i64 unix seconds` (`E/auction_packets.rs:335`). An empty result, and a page
    /// beyond the last, come as `count == 0`.
    Searched {
        /// The page this answer is for.
        page: i32,
        /// The lots of the page (at most [`PAGE_SIZE`]).
        lots: Vec<Lot>,
        /// The error field (0 = none).
        error: i16,
        /// The server's clock, unix seconds.
        server_time: i64,
    },
    /// `SCAuctionBid` (0x131): the bid or buyout was accepted and the money taken. `AuctionBid.Write`
    /// (`u64 lot, u8 world, u32 bidder, str name, i32 money`), `bool is_buyout`, `u32 item template`
    /// (`E/auction_packets.rs:359`, `E/auction.rs:178`).
    Bid {
        /// Lot id.
        lot_id: u64,
        /// Bidder's character id.
        bidder_id: u32,
        /// Bidder's name.
        bidder_name: Box<str>,
        /// Copper bid (the buyout price for a buyout).
        money: i32,
        /// The lot was bought out.
        buyout: bool,
        /// Template of the item.
        template: u32,
    },
    /// `SCAuctionCanceled` (0x132): the seller's lot was cancelled; the item comes back by mail.
    /// One lot (`E/auction_packets.rs:376`).
    Canceled(Box<Lot>),
    /// `SCAuctionLowestPrice` (0x130): `u32 template, u8 grade, i32 copper` (`E/auction_packets.rs:394`).
    /// Not reliable (the server answers with the lowest buyout of any grade, 0 without lots).
    LowestPrice {
        /// Item template.
        template: u32,
        /// Grade asked for.
        grade: u8,
        /// Lowest buyout of the template in copper, 0 for none.
        money: i32,
    },
}

/// Parses one S→C auction packet. `Ok(None)` when the packet is not one of the auction answers.
pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Result<Option<AuctionEvent>, MarshalError> {
    if level != LEVEL_GAME {
        return Ok(None);
    }
    let mut s = PacketStream::from_bytes(body);
    let s = &mut s;
    let ev = match opcode {
        sc::SCAuctionPostedPacket => AuctionEvent::Posted(Box::new(read_lot(s)?)),
        sc::SCAuctionCanceledPacket => AuctionEvent::Canceled(Box::new(read_lot(s)?)),
        sc::SCAuctionSearchedPacket => {
            let page = s.read_i32()?;
            let n = s.read_i32()?;
            let n = usize::try_from(n)
                .map_err(|_| MarshalError::Invalid(format!("negative lot count {n}")))?;
            // (a corrupt count never drives an allocation: every lot is at least MIN_LOT_LEN)
            if n.saturating_mul(MIN_LOT_LEN) > s.remaining() {
                return Err(MarshalError::EndOfStream {
                    pos: s.pos(),
                    wanted: n.saturating_mul(MIN_LOT_LEN),
                    len: s.len(),
                });
            }
            let mut lots = Vec::with_capacity(n);
            for _ in 0..n {
                lots.push(read_lot(s)?);
            }
            AuctionEvent::Searched {
                page,
                lots,
                error: s.read_i16()?,
                server_time: s.read_i64()?,
            }
        }
        sc::SCAuctionBidPacket => {
            let lot_id = s.read_u64()?;
            let _world = s.read_u8()?;
            let bidder_id = s.read_u32()?;
            let bidder_name = s.read_string()?.into_boxed_str();
            let money = s.read_i32()?;
            AuctionEvent::Bid {
                lot_id,
                bidder_id,
                bidder_name,
                money,
                buyout: s.read_bool()?,
                template: s.read_u32()?,
            }
        }
        sc::SCAuctionLowestPricePacket => AuctionEvent::LowestPrice {
            template: s.read_u32()?,
            grade: s.read_u8()?,
            money: s.read_i32()?,
        },
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

// ---- requests --------------------------------------------------------------------------------

/// A level-1 packet with the two auctioneer ids in front.
fn request(
    opcode: u16,
    npc: u32,
    npc2: u32,
    fill: impl FnOnce(&mut PacketStream),
) -> crate::cs::Out {
    let mut s = PacketStream::new();
    s.write_bc(npc).write_bc(npc2);
    fill(&mut s);
    crate::cs::Out {
        level: LEVEL_GAME,
        opcode,
        body: s.into_bytes(),
    }
}

/// `CSAuctionPostPacket` (0xB7): lists the whole stack of the bag item `item_id`. Server read:
/// `E/auction_packets.rs:67`; the checks are `E/auction.rs:1245-1330`: the item is the character's
/// own, in the bag and not soul-bound (flag 0x01), `start >= 0`, `buyout == 0 || buyout >= start`,
/// `duration <= 3`, and the character can pay [`listing_fee`] (else error 241).
///
/// Body: `bc npc, bc npc2, u64 item_id, i32 start, i32 buyout, u8 duration`.
pub fn post(
    npc: u32,
    npc2: u32,
    item_id: u64,
    start: i32,
    buyout: i32,
    duration: u8,
) -> crate::cs::Out {
    request(cs::CSAuctionPostPacket, npc, npc2, |s| {
        s.write_u64(item_id)
            .write_i32(start)
            .write_i32(buyout)
            .write_u8(duration);
    })
}

/// What a search asks for (`AuctionSearch`, `E/auction.rs:206-228`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search<'a> {
    /// Words of the item name (case-insensitive); empty = any item.
    pub keyword: &'a str,
    /// The keyword must be the whole name.
    pub exact: bool,
    /// Item grade, 0 = any.
    pub grade: u8,
    /// Auction categories `(a, b, c)`, 0 = any.
    pub categories: [u8; 3],
    /// Result page, from 0.
    pub page: i32,
    /// Only lots of this seller's character id, 0 = every seller.
    pub client_id: u32,
    /// Lowest and highest item level, 0 = no limit.
    pub levels: [u8; 2],
    /// [`sort_kind`] and [`sort_order`].
    pub sort: [u8; 2],
}

impl Default for Search<'_> {
    fn default() -> Self {
        Search {
            keyword: "",
            exact: false,
            grade: 0,
            categories: [0; 3],
            page: 0,
            client_id: 0,
            levels: [0; 2],
            sort: [sort_kind::DEFAULT, sort_order::ASC],
        }
    }
}

/// `CSAuctionSearchPacket` (0xB8). Server read: `E/auction_packets.rs:113`, matching
/// `E/auction.rs:1330-1388`. Answered by `SCAuctionSearched` (nine lots a page). Note that the
/// server matches the keyword against the item name in every locale and sorts by buyout with the
/// lots *without* a buyout (0) first.
///
/// Body: `bc npc, bc npc2, str keyword, bool exact, u8 grade, u8 cat a, u8 cat b, u8 cat c,
/// i32 page, u32 client_id, i32 filter 0, u8 world 0, u8 min level, u8 max level, u8 sort kind,
/// u8 sort order`.
pub fn search(npc: u32, npc2: u32, q: &Search<'_>) -> crate::cs::Out {
    request(cs::CSAuctionSearchPacket, npc, npc2, |s| {
        s.write_str(q.keyword)
            .write_bool(q.exact)
            .write_u8(q.grade)
            .write_u8(q.categories[0])
            .write_u8(q.categories[1])
            .write_u8(q.categories[2])
            .write_i32(q.page)
            .write_u32(q.client_id)
            .write_i32(0)
            .write_u8(0)
            .write_u8(q.levels[0])
            .write_u8(q.levels[1])
            .write_u8(q.sort[0])
            .write_u8(q.sort[1]);
    })
}

/// `CSBidAuctionPacket` (0xB9): bids on `lot`, or buys it out when `money` reaches its buyout
/// price. Server read: `E/auction_packets.rs:151`: an `AuctionLot` (`E/auction.rs:330`, of which
/// the server only uses the id; the item is written empty) and an `AuctionBid`
/// (`E/auction.rs:165`). The money is taken at once; a bid must exceed the current bid and reach
/// the start price, and the lot must not be the bidder's own (`E/auction.rs:940-1066`).
///
/// Body: `bc npc, bc npc2, u64 lot, u8 duration, u32 0 (empty item), u8 world, u32 client,
/// str client name, i32 start, i32 buyout, i64 post date, u8 bid world, u32 bidder, str bidder
/// name, i32 bid money, i32 extra`, then `u64 lot, u8 world, u32 me, str my name, i32 money`.
pub fn bid(
    npc: u32,
    npc2: u32,
    lot: &Lot,
    my_id: u32,
    my_name: &str,
    money: i32,
) -> crate::cs::Out {
    request(cs::CSBidAuctionPacket, npc, npc2, |s| {
        write_lot_header(s, lot);
        s.write_u64(lot.id)
            .write_u8(lot.world)
            .write_u32(my_id)
            .write_str(my_name)
            .write_i32(money);
    })
}

/// `CSCancelAuctionPacket` (0xBA): the seller takes `lot` back before anybody bid; the item comes
/// back by mail and the fee is not refunded. Server read: `E/auction_packets.rs:195`, handled by
/// `E/auction.rs:884-938`. Answered by `SCAuctionCanceled`.
///
/// Body: `bc npc, bc npc2`, then the lot as in [`bid`].
pub fn cancel(npc: u32, npc2: u32, lot: &Lot) -> crate::cs::Out {
    request(cs::CSCancelAuctionPacket, npc, npc2, |s| {
        write_lot_header(s, lot);
    })
}

/// `CSAuctionMyBidListPacket` (0xBB): the lots the character is the highest bidder of, nine a
/// page, answered by `SCAuctionSearched`. Server read: `E/auction_packets.rs:233`.
///
/// Body: `bc npc, bc npc2, i32 page`.
pub fn my_bid_list(npc: u32, npc2: u32, page: i32) -> crate::cs::Out {
    request(cs::CSAuctionMyBidListPacket, npc, npc2, |s| {
        s.write_i32(page);
    })
}

/// `CSAuctionLowestPricePacket` (0xBC): the lowest buyout of an item template, answered by
/// `SCAuctionLowestPrice`. Server read: `E/auction_packets.rs:271`. Unreliable (it ignores the
/// grade); [`search`] is the better way to learn a price.
///
/// Body: `bc npc, bc npc2, u32 template, u8 grade`.
pub fn lowest_price(npc: u32, npc2: u32, template: u32, grade: u8) -> crate::cs::Out {
    request(cs::CSAuctionLowestPricePacket, npc, npc2, |s| {
        s.write_u32(template).write_u8(grade);
    })
}

/// `AuctionLot.Read` as the server reads it in the bid and cancel packets (`E/auction.rs:330`,
/// `WireItem::read` at `E/auction.rs:269`): the item is a bare template id 0, which the reader
/// accepts as "no item" and which is all the server needs (it looks the lot up by id).
fn write_lot_header(s: &mut PacketStream, lot: &Lot) {
    s.write_u64(lot.id)
        .write_u8(lot.duration)
        .write_u32(0)
        .write_u8(lot.world)
        .write_u32(lot.client_id)
        .write_str(&lot.client_name)
        .write_i32(lot.start)
        .write_i32(lot.buyout)
        .write_i64(lot.post_date)
        .write_u8(lot.bid_world)
        .write_u32(lot.bidder_id)
        .write_str(&lot.bidder_name)
        .write_i32(lot.bid_money)
        .write_i32(lot.extra);
}

#[cfg(test)]
mod tests;
