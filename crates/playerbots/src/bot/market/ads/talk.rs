//! One negotiation between a seller and a buyer, as a pure state machine.
//!
//! A [`Talk`] starts when a buyer answers an advertisement of goods for sale, or a seller answers
//! one of goods wanted, and ends in an agreement (the price per unit, the quantity, the place to
//! meet) or in a refusal. Every line the other side says is read by
//! [`grammar::parse`](super::grammar::parse); [`Talk::on_line`] says what to answer, if anything.
//!
//! ```text
//!   Opened ──(ask)──► Quoted ──(price ok / "deal")──► Agreed ──► [meeting, trade: goals::market]
//!      │                 │  └─ one counter-offer, then "my lowest", then no
//!      └─────────────────┴───────────────► Closed (declined, cancelled, timed out)
//! ```
//!
//! The seller never goes under its floor and never counters more than once on the price; the buyer
//! never pays over its ceiling and haggles once. The seller names the place to meet (the glue asks
//! the meeting rules of the market goal through a callback); the buyer reads it.

use super::grammar::Line;
use super::lines::{Decline, Meet, Speech};
use super::price::Quote;

/// A talk with nobody answering for this long ends (milliseconds).
pub const IDLE_MS: u64 = 4 * 60_000;
/// An agreed deal that has not been traded within this long is dropped.
pub const AGREED_MS: u64 = 12 * 60_000;

/// Which side the bot is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The bot sells.
    Seller,
    /// The bot buys.
    Buyer,
}

/// Where the lines go: a whisper to the other side, or the channel the other side spoke in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    /// A private message.
    Whisper,
    /// The trade channel (addressed by name).
    Channel,
}

/// How a talk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// No agreement on the price or the goods.
    Declined,
    /// One side called an agreed deal off.
    Cancelled,
    /// Nobody answered for too long.
    TimedOut,
    /// The trade was made.
    Done,
}

/// Where a talk stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// A first line was exchanged; no price on the table yet.
    Opened,
    /// A price is on the table.
    Quoted,
    /// Price, quantity and place are agreed.
    Agreed,
    /// Over.
    Closed(Closed),
}

/// What the meeting-place callback is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceQ<'a> {
    /// The seller chooses where to meet `peer`.
    Choose {
        /// The buyer.
        peer: &'a str,
    },
    /// The buyer was told the place by the seller `peer`: `mirage` is what the line said.
    Told {
        /// The seller.
        peer: &'a str,
        /// The line named Mirage Isle (else a town).
        mirage: bool,
    },
}

/// A negotiation. See the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct Talk {
    /// Which side the bot is on.
    pub role: Role,
    /// The other side's character name.
    pub peer: Box<str>,
    /// Where the lines go.
    pub medium: Medium,
    /// The item.
    pub template: u32,
    /// Units on the table.
    pub qty: u32,
    /// Copper per unit on the table.
    pub unit: u32,
    pub funding: Option<super::Funding>,
    /// The lowest price per unit a seller takes, or the highest a buyer pays.
    pub limit: u32,
    /// Units the seller has (the cap of `qty`); 0 = not known (a buyer).
    pub avail: u32,
    /// Where it stands.
    pub stage: Stage,
    /// The one counter-offer was made.
    pub countered: bool,
    /// Where to meet, once agreed.
    pub meet: Option<Meet>,
    /// Bot clock of the first line.
    pub started_ms: u64,
    /// Times the seller told the place after the agreement (it answers a question twice at most).
    pub place_told: u8,
    /// Bot clock of the last line either side said.
    pub last_ms: u64,
}

fn per_unit(l: &Line, qty: u32) -> Option<u32> {
    let p = l.price?;
    let c = if p.per_unit {
        p.copper
    } else {
        p.copper / i64::from(qty.max(1))
    };
    u32::try_from(c.max(0)).ok()
}

impl Talk {
    /// A seller's talk with a buyer who answered the advertisement: `quote` is the listing's price,
    /// `avail` the units it still has.
    pub fn seller(
        peer: &str,
        medium: Medium,
        template: u32,
        avail: u32,
        quote: Quote,
        now_ms: u64,
    ) -> Talk {
        Talk {
            role: Role::Seller,
            peer: peer.into(),
            medium,
            template,
            qty: avail,
            unit: quote.ask,
            funding: None,
            limit: quote.floor,
            avail,
            stage: Stage::Opened,
            countered: false,
            meet: None,
            started_ms: now_ms,
            place_told: 0,
            last_ms: now_ms,
        }
    }

    /// A buyer's talk with a seller; the buyer wants `qty` units and pays at most `ceiling` per
    /// unit.
    pub fn buyer(
        peer: &str,
        medium: Medium,
        template: u32,
        qty: u32,
        ceiling: u32,
        now_ms: u64,
    ) -> Talk {
        Talk {
            role: Role::Buyer,
            peer: peer.into(),
            medium,
            template,
            qty,
            unit: 0,
            funding: None,
            limit: ceiling,
            avail: 0,
            stage: Stage::Opened,
            countered: false,
            meet: None,
            started_ms: now_ms,
            place_told: 0,
            last_ms: now_ms,
        }
    }

    /// Copper for all the units on the table.
    pub fn total(&self) -> i64 {
        i64::from(self.unit) * i64::from(self.qty)
    }

    /// Whether the talk is over.
    pub fn is_closed(&self) -> bool {
        matches!(self.stage, Stage::Closed(_))
    }

    /// Whether price, quantity and place are agreed.
    pub fn is_agreed(&self) -> bool {
        self.stage == Stage::Agreed
    }

    /// Ends the talk.
    pub fn close(&mut self, how: Closed) {
        self.stage = Stage::Closed(how);
    }

    /// Ends a talk nobody takes part in any more. Returns whether it just ended.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let limit = match self.stage {
            Stage::Closed(_) => return false,
            Stage::Agreed => AGREED_MS,
            _ => IDLE_MS,
        };
        if now_ms.saturating_sub(self.last_ms) > limit {
            self.close(Closed::TimedOut);
            true
        } else {
            false
        }
    }

    /// The buyer's first line to a seller who advertised `ask` copper per unit, if it is worth a
    /// line: take the goods at the asked price, haggle once when it is a little over the ceiling,
    /// or (`ask_first`) ask what it costs. `None` when the price is far over what the buyer pays.
    pub fn open_as_buyer(&mut self, ask: u32, ask_first: bool) -> Option<Speech> {
        if u64::from(ask) * 100 > u64::from(self.limit) * 130 {
            self.close(Closed::Declined);
            return None;
        }
        if ask_first {
            return Some(Speech::Ask {
                template: self.template,
            });
        }
        if ask <= self.limit {
            self.unit = ask;
            self.stage = Stage::Quoted;
            Some(Speech::Take {
                qty: self.qty,
                unit: None,
            })
        } else {
            self.countered = true;
            self.unit = self.limit;
            self.stage = Stage::Quoted;
            Some(Speech::Counter {
                unit: self.limit,
                seller: false,
            })
        }
    }

    /// The seller's first line to a buyer whose reply to its advertisement was `l`: nothing to
    /// add to a bare reply is a quote, anything more is handled like a later line.
    pub fn on_line(
        &mut self,
        l: &Line,
        medium: Medium,
        now_ms: u64,
        place: &mut dyn FnMut(PlaceQ<'_>) -> Option<Meet>,
    ) -> Option<Speech> {
        if self.is_closed() {
            return None;
        }
        self.last_ms = now_ms;
        self.medium = medium;
        match self.role {
            Role::Seller => self.seller_line(l, place),
            Role::Buyer => self.buyer_line(l, place),
        }
    }

    // ---- the seller ----------------------------------------------------------------------

    fn seller_line(
        &mut self,
        l: &Line,
        place: &mut dyn FnMut(PlaceQ<'_>) -> Option<Meet>,
    ) -> Option<Speech> {
        if self.stage == Stage::Agreed {
            if l.refuse && !l.take {
                self.close(Closed::Cancelled);
                return None;
            }
            // (only a question gets the place, and only twice: "ill be at the mailbox" is a
            // statement, and two bots must never answer each other's statements for ever)
            if (l.where_q || l.ask || l.question) && self.place_told < 2 {
                self.place_told += 1;
                return self.meet.map(|meet| Speech::Place { meet });
            }
            return None;
        }
        if l.refuse && !l.take && l.price.is_none() {
            self.close(Closed::Declined);
            return None;
        }
        if self.avail == 0 {
            self.close(Closed::Declined);
            return Some(Speech::Decline(Decline::SoldOut));
        }
        if let Some(q) = l.qty {
            self.qty = q.clamp(1, self.avail);
        }
        match per_unit(l, self.qty) {
            Some(p) if p >= self.unit => self.agree(place),
            Some(p) if p >= self.limit => {
                self.unit = p;
                self.agree(place)
            }
            Some(p) => {
                if self.countered {
                    self.close(Closed::Declined);
                    return Some(Speech::Decline(Decline::Price));
                }
                self.countered = true;
                // half way, never under the floor
                let unit = ((self.unit + p) / 2).max(self.limit).min(self.unit);
                self.unit = unit;
                self.stage = Stage::Quoted;
                Some(Speech::Counter { unit, seller: true })
            }
            None if l.take || l.agree => self.agree(place),
            None if l.haggle => {
                self.stage = Stage::Quoted;
                if !self.countered {
                    self.countered = true;
                    let unit = (self.unit * 92 / 100).max(self.limit).min(self.unit);
                    if unit < self.unit {
                        self.unit = unit;
                        return Some(Speech::Counter { unit, seller: true });
                    }
                    return Some(Speech::Lowest { unit: self.unit });
                }
                if self.unit > self.limit {
                    self.unit = self.limit;
                    return Some(Speech::Lowest { unit: self.limit });
                }
                self.close(Closed::Declined);
                Some(Speech::Decline(Decline::Price))
            }
            None if l.ask || self.stage == Stage::Opened => {
                self.stage = Stage::Quoted;
                Some(Speech::Quote {
                    template: self.template,
                    qty: self.avail,
                    unit: self.unit,
                })
            }
            None => None,
        }
    }

    fn agree(&mut self, place: &mut dyn FnMut(PlaceQ<'_>) -> Option<Meet>) -> Option<Speech> {
        let meet = place(PlaceQ::Choose { peer: &self.peer })?;
        self.meet = Some(meet);
        self.stage = Stage::Agreed;
        Some(Speech::Confirm {
            template: self.template,
            qty: self.qty,
            total: self.total(),
            meet,
        })
    }

    // ---- the buyer -----------------------------------------------------------------------

    fn buyer_line(
        &mut self,
        l: &Line,
        place: &mut dyn FnMut(PlaceQ<'_>) -> Option<Meet>,
    ) -> Option<Speech> {
        if self.stage == Stage::Agreed {
            if l.refuse && !l.take {
                self.close(Closed::Cancelled);
            }
            return None;
        }
        if l.refuse && !l.take && l.price.is_none() {
            self.close(Closed::Declined);
            return None;
        }
        // the seller closes the deal and names the place: check the total against ours
        if l.agree && (l.mirage || l.town) && self.stage == Stage::Quoted && self.unit > 0 {
            if let Some(p) = l.price.filter(|p| !p.per_unit) {
                if p.copper > self.total() + self.total() / 20 {
                    self.close(Closed::Declined);
                    return Some(Speech::Decline(Decline::Price));
                }
            }
            let meet = place(PlaceQ::Told {
                peer: &self.peer,
                mirage: l.mirage,
            })?;
            self.meet = Some(meet);
            self.stage = Stage::Agreed;
            return Some(Speech::OnMyWay);
        }
        if let Some(q) = l.qty {
            // "i have 20": we take what we want of it
            self.avail = q;
            self.qty = self.qty.min(q).max(1);
        }
        match per_unit(l, self.qty) {
            Some(p) if p <= self.limit => {
                self.unit = p;
                self.stage = Stage::Quoted;
                Some(Speech::Take {
                    qty: self.qty,
                    unit: None,
                })
            }
            Some(_) if !self.countered => {
                self.countered = true;
                self.unit = self.limit;
                self.stage = Stage::Quoted;
                Some(Speech::Counter {
                    unit: self.limit,
                    seller: false,
                })
            }
            Some(_) => {
                self.close(Closed::Declined);
                Some(Speech::Decline(Decline::Price))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::grammar::parse;
    use super::*;

    fn quote() -> Quote {
        // ask 30s, floor 24s, ceiling 36s
        Quote {
            ask: 3000,
            floor: 2400,
            ceiling: 3600,
        }
    }

    fn mirage(_: PlaceQ<'_>) -> Option<Meet> {
        Some(Meet::Mirage)
    }

    fn say(t: &mut Talk, text: &str, now: u64) -> Option<Speech> {
        t.on_line(&parse(text), Medium::Channel, now, &mut mirage)
    }

    fn seller() -> Talk {
        Talk::seller("Bob", Medium::Channel, 3411, 20, quote(), 1000)
    }

    #[test]
    fn a_bare_reply_gets_the_quote() {
        let mut t = seller();
        assert_eq!(
            say(&mut t, "how much for the ore?", 2000),
            Some(Speech::Quote {
                template: 3411,
                qty: 20,
                unit: 3000
            })
        );
        assert_eq!(t.stage, Stage::Quoted);
        // "interested" / "pst" style replies too
        let mut t = seller();
        assert!(matches!(
            say(&mut t, "hi", 2000),
            Some(Speech::Quote { .. })
        ));
    }

    #[test]
    fn taking_the_goods_at_the_asked_price_closes_the_deal() {
        let mut t = seller();
        say(&mut t, "price?", 2000);
        let s = say(&mut t, "ok ill take 5", 3000).unwrap();
        assert_eq!(
            s,
            Speech::Confirm {
                template: 3411,
                qty: 5,
                total: 15_000,
                meet: Meet::Mirage
            }
        );
        assert!(t.is_agreed());
        assert_eq!((t.qty, t.unit, t.total()), (5, 3000, 15_000));
    }

    #[test]
    fn a_buyer_asking_for_more_than_there_is_gets_what_there_is() {
        let mut t = seller();
        say(&mut t, "ill take 50", 2000);
        assert_eq!(t.qty, 20);
        assert!(t.is_agreed());
    }

    #[test]
    fn a_price_above_the_floor_is_accepted_and_a_lower_one_is_countered_once() {
        // 27s is above the 24s floor: accepted at the buyer's price
        let mut t = seller();
        let s = say(&mut t, "ill take 10 for 27s ea", 2000).unwrap();
        assert!(
            matches!(
                s,
                Speech::Confirm {
                    qty: 10,
                    total: 27_000,
                    ..
                }
            ),
            "{s:?}"
        );
        assert_eq!(t.unit, 2700);
        // 20s is under the floor: half way (25s), once
        let mut t = seller();
        let s = say(&mut t, "20s ea?", 2000).unwrap();
        assert_eq!(
            s,
            Speech::Counter {
                unit: 2500,
                seller: true
            }
        );
        assert_eq!(t.stage, Stage::Quoted);
        // accepting the counter-offer closes at 25s
        let s = say(&mut t, "ok deal", 3000).unwrap();
        assert!(matches!(s, Speech::Confirm { .. }));
        assert_eq!(t.unit, 2500);
        // still low after the counter: no
        let mut t = seller();
        say(&mut t, "20s ea?", 2000);
        let s = say(&mut t, "15s ea", 3000).unwrap();
        assert_eq!(s, Speech::Decline(Decline::Price));
        assert_eq!(t.stage, Stage::Closed(Closed::Declined));
        // and a closed talk says nothing more
        assert_eq!(say(&mut t, "ok 30s", 4000), None);
    }

    #[test]
    fn the_seller_never_sells_under_the_floor() {
        for text in [
            "1c",
            "1 copper ea",
            "5s ea",
            "23s ea",
            "ill take them for 1s",
        ] {
            let mut t = seller();
            let first = say(&mut t, text, 2000);
            assert!(
                !matches!(first, Some(Speech::Confirm { .. })),
                "{text}: {first:?}"
            );
            assert!(t.unit >= t.limit, "{text}");
            let second = say(&mut t, text, 3000);
            assert!(
                !matches!(second, Some(Speech::Confirm { .. })),
                "{text}: {second:?}"
            );
        }
        // haggling without a price walks down to the floor and stops there
        let mut t = seller();
        say(&mut t, "how much", 1500);
        let mut last = 3000;
        for i in 0..6 {
            match say(&mut t, "too much, cheaper?", 2000 + i * 100) {
                Some(Speech::Counter { unit, .. }) | Some(Speech::Lowest { unit }) => {
                    assert!(unit >= 2400 && unit <= last, "{unit}");
                    last = unit;
                }
                other => {
                    assert_eq!(other, Some(Speech::Decline(Decline::Price)), "{i}");
                    break;
                }
            }
        }
        assert!(t.is_closed() || t.unit == 2400, "{t:?}");
    }

    #[test]
    fn a_total_price_is_divided_by_the_quantity() {
        let mut t = seller();
        // 10 units for 3g total = 30s each: the asked price
        let s = say(&mut t, "ill take 10 for 3g total", 2000).unwrap();
        assert!(matches!(s, Speech::Confirm { qty: 10, .. }), "{s:?}");
        assert_eq!(t.unit, 3000);
    }

    #[test]
    fn refusals_end_the_talk_quietly() {
        let mut t = seller();
        say(&mut t, "price?", 2000);
        assert_eq!(say(&mut t, "no thanks", 3000), None);
        assert_eq!(t.stage, Stage::Closed(Closed::Declined));
        // after an agreement a refusal cancels the deal
        let mut t = seller();
        say(&mut t, "ill take 5", 2000);
        assert!(t.is_agreed());
        assert_eq!(say(&mut t, "sorry, cancel", 3000), None);
        assert_eq!(t.stage, Stage::Closed(Closed::Cancelled));
    }

    #[test]
    fn after_the_deal_where_is_answered_with_the_place() {
        let mut t = seller();
        say(&mut t, "ill take 5", 2000);
        assert_eq!(
            say(&mut t, "where?", 3000),
            Some(Speech::Place { meet: Meet::Mirage })
        );
        // omw / thanks need no answer, nor does a statement about the place
        assert_eq!(say(&mut t, "ok omw", 4000), None);
        assert_eq!(say(&mut t, "thx", 5000), None);
        assert_eq!(
            say(&mut t, "ill be at the mailbox on Mirage Isle", 5500),
            None
        );
        assert_eq!(say(&mut t, "Mirage Isle, by the mailbox", 5600), None);
        // a question is answered twice at most (the first "where?" was the first answer)
        assert!(say(&mut t, "where do we meet?", 6000).is_some());
        assert_eq!(say(&mut t, "which town?", 7000), None);
        assert!(t.is_agreed());
    }

    #[test]
    fn a_seller_without_goods_says_sold_out() {
        let mut t = Talk::seller("Bob", Medium::Channel, 3411, 0, quote(), 1000);
        assert_eq!(
            say(&mut t, "how much", 2000),
            Some(Speech::Decline(Decline::SoldOut))
        );
        assert!(t.is_closed());
    }

    #[test]
    fn a_seller_with_no_place_to_offer_says_nothing_and_does_not_agree() {
        let mut t = seller();
        let s = t.on_line(&parse("ill take 5"), Medium::Channel, 2000, &mut |_| None);
        assert_eq!(s, None);
        assert!(!t.is_agreed());
    }

    // ---- the buyer --------------------------------------------------------------------

    fn buyer() -> Talk {
        // wants 10, pays at most 3200 per unit
        Talk::buyer("Ann", Medium::Channel, 3411, 10, 3200, 1000)
    }

    #[test]
    fn a_buyer_takes_goods_at_a_fair_price_and_haggles_a_little_over_its_ceiling() {
        let mut t = buyer();
        assert_eq!(
            t.open_as_buyer(3000, false),
            Some(Speech::Take {
                qty: 10,
                unit: None
            })
        );
        assert_eq!((t.unit, t.stage), (3000, Stage::Quoted));
        // a little over: one counter at the ceiling
        let mut t = buyer();
        assert_eq!(
            t.open_as_buyer(3800, false),
            Some(Speech::Counter {
                unit: 3200,
                seller: false
            })
        );
        assert!(t.countered);
        // far over: no line at all
        let mut t = buyer();
        assert_eq!(t.open_as_buyer(9000, false), None);
        assert!(t.is_closed());
        // or ask first, but not about a price far over what the buyer pays
        let mut t = buyer();
        assert_eq!(
            t.open_as_buyer(3100, true),
            Some(Speech::Ask { template: 3411 })
        );
        let mut t = buyer();
        assert_eq!(t.open_as_buyer(9000, true), None);
    }

    #[test]
    fn a_buyer_accepts_a_quote_within_its_ceiling_and_counters_once_over_it() {
        let mut t = buyer();
        t.open_as_buyer(3100, true);
        let s = say(&mut t, "3000 ea", 2000);
        assert_eq!(s, None, "a bare number is no money");
        let s = say(&mut t, "30s each, i have 20 copper ore", 2500);
        assert_eq!(
            s,
            Some(Speech::Take {
                qty: 10,
                unit: None
            })
        );
        assert_eq!(t.unit, 3000);
        // too dear: counter at the ceiling, then give up
        let mut t = buyer();
        t.open_as_buyer(3100, true);
        assert_eq!(
            say(&mut t, "40s each", 2000),
            Some(Speech::Counter {
                unit: 3200,
                seller: false
            })
        );
        assert_eq!(
            say(&mut t, "best i can do is 38s ea", 3000),
            Some(Speech::Decline(Decline::Price))
        );
        assert!(t.is_closed());
        // the seller's counter within the ceiling is taken
        let mut t = buyer();
        t.open_as_buyer(3800, false);
        assert_eq!(
            say(&mut t, "best i can do is 32s ea", 2000),
            Some(Speech::Take {
                qty: 10,
                unit: None
            })
        );
        assert_eq!(t.unit, 3200);
    }

    #[test]
    fn the_buyer_reads_the_place_from_the_confirmation() {
        let mut t = buyer();
        t.open_as_buyer(3000, false);
        let s = t.on_line(
            &parse("deal! 10 Copper Ore for 3g total. meet me on Mirage Isle by the mailbox"),
            Medium::Channel,
            2000,
            &mut |q| {
                assert_eq!(
                    q,
                    PlaceQ::Told {
                        peer: "Ann",
                        mirage: true
                    }
                );
                Some(Meet::Mirage)
            },
        );
        assert_eq!(s, Some(Speech::OnMyWay));
        assert!(t.is_agreed());
        assert_eq!(t.meet, Some(Meet::Mirage));
        assert_eq!(t.total(), 30_000);
        // a town is looked up by the callback
        let mut t = buyer();
        t.open_as_buyer(3000, false);
        let s = t.on_line(
            &parse("deal! 10 Copper Ore for 3g total. meet me at the auctioneer in Marianople"),
            Medium::Channel,
            2000,
            &mut |q| {
                assert_eq!(
                    q,
                    PlaceQ::Told {
                        peer: "Ann",
                        mirage: false
                    }
                );
                Some(Meet::Town { x: 5.0, y: 6.0 })
            },
        );
        assert_eq!(s, Some(Speech::OnMyWay));
        assert_eq!(t.meet, Some(Meet::Town { x: 5.0, y: 6.0 }));
    }

    #[test]
    fn a_seller_that_confirms_a_higher_total_than_agreed_is_declined() {
        let mut t = buyer();
        t.open_as_buyer(3000, false);
        // agreed 10 x 30s = 3g, the confirmation says 9g
        let s = say(
            &mut t,
            "deal! 10 copper ore for 9g total. meet me on Mirage Isle by the mailbox",
            2000,
        );
        assert_eq!(s, Some(Speech::Decline(Decline::Price)));
        assert!(t.is_closed() && t.meet.is_none());
    }

    #[test]
    fn a_buyer_whose_seller_is_sold_out_walks_away() {
        let mut t = buyer();
        t.open_as_buyer(3000, false);
        assert_eq!(say(&mut t, "sorry, already sold", 2000), None);
        assert_eq!(t.stage, Stage::Closed(Closed::Declined));
    }

    #[test]
    fn idle_talks_time_out() {
        let mut t = seller();
        assert!(!t.expire(1000 + IDLE_MS));
        assert!(t.expire(1000 + IDLE_MS + 1));
        assert_eq!(t.stage, Stage::Closed(Closed::TimedOut));
        assert!(!t.expire(10_000_000), "already closed");
        // an agreed deal keeps much longer
        let mut t = seller();
        say(&mut t, "ill take 5", 2000);
        assert!(!t.expire(2000 + IDLE_MS + 1));
        assert!(t.expire(2000 + AGREED_MS + 1));
    }

    #[test]
    fn a_conversation_between_two_bots_reaches_a_deal_through_their_own_words() {
        // the seller's quote and counters are read by the buyer, and back
        let mut rng = fastrand::Rng::with_seed(11);
        let words = super::super::lines::Words {
            item: "Copper Ore",
            place: "Marianople",
        };
        for round in 0..60 {
            let mut s = Talk::seller("Ann", Medium::Channel, 3411, 20, quote(), 0);
            let mut b = Talk::buyer("Bob", Medium::Channel, 3411, 10, 2600 + round * 20, 0);
            let mut to_seller = b.open_as_buyer(3000, round % 3 == 0);
            let mut now = 1000;
            for _ in 0..8 {
                let Some(sp) = to_seller.take() else { break };
                now += 1000;
                let text = super::super::lines::render(&sp, &words, &mut rng);
                let reply = s.on_line(&parse(&text), Medium::Channel, now, &mut mirage);
                let Some(sp) = reply else { break };
                now += 1000;
                let text = super::super::lines::render(&sp, &words, &mut rng);
                to_seller = b.on_line(&parse(&text), Medium::Channel, now, &mut mirage);
            }
            // whatever happened, nobody paid over the ceiling or sold under the floor
            if s.is_agreed() {
                assert!(s.unit >= s.limit, "round {round}: {s:?}");
                assert!(
                    b.is_agreed() || b.stage == Stage::Quoted,
                    "round {round}: {b:?}"
                );
                if b.is_agreed() {
                    assert!(
                        b.unit <= b.limit && b.unit == s.unit,
                        "round {round}: {s:?} / {b:?}"
                    );
                }
            }
        }
    }
}
