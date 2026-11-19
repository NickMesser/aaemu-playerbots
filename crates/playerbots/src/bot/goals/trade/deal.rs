//! Safe trades: what a bot agreed to, and the check it makes before it locks and before it
//! confirms.
//!
//! The server lets both sides put up anything, and clears the locks and confirmations whenever
//! either offer changes, so a bot that locks "when the other side has put something up" can be
//! robbed: a buyer puts up one copper, the bot locks, the buyer confirms. A [`Deal`] states what the
//! bot gives and what it demands in return; [`verify`] compares it with the window as the server
//! confirmed it, and the trade goal locks only on [`Verdict::Ok`], again after every unlock (the
//! offers may be different now), and cancels on [`Verdict::Refuse`].
//!
//! The bot's own offer is part of the check: whatever stands in the window on its side must be
//! exactly what the deal says it gives, nothing else.

use smallvec::SmallVec;

use super::log::Window;
use super::Plan;

/// Whether the character in the window is the one the bot agreed to trade with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The unit that opened the window is the agreed character.
    Match,
    /// It is somebody else.
    Mismatch,
    /// The unit is not in view (a bot-to-bot trade the coordinator arranged is trusted then).
    Unknown,
}

/// What the bot demands of the other side's offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terms {
    /// Any offer that is not empty: the random trades between two of our own bots, where both
    /// sides give away cheap goods.
    Generous,
    /// An offer worth at least this many copper (vendor refund values plus money): what a trade
    /// with a player that was not agreed in chat must be.
    Worth(i64),
    /// Exactly these goods (`(template, units)`; nothing else, no more units) and at least
    /// `money` copper: what the chat agreed.
    Exact {
        /// The goods the other side puts up.
        items: SmallVec<[(u32, i32); 2]>,
        /// The copper the other side puts up, at least.
        money: i32,
        /// Whether the trade must reject a higher payment too.
        fixed_payment: bool,
    },
}

/// What a bot gives and what it takes for it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deal {
    /// What the bot puts up (bag slot, template, units, copper).
    pub give: Plan,
    /// What the other side must put up.
    pub terms: Terms,
    /// The window must be known to belong to the agreed character ([`Identity::Match`]); an
    /// unknown unit is not enough. True for deals made in chat.
    pub strict: bool,
}

impl Deal {
    /// The deal of a random trade with another of our bots: it gets anything.
    pub fn generous(give: Plan) -> Deal {
        Deal {
            give,
            terms: Terms::Generous,
            strict: false,
        }
    }

    /// A deal made in chat: `give` for exactly `items` and at least `money` copper.
    pub fn agreed(give: Plan, items: &[(u32, i32)], money: i32) -> Deal {
        Deal {
            give,
            terms: Terms::Exact {
                items: items.iter().copied().collect(),
                money,
                fixed_payment: false,
            },
            strict: true,
        }
    }

    /// An agreed deal whose payment must match exactly.
    pub fn fixed(give: Plan, items: &[(u32, i32)], money: i32) -> Deal {
        Deal {
            give,
            terms: Terms::Exact {
                items: items.iter().copied().collect(),
                money,
                fixed_payment: true,
            },
            strict: true,
        }
    }

    /// The deal of a trade nobody agreed to in chat with a character that is not one of ours: the
    /// other side must put up at least `percent` percent of the value of `give`, by `worth`
    /// (copper per unit of a template).
    pub fn fair(give: Plan, percent: i64, worth: &dyn Fn(u32) -> i64) -> Deal {
        let mine =
            i64::from(give.money.max(0)) + worth(give.template) * i64::from(give.amount.max(0));
        Deal {
            give,
            terms: Terms::Worth(mine * percent / 100),
            strict: false,
        }
    }
}

/// Why the bot will not go on with a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The window belongs to somebody else than the agreed character.
    WrongPartner,
    /// The bot's own offer is not what the deal says it gives.
    MineChanged,
    /// The other side offers an item the deal does not name.
    ExtraItem,
    /// The other side offers more units of an item than the deal names.
    TooMany,
    /// A fixed-payment deal does not accept extra copper.
    ExtraMoney,
    /// The other side's offer is worth less than the bot asks.
    TooLittle,
}

impl Refusal {
    /// A short phrase for the log.
    pub fn why(self) -> &'static str {
        match self {
            Refusal::WrongPartner => "the window is not the agreed partner's",
            Refusal::MineChanged => "my own offer is not what we agreed",
            Refusal::ExtraItem => "the partner offers an item that was not agreed",
            Refusal::TooMany => "the partner offers more of an item than agreed",
            Refusal::ExtraMoney => "the partner offers more copper than agreed",
            Refusal::TooLittle => "the partner's offer is worth too little",
        }
    }
}

/// The answer of [`verify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not there yet: the bot's own put-up is on its way or the other side has not finished
    /// putting up what was agreed. Nothing wrong; do not lock yet.
    Wait,
    /// The window holds exactly the deal: lock (or, locked on both sides, confirm).
    Ok,
    /// The window does not hold the deal; do not lock. A refusal that lasts, or one after a lock,
    /// ends the trade.
    Refuse(Refusal),
}

/// Compares the window as the server confirmed it with the deal.
///
/// `worth` is copper per unit of an item template (for [`Terms::Worth`]).
pub fn verify(deal: &Deal, w: &Window, who: Identity, worth: &dyn Fn(u32) -> i64) -> Verdict {
    match who {
        Identity::Mismatch => return Verdict::Refuse(Refusal::WrongPartner),
        Identity::Unknown if deal.strict => return Verdict::Refuse(Refusal::WrongPartner),
        _ => {}
    }
    match own_offer(deal, w) {
        Own::Changed => return Verdict::Refuse(Refusal::MineChanged),
        Own::Pending => return Verdict::Wait,
        Own::Standing => {}
    }
    let other = &w.other;
    if other.is_empty() {
        return Verdict::Wait;
    }
    match &deal.terms {
        Terms::Generous => Verdict::Ok,
        Terms::Worth(min) => {
            let value = i64::from(other.money.max(0))
                + other
                    .items
                    .iter()
                    .map(|p| worth(p.template) * i64::from(p.amount.max(0)))
                    .sum::<i64>();
            if value >= *min {
                Verdict::Ok
            } else {
                Verdict::Refuse(Refusal::TooLittle)
            }
        }
        Terms::Exact {
            items,
            money,
            fixed_payment,
        } => {
            if *fixed_payment && other.money > *money {
                return Verdict::Refuse(Refusal::ExtraMoney);
            }
            exact(items, *money, w)
        }
    }
}

/// What the window says about the bot's own side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Own {
    /// Exactly the deal's `give`.
    Standing,
    /// Part of it is still on its way.
    Pending,
    /// Something else is there.
    Changed,
}

/// Compares the bot's own side of the window with what the deal says it gives.
pub fn own_offer(deal: &Deal, w: &Window) -> Own {
    let give = &deal.give;
    let mine = &w.mine;
    let mut pending = false;
    // the items: only the planned stack, in the planned amount
    if give.amount > 0 {
        match mine.items.as_slice() {
            [] => pending = true,
            [p] if p.slot == give.slot && p.amount == give.amount => {}
            _ => return Own::Changed,
        }
    } else if !mine.items.is_empty() {
        return Own::Changed;
    }
    // the money: the planned amount (nothing is put up before it is sent)
    if mine.money != give.money.max(0) {
        if mine.money == 0 && give.money > 0 {
            pending = true;
        } else {
            return Own::Changed;
        }
    }
    if pending {
        Own::Pending
    } else {
        Own::Standing
    }
}

fn exact(want: &[(u32, i32)], money: i32, w: &Window) -> Verdict {
    let other = &w.other;
    // every item the partner put up must be one of the deal, and never more units than agreed
    let mut have: SmallVec<[(u32, i32); 4]> = SmallVec::new();
    for p in &other.items {
        match have.iter_mut().find(|h| h.0 == p.template) {
            Some(h) => h.1 = h.1.saturating_add(p.amount),
            None => have.push((p.template, p.amount)),
        }
    }
    for &(template, units) in &have {
        match want.iter().find(|x| x.0 == template) {
            None => return Verdict::Refuse(Refusal::ExtraItem),
            Some(x) if units > x.1 => return Verdict::Refuse(Refusal::TooMany),
            Some(_) => {}
        }
    }
    let complete = want
        .iter()
        .all(|&(template, units)| have.iter().any(|h| h.0 == template && h.1 == units));
    if complete && other.money >= money {
        Verdict::Ok
    } else {
        // (short on units or copper: the partner may still be putting it up)
        Verdict::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::super::log::{Offer, Put};
    use super::*;

    fn window(mine: Offer, other: Offer) -> Window {
        let mut w = Window::new(77, 1000);
        w.mine = mine;
        w.other = other;
        w
    }

    fn mine_ore(slot: u8, amount: i32) -> Offer {
        Offer {
            items: [Put {
                slot,
                template: 0,
                amount,
            }]
            .into_iter()
            .collect(),
            money: 0,
        }
    }

    fn theirs(items: &[(u32, i32)], money: i32) -> Offer {
        Offer {
            items: items
                .iter()
                .map(|&(template, amount)| Put {
                    slot: 0,
                    template,
                    amount,
                })
                .collect(),
            money,
        }
    }

    fn sell_ore() -> Deal {
        // 10 copper ore from slot 4 for 3000 copper
        Deal::agreed(
            Plan {
                slot: 4,
                template: 3411,
                amount: 10,
                money: 0,
            },
            &[],
            3000,
        )
    }

    fn no_worth(_: u32) -> i64 {
        0
    }

    #[test]
    fn a_seller_locks_only_for_the_agreed_money() {
        let d = sell_ore();
        let v = |o: Offer| verify(&d, &window(mine_ore(4, 10), o), Identity::Match, &no_worth);
        assert_eq!(v(theirs(&[], 3000)), Verdict::Ok);
        assert_eq!(v(theirs(&[], 5000)), Verdict::Ok, "more is fine");
        // nothing yet, or less than the price: wait, never lock
        assert_eq!(v(theirs(&[], 0)), Verdict::Wait);
        assert_eq!(v(theirs(&[], 1)), Verdict::Wait, "one copper is no payment");
        assert_eq!(v(theirs(&[], 2999)), Verdict::Wait);
    }

    #[test]
    fn a_fixed_trade_refuses_extra_money() {
        let d = Deal::fixed(
            Plan {
                money: 40,
                ..Plan::default()
            },
            &[(8008, 3)],
            0,
        );
        let mine = Offer {
            money: 40,
            ..Offer::default()
        };
        let check = |items: &[(u32, i32)], money| {
            let w = window(mine.clone(), theirs(items, money));
            verify(&d, &w, Identity::Match, &no_worth)
        };
        assert_eq!(check(&[(8008, 2)], 0), Verdict::Wait);
        assert_eq!(check(&[(8008, 3)], 1), Verdict::Refuse(Refusal::ExtraMoney));
        assert_eq!(check(&[(8008, 3)], 0), Verdict::Ok);
    }

    #[test]
    fn a_swapped_or_extra_item_is_refused() {
        let d = sell_ore();
        let v = |o: Offer| verify(&d, &window(mine_ore(4, 10), o), Identity::Match, &no_worth);
        // the price plus a junk item nobody agreed to
        assert_eq!(
            v(theirs(&[(9999, 1)], 3000)),
            Verdict::Refuse(Refusal::ExtraItem)
        );
        // an item instead of the money
        assert_eq!(
            v(theirs(&[(7765, 5)], 0)),
            Verdict::Refuse(Refusal::ExtraItem)
        );
    }

    #[test]
    fn a_buyer_demands_exactly_the_goods() {
        // pays 3000 for 10 copper ore
        let d = Deal::agreed(
            Plan {
                slot: 0,
                template: 0,
                amount: 0,
                money: 3000,
            },
            &[(3411, 10)],
            0,
        );
        let mine = Offer {
            items: Default::default(),
            money: 3000,
        };
        let v = |o: Offer| verify(&d, &window(mine.clone(), o), Identity::Match, &no_worth);
        assert_eq!(v(theirs(&[(3411, 10)], 0)), Verdict::Ok);
        // two stacks that add up are fine
        assert_eq!(v(theirs(&[(3411, 6), (3411, 4)], 0)), Verdict::Ok);
        // too few units: wait; the wrong item or too many units: refuse
        assert_eq!(v(theirs(&[(3411, 9)], 0)), Verdict::Wait);
        assert_eq!(v(theirs(&[], 0)), Verdict::Wait);
        assert_eq!(
            v(theirs(&[(3712, 10)], 0)),
            Verdict::Refuse(Refusal::ExtraItem)
        );
        assert_eq!(
            v(theirs(&[(3411, 10), (3712, 1)], 0)),
            Verdict::Refuse(Refusal::ExtraItem)
        );
        assert_eq!(
            v(theirs(&[(3411, 11)], 0)),
            Verdict::Refuse(Refusal::TooMany)
        );
    }

    #[test]
    fn the_partner_must_be_the_agreed_character() {
        let d = sell_ore();
        let w = window(mine_ore(4, 10), theirs(&[], 3000));
        assert_eq!(verify(&d, &w, Identity::Match, &no_worth), Verdict::Ok);
        assert_eq!(
            verify(&d, &w, Identity::Mismatch, &no_worth),
            Verdict::Refuse(Refusal::WrongPartner)
        );
        // a deal made in chat does not trust a unit it cannot see
        assert_eq!(
            verify(&d, &w, Identity::Unknown, &no_worth),
            Verdict::Refuse(Refusal::WrongPartner)
        );
        // a coordinator pairing between our bots does
        let g = Deal::generous(d.give);
        assert_eq!(verify(&g, &w, Identity::Unknown, &no_worth), Verdict::Ok);
        assert_eq!(
            verify(&g, &w, Identity::Mismatch, &no_worth),
            Verdict::Refuse(Refusal::WrongPartner)
        );
    }

    #[test]
    fn the_bots_own_offer_must_stay_what_was_agreed() {
        let d = sell_ore();
        let pay = theirs(&[], 3000);
        let v = |mine: Offer| verify(&d, &window(mine, pay.clone()), Identity::Match, &no_worth);
        assert_eq!(v(mine_ore(4, 10)), Verdict::Ok);
        // not put up yet
        assert_eq!(v(Offer::default()), Verdict::Wait);
        // another slot, another amount, a second item, money nobody planned
        assert_eq!(v(mine_ore(5, 10)), Verdict::Refuse(Refusal::MineChanged));
        assert_eq!(v(mine_ore(4, 3)), Verdict::Refuse(Refusal::MineChanged));
        let mut two = mine_ore(4, 10);
        two.items.push(Put {
            slot: 6,
            template: 0,
            amount: 1,
        });
        assert_eq!(v(two), Verdict::Refuse(Refusal::MineChanged));
        let mut money = mine_ore(4, 10);
        money.money = 5;
        assert_eq!(v(money), Verdict::Refuse(Refusal::MineChanged));
    }

    #[test]
    fn a_buyers_money_is_checked_too() {
        let d = Deal::agreed(
            Plan {
                slot: 0,
                template: 0,
                amount: 0,
                money: 3000,
            },
            &[(3411, 10)],
            0,
        );
        let o = theirs(&[(3411, 10)], 0);
        let v = |m: i32| {
            verify(
                &d,
                &window(
                    Offer {
                        items: Default::default(),
                        money: m,
                    },
                    o.clone(),
                ),
                Identity::Match,
                &no_worth,
            )
        };
        assert_eq!(v(3000), Verdict::Ok);
        assert_eq!(v(0), Verdict::Wait, "the money is still on its way");
        assert_eq!(v(2000), Verdict::Refuse(Refusal::MineChanged));
    }

    #[test]
    fn a_fair_deal_asks_for_a_share_of_what_the_bot_gives() {
        let worth = |t: u32| if t == 3411 { 20 } else { 5 };
        let give = Plan {
            slot: 4,
            template: 3411,
            amount: 10,
            money: 100,
        };
        // 10 ore at 20 plus 100 copper = 300; at 60 percent the partner owes 180
        let d = Deal::fair(give, 60, &worth);
        assert_eq!(d.terms, Terms::Worth(180));
        assert!(!d.strict);
        let mut mine = mine_ore(4, 10);
        mine.money = 100;
        let v = |o: Offer| verify(&d, &window(mine.clone(), o), Identity::Unknown, &worth);
        assert_eq!(v(theirs(&[], 180)), Verdict::Ok);
        assert_eq!(v(theirs(&[(7765, 40)], 0)), Verdict::Ok, "40 x 5 = 200");
        assert_eq!(
            v(theirs(&[(7765, 1)], 1)),
            Verdict::Refuse(Refusal::TooLittle)
        );
        assert_eq!(v(theirs(&[], 0)), Verdict::Wait);
    }

    #[test]
    fn a_generous_deal_takes_anything_but_nothing() {
        let d = Deal::generous(Plan {
            slot: 4,
            template: 3411,
            amount: 3,
            money: 0,
        });
        let v = |o: Offer| verify(&d, &window(mine_ore(4, 3), o), Identity::Match, &no_worth);
        assert_eq!(v(theirs(&[], 0)), Verdict::Wait);
        assert_eq!(v(theirs(&[(1, 1)], 0)), Verdict::Ok);
        assert_eq!(v(theirs(&[], 1)), Verdict::Ok);
    }
}
