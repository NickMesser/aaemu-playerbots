//! Prices at the auction house: what to ask for an item, and what is worth buying.
//!
//! Everything here is pure arithmetic on copper amounts, so the goal's decisions can be tested
//! without a server.
//!
//! # What selling costs
//!
//! The server takes a listing fee when a lot is posted (`buyout * 0.01 * (duration + 1)`, never
//! refunded, [`aa_proto::auction::listing_fee`]) and pays the seller 90% of the sale price. A bot
//! that could simply sell to a vendor (`items.refund`, the amount a vendor pays for one item) lists
//! a stack only when the money it will certainly net, `floor(0.9 * buyout) - fee`, beats the vendor
//! by [`MIN_GAIN_PCT`]; the cheapest such buyout is the *floor*. On top of the floor the price
//! follows the market:
//!
//! * with a market ([`Quote`]: the median of the lowest buyouts per item of the lots of the same
//!   item), the bot asks `quote * u%` with `u` between 95 and 99 (a little under the others), but
//!   never less than the floor; when even the floor is clearly above the market (more than
//!   [`OVERPRICED_PCT`] of it) the vendor is the better buyer and the stack is not listed;
//! * without one, the bot asks the middle between what a vendor pays and what a vendor charges
//!   (`items.refund` and `items.price`), or one and a half times the refund when the item has no
//!   shop price.

use aa_proto::auction::{listing_fee, Lot};

/// The share of the sale price the seller is paid, in percent (`remove_auction_lot_sold`).
pub const SELLER_SHARE_PCT: f64 = 0.9;
/// The least gain over a vendor's offer that makes a listing worth the wait, in percent.
pub const MIN_GAIN_PCT: i64 = 8;
/// A floor above this percentage of the market quote means the market is below what the vendor
/// pays, once the fee and the tax are counted.
pub const OVERPRICED_PCT: i64 = 103;
/// How many of the cheapest lots the quote looks at.
pub const QUOTE_LOTS: usize = 5;
/// A stack is worth a trip when a vendor would pay at least this much for it (copper).
pub const MIN_STACK_VALUE: u64 = 100;
/// A visit is worth it when the stacks together are worth at least this much to a vendor (copper).
pub const MIN_TOTAL_VALUE: u64 = 300;
/// A lot is a bargain when it costs at most this percentage of what a vendor would pay for it.
pub const BARGAIN_PCT: u64 = 75;

/// The trade facts of an item the price needs ([`aa_world::ItemFacts`] reduced to copper amounts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// What a vendor asks for one item (`items.price`).
    pub price: u32,
    /// What a vendor pays for one item (`items.refund`).
    pub refund: u32,
}

/// What the market asks for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// The median of the lowest per-item buyouts (copper).
    pub unit: u32,
    /// The lowest per-item buyout (copper).
    pub lowest: u32,
    /// How many lots the quote is made of (at most [`QUOTE_LOTS`]).
    pub lots: u8,
}

/// The market quote for the per-item buyouts `units` (any order): the median of the cheapest
/// [`QUOTE_LOTS`]. `None` without a lot.
pub fn quote(units: &mut [u32]) -> Option<Quote> {
    units.sort_unstable();
    let n = units.len().min(QUOTE_LOTS);
    if n == 0 {
        return None;
    }
    let cheapest = &units[..n];
    let unit = if n % 2 == 1 {
        cheapest[n / 2]
    } else {
        // (the mean of the two in the middle, rounded up)
        let (a, b) = (u64::from(cheapest[n / 2 - 1]), u64::from(cheapest[n / 2]));
        ((a + b).div_ceil(2)) as u32
    };
    Some(Quote {
        unit,
        lowest: cheapest[0],
        lots: n as u8,
    })
}

/// The per-item buyouts of the lots in `lots` that are the item `template` of grade `grade`
/// (0 = any), have a buyout and are not `me`'s own, as input for [`quote`].
pub fn market_units(lots: &[Lot], template: u32, grade: u8, me: u32) -> Vec<u32> {
    lots.iter()
        .filter(|l| l.template == template && (grade == 0 || l.grade == grade))
        .filter(|l| l.client_id != me)
        .filter_map(Lot::buyout_unit)
        .collect()
}

/// What the seller nets when a lot with this buyout sells at once: the share the server pays
/// out minus the listing fee that was taken up front (copper, can be negative).
pub fn net_of(buyout: i64, duration: u8) -> i64 {
    let b = buyout.clamp(0, i64::from(i32::MAX));
    let share = (b as f64 * SELLER_SHARE_PCT) as i64;
    share - i64::from(listing_fee(b as i32, duration))
}

/// The lowest buyout whose net beats a vendor paying `vendor` copper for the stack by
/// [`MIN_GAIN_PCT`]; `None` when it does not fit the wire's `i32`.
pub fn floor_buyout(vendor: u64, duration: u8) -> Option<i64> {
    let vendor = i64::try_from(vendor).ok()?;
    let target = ((vendor.checked_mul(100 + MIN_GAIN_PCT)? + 99) / 100).max(1);
    // net ~ buyout * (0.9 - 0.01 * (duration + 1)); start there and walk up over the truncation
    let rate = 0.9 - 0.01 * f64::from(u32::from(duration) + 1);
    let start = ((target as f64 / rate).ceil() as i64)
        .saturating_sub(2)
        .max(1);
    (start..).take(8).find_map(|b| {
        (net_of(b, duration) >= target)
            .then_some(b)
            .filter(|b| *b <= i64::from(i32::MAX))
    })
}

/// The price of a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask {
    /// Starting bid (copper).
    pub start: i32,
    /// Buyout price (copper).
    pub buyout: i32,
    /// The buyout per item, rounded up.
    pub unit: u32,
    /// The listing fee taken at posting.
    pub fee: i32,
    /// What the stack nets when it sells at the buyout price.
    pub net: i64,
}

/// Why a stack is not listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// A vendor does not pay anything for it, or the prices overflow.
    NoValue,
    /// A vendor pays more than the market would, fee and tax counted.
    BelowVendor,
}

/// The price for `count` items: `quote` is the market (if any), `undercut_pct` how much of the
/// quote to ask (95..=99), `duration` the lot's `AuctionDuration`. See the module docs.
pub fn ask(
    facts: Facts,
    count: u32,
    quote: Option<Quote>,
    undercut_pct: u32,
    duration: u8,
) -> Result<Ask, Skip> {
    if count == 0 || facts.refund == 0 {
        return Err(Skip::NoValue);
    }
    let count64 = u64::from(count);
    let vendor = u64::from(facts.refund) * count64;
    let floor = floor_buyout(vendor, duration).ok_or(Skip::NoValue)?;
    let target_unit = match quote {
        Some(q) => (u64::from(q.unit) * u64::from(undercut_pct.clamp(50, 100)) / 100).max(1),
        None => u64::from(heuristic_unit(facts)),
    };
    let target = i64::try_from(target_unit * count64).map_err(|_| Skip::NoValue)?;
    if let Some(q) = quote {
        let market = i64::try_from(u64::from(q.unit) * count64).map_err(|_| Skip::NoValue)?;
        if floor * 100 > market * OVERPRICED_PCT {
            return Err(Skip::BelowVendor);
        }
    }
    let buyout = floor.max(target);
    let buyout32 = i32::try_from(buyout).map_err(|_| Skip::NoValue)?;
    let start = i32::try_from((buyout * 9 / 10).max(1)).map_err(|_| Skip::NoValue)?;
    Ok(Ask {
        start,
        buyout: buyout32,
        unit: ((buyout + i64::from(count) - 1) / i64::from(count)) as u32,
        fee: listing_fee(buyout32, duration),
        net: net_of(buyout, duration),
    })
}

/// The per-item price asked when nobody else sells the item: the middle between what a vendor
/// pays and what a vendor charges, or one and a half times the refund for an item the shops do
/// not sell.
pub fn heuristic_unit(facts: Facts) -> u32 {
    if facts.price > facts.refund {
        ((u64::from(facts.price) + u64::from(facts.refund)) / 2) as u32
    } else {
        (u64::from(facts.refund) * 3 / 2) as u32
    }
}

/// Why a lot is worth buying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// It costs far less than a vendor pays for it.
    Bargain,
    /// A recent record of completed sales supports buying it for resale.
    Resale,
    /// The bot can use the item and the lot costs no more than a shop would.
    Useful,
}

/// Use a discount from observed sales to leave room for competition on the resale.
pub(crate) fn resale_ask(unit: f32, count: u32, cost: i64) -> Option<Ask> {
    if !unit.is_finite() || unit <= 0.0 || count == 0 || cost <= 0 {
        return None;
    }
    let buyout = (f64::from(unit) * f64::from(count) * 0.9).floor() as i64;
    let buyout = i32::try_from(buyout).ok().filter(|&price| price > 0)?;
    let fee = listing_fee(buyout, aa_proto::auction::duration::H48);
    let net = net_of(i64::from(buyout), aa_proto::auction::duration::H48);
    // Fixed-price listings protect the margin even when someone places the minimum bid.
    (net >= cost + (cost / 5).max(50)).then_some(Ask {
        start: buyout,
        buyout,
        unit: buyout as u32 / count,
        fee,
        net,
    })
}

/// Whether the bot should buy `lot` out for its buyout price. `facts` are the item's, `useful`
/// whether the bot can use the item itself, `money` what it has, `budget` the most it spends.
/// Never its own lot, a lot somebody bid on, or one without a buyout price.
pub fn worth_buying(
    lot: &Lot,
    facts: Facts,
    useful: bool,
    me: u32,
    money: i64,
    budget: i64,
) -> Option<Why> {
    if lot.client_id == me || lot.has_bid() || lot.buyout <= 0 || lot.count <= 0 {
        return None;
    }
    let pay = i64::from(lot.buyout);
    if pay > budget || pay > money {
        return None;
    }
    let count = lot.count as u64;
    let vendor = u64::from(facts.refund) * count;
    if vendor > 0 && (pay as u64) * 100 <= vendor * BARGAIN_PCT {
        return Some(Why::Bargain);
    }
    let shop = u64::from(facts.price) * count;
    if useful && shop > 0 && (pay as u64) <= shop {
        return Some(Why::Useful);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_proto::auction::duration;

    fn lot(template: u32, count: i32, buyout: i32, client: u32) -> Lot {
        Lot {
            id: u64::from(template) * 100 + u64::from(client),
            template,
            count,
            buyout,
            client_id: client,
            ..Lot::default()
        }
    }

    #[test]
    fn resale_includes_the_listing_fee_and_requires_a_margin() {
        let ask = resale_ask(200.0, 10, 1000).unwrap();
        assert_eq!(
            (ask.start, ask.buyout, ask.fee, ask.net),
            (1800, 1800, 72, 1548)
        );
        assert!(resale_ask(200.0, 10, 1450).is_none());
        assert!(resale_ask(f32::NAN, 10, 1000).is_none());
    }

    #[test]
    fn the_quote_is_the_median_of_the_lowest_few() {
        assert_eq!(quote(&mut []), None);
        assert_eq!(
            quote(&mut [40]),
            Some(Quote {
                unit: 40,
                lowest: 40,
                lots: 1
            })
        );
        // two: the mean, rounded up
        assert_eq!(quote(&mut [41, 40]).unwrap().unit, 41);
        // five of seven: 10 20 30 40 50 -> 30; the dear ones do not count
        let q = quote(&mut [900, 50, 10, 30, 1000, 20, 40]).unwrap();
        assert_eq!((q.unit, q.lowest, q.lots), (30, 10, 5));
        // four: the mean of the two in the middle
        assert_eq!(quote(&mut [10, 20, 30, 40]).unwrap().unit, 25);
    }

    #[test]
    fn market_units_skip_own_lots_other_items_and_lots_without_a_buyout() {
        let lots = [
            lot(1, 10, 100, 7), // 10 each
            lot(1, 4, 100, 7),  // 25 each
            lot(1, 5, 100, 99), // mine
            lot(2, 1, 50, 7),   // another item
            lot(1, 3, 0, 7),    // no buyout
            lot(1, 0, 100, 7),  // no items
        ];
        let mut units = market_units(&lots, 1, 0, 99);
        units.sort_unstable();
        assert_eq!(units, [10, 25]);
        // the grade filter
        let mut g = lot(1, 1, 10, 7);
        g.grade = 3;
        assert!(market_units(&[g.clone()], 1, 2, 99).is_empty());
        assert_eq!(market_units(&[g], 1, 3, 99), [10]);
    }

    #[test]
    fn the_net_is_the_share_minus_the_fee() {
        // 48 h: fee 4%, share 90%
        assert_eq!(net_of(1000, duration::H48), 900 - 40);
        assert_eq!(net_of(1000, duration::H6), 900 - 10);
        assert_eq!(net_of(0, duration::H48), 0);
        assert_eq!(net_of(-3, duration::H48), 0);
        // the server truncates both
        assert_eq!(net_of(99, duration::H48), 89 - 3);
    }

    #[test]
    fn the_floor_beats_the_vendor_by_the_margin() {
        for vendor in [1u64, 7, 20, 100, 333, 1_000, 12_345, 1_000_000] {
            let b = floor_buyout(vendor, duration::H48).unwrap();
            let target = (vendor as i64 * 108 + 99) / 100;
            assert!(net_of(b, duration::H48) >= target, "{vendor}: {b}");
            // and it is the cheapest such buyout
            assert!(net_of(b - 1, duration::H48) < target, "{vendor}: {}", b - 1);
        }
        assert_eq!(floor_buyout(u64::MAX, duration::H48), None);
        assert_eq!(floor_buyout(u64::from(u32::MAX) * 10, duration::H6), None);
    }

    #[test]
    fn with_a_market_the_ask_undercuts_it_but_not_below_the_floor() {
        let facts = Facts {
            price: 40,
            refund: 20,
        };
        let q = Quote {
            unit: 100,
            lowest: 90,
            lots: 5,
        };
        // 20 items: the market asks 2000, the bot 97% of it
        let a = ask(facts, 20, Some(q), 97, duration::H48).unwrap();
        assert_eq!(a.buyout, 1940);
        assert_eq!(a.start, 1746);
        assert_eq!(a.fee, 77);
        assert_eq!(a.net, 1746 - 77);
        assert_eq!(a.unit, 97);
        // vendor: 400. Market at 26 each (520): the floor is 503, the bot's 96% would be 499: floor
        let q = Quote {
            unit: 26,
            lowest: 26,
            lots: 3,
        };
        let a = ask(facts, 20, Some(q), 96, duration::H48).unwrap();
        assert_eq!(a.buyout as i64, floor_buyout(400, duration::H48).unwrap());
        assert!(a.net >= 432);
        // market at 24 each (480): 14% of fee and tax leave 412, the vendor pays 400 and the bot
        // wants 8% over it
        let q = Quote {
            unit: 24,
            lowest: 24,
            lots: 3,
        };
        assert_eq!(
            ask(facts, 20, Some(q), 97, duration::H48),
            Err(Skip::BelowVendor)
        );
    }

    #[test]
    fn without_a_market_the_ask_is_between_the_vendor_prices() {
        let facts = Facts {
            price: 40,
            refund: 20,
        };
        let a = ask(facts, 10, None, 97, duration::H48).unwrap();
        assert_eq!(heuristic_unit(facts), 30);
        assert_eq!(a.buyout, 300);
        assert!(a.net > 200, "{a:?}");
        // no shop price: one and a half times the refund
        let odd = Facts {
            price: 0,
            refund: 20,
        };
        assert_eq!(heuristic_unit(odd), 30);
        // nothing a vendor pays for
        assert_eq!(
            ask(
                Facts {
                    price: 5,
                    refund: 0
                },
                10,
                None,
                97,
                duration::H48
            ),
            Err(Skip::NoValue)
        );
        assert_eq!(ask(facts, 0, None, 97, duration::H48), Err(Skip::NoValue));
    }

    #[test]
    fn the_ask_never_overflows() {
        let rich = Facts {
            price: u32::MAX,
            refund: u32::MAX,
        };
        assert_eq!(ask(rich, 1000, None, 97, duration::H48), Err(Skip::NoValue));
        let q = Quote {
            unit: u32::MAX,
            lowest: u32::MAX,
            lots: 1,
        };
        assert_eq!(
            ask(
                Facts {
                    price: 10,
                    refund: 5
                },
                1000,
                Some(q),
                99,
                duration::H48
            ),
            Err(Skip::NoValue)
        );
    }

    #[test]
    fn a_listing_never_loses_against_the_vendor() {
        let mut rng = fastrand::Rng::with_seed(7);
        for _ in 0..2000 {
            let refund = rng.u32(1..5000);
            let facts = Facts {
                price: refund + rng.u32(0..5000),
                refund,
            };
            let count = rng.u32(1..200);
            let dur = rng.u8(0..4);
            let quote = (rng.u8(0..3) > 0).then(|| {
                let unit = rng.u32(1..20_000);
                Quote {
                    unit,
                    lowest: unit,
                    lots: 3,
                }
            });
            if let Ok(a) = ask(facts, count, quote, rng.u32(95..100), dur) {
                let vendor = i64::from(facts.refund) * i64::from(count);
                assert!(a.net > vendor, "{facts:?} x{count} {quote:?}: {a:?}");
                assert!(a.start <= a.buyout && a.start >= 1);
                assert_eq!(a.fee, listing_fee(a.buyout, dur));
            }
        }
    }

    #[test]
    fn what_is_worth_buying() {
        let facts = Facts {
            price: 40,
            refund: 20,
        };
        let me = 9;
        // 10 items, a vendor pays 200: 140 is a bargain (70%)
        let cheap = lot(1, 10, 140, 7);
        assert_eq!(
            worth_buying(&cheap, facts, false, me, 10_000, 5_000),
            Some(Why::Bargain)
        );
        // 300 is not, unless the bot can use it and a shop would charge 400
        let fair = lot(1, 10, 300, 7);
        assert_eq!(worth_buying(&fair, facts, false, me, 10_000, 5_000), None);
        assert_eq!(
            worth_buying(&fair, facts, true, me, 10_000, 5_000),
            Some(Why::Useful)
        );
        // dearer than the shop
        assert_eq!(
            worth_buying(&lot(1, 10, 450, 7), facts, true, me, 10_000, 5_000),
            None
        );
        // budget and purse
        assert_eq!(worth_buying(&cheap, facts, false, me, 100, 5_000), None);
        assert_eq!(worth_buying(&cheap, facts, false, me, 10_000, 100), None);
        // own lot, bid on, no buyout, nothing in it
        assert_eq!(
            worth_buying(&lot(1, 10, 140, me), facts, false, me, 10_000, 5_000),
            None
        );
        let mut bid = cheap.clone();
        bid.bidder_id = 4;
        assert_eq!(worth_buying(&bid, facts, false, me, 10_000, 5_000), None);
        assert_eq!(
            worth_buying(&lot(1, 10, 0, 7), facts, false, me, 10_000, 5_000),
            None
        );
        assert_eq!(
            worth_buying(&lot(1, 0, 10, 7), facts, false, me, 10_000, 5_000),
            None
        );
    }
}
