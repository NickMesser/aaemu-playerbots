//! Prices in chat: how a bot says an amount of copper, and what it asks for an item.
//!
//! A player says money as gold, silver and copper (`1g 20s 5c`; 100 copper make a silver, 100
//! silver a gold). The prices of the advertisements come from the item facts: a vendor pays the
//! `refund` and asks the `price`, so a bot sells somewhere in between (a buyer who could buy at the
//! vendor's price would not pay more, a seller who could sell to the vendor would not take less).
//! [`PriceSource`] is the one place that decides; the auction house's price memory can plug in
//! later by implementing it.

use aa_world::World;

/// Copper in a silver.
pub const SILVER: i64 = 100;
/// Copper in a gold.
pub const GOLD: i64 = 10_000;

/// An amount of copper the way players write it: `1g 20s`, `30s`, `45c`; at least `0c`.
pub fn fmt(copper: i64) -> String {
    let c = copper.max(0);
    if c == 0 {
        return "0c".into();
    }
    let (g, s, cc) = (c / GOLD, (c % GOLD) / SILVER, c % SILVER);
    let mut out = String::new();
    for (n, unit) in [(g, 'g'), (s, 's'), (cc, 'c')] {
        if n > 0 {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&n.to_string());
            out.push(unit);
        }
    }
    out
}

/// What a bot asks for one unit of an item, the lowest it takes, and the most a bot pays for it
/// (copper per unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote {
    /// The asking price of a seller.
    pub ask: u32,
    /// The lowest price a seller takes.
    pub floor: u32,
    /// The highest price a buyer pays.
    pub ceiling: u32,
}

impl Quote {
    /// A companion saves up to ten percent while the seller keeps the same minimum price.
    pub fn for_companion(self) -> Self {
        Self {
            ask: (self.ask - self.ask / 10).max(self.floor),
            ..self
        }
    }

    /// The same quote with the asking price moved by `percent` percent (`-10..=10` is the
    /// per-bot spread), the floor and the ceiling following in proportion.
    pub fn spread(self, percent: i32) -> Quote {
        let scale = |v: u32| {
            let v = i64::from(v) * i64::from(100 + percent) / 100;
            u32::try_from(v.max(1)).unwrap_or(u32::MAX)
        };
        Quote {
            ask: scale(self.ask),
            floor: scale(self.floor),
            ceiling: scale(self.ceiling),
        }
    }
}

/// Where prices come from.
pub trait PriceSource {
    /// What an item is worth, `None` for an item that has no price at all.
    fn quote(&self, template: u32) -> Option<Quote>;
}

/// Prices from the vendor values in the item facts (the default [`PriceSource`]).
#[derive(Clone, Copy)]
pub struct VendorPrices<'a>(pub &'a World);

impl PriceSource for VendorPrices<'_> {
    fn quote(&self, template: u32) -> Option<Quote> {
        let f = self.0.item_facts(template)?;
        quote_from_vendor(f.price, f.refund)
    }
}

/// A price between what a vendor pays (`refund`) and what it asks (`price`): the ask a little
/// over half way, the floor a fifth lower but over the refund, the ceiling a fifth over the ask
/// but never over the vendor's price. `None` for an item with neither.
pub fn quote_from_vendor(price: u32, refund: u32) -> Option<Quote> {
    if price == 0 && refund == 0 {
        return None;
    }
    let price = price.max(refund);
    let ask = if refund == 0 {
        // (a vendor that does not buy it: ask under half of what it asks)
        (price * 45 / 100).max(1)
    } else if price > refund {
        (refund + (price - refund) * 55 / 100)
            .max(refund + 1)
            .min(price)
    } else {
        price
    };
    let floor = (refund * 115 / 100)
        .max(ask * 80 / 100)
        .clamp(refund.min(ask), ask);
    let ceiling = (ask * 120 / 100).min(price).max(ask);
    Some(Quote {
        ask,
        floor: floor.max(1),
        ceiling,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_discount_preserves_the_sellers_floor() {
        let quote = Quote {
            ask: 200,
            floor: 185,
            ceiling: 230,
        };
        assert_eq!(quote.for_companion(), Quote { ask: 185, ..quote });
    }

    #[test]
    fn amounts_are_written_like_players_do() {
        assert_eq!(fmt(0), "0c");
        assert_eq!(fmt(-5), "0c");
        assert_eq!(fmt(45), "45c");
        assert_eq!(fmt(3000), "30s");
        assert_eq!(fmt(10_000), "1g");
        assert_eq!(fmt(12_005), "1g 20s 5c");
        assert_eq!(fmt(10_005), "1g 5c");
        assert_eq!(fmt(250_000), "25g");
    }

    #[test]
    fn a_price_lies_between_what_the_vendor_pays_and_asks() {
        let q = quote_from_vendor(40, 20).unwrap();
        assert_eq!(
            q,
            Quote {
                ask: 31,
                floor: 24,
                ceiling: 37
            }
        );
        for (price, refund) in [
            (40, 20),
            (1000, 1),
            (9, 9),
            (100, 0),
            (0, 50),
            (3, 1),
            (2_000_000, 900_000),
        ] {
            let q = quote_from_vendor(price, refund).unwrap();
            let top = price.max(refund);
            assert!(
                q.floor >= refund.max(1).min(q.ask),
                "{price}/{refund}: {q:?}"
            );
            assert!(
                q.floor <= q.ask && q.ask <= q.ceiling,
                "{price}/{refund}: {q:?}"
            );
            assert!(q.ask <= top.max(1), "{price}/{refund}: {q:?}");
            assert!(q.ceiling <= top.max(q.ask), "{price}/{refund}: {q:?}");
        }
        // nothing to go by
        assert_eq!(quote_from_vendor(0, 0), None);
    }

    #[test]
    fn the_spread_moves_all_three_together() {
        let q = quote_from_vendor(1000, 500).unwrap();
        let hi = q.spread(10);
        let lo = q.spread(-10);
        assert!(hi.ask > q.ask && lo.ask < q.ask);
        assert!(lo.floor <= lo.ask && hi.ask <= hi.ceiling);
        assert_eq!(q.spread(0), q);
        // never below one copper
        assert_eq!(quote_from_vendor(1, 1).unwrap().spread(-10).ask, 1);
    }
}
