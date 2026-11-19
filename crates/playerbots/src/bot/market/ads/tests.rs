//! Scripted tests of the advertisement state: lines in, speeches out.

use super::board::Board;
use super::lines::{self, Decline, Meet, Speech, Words};
use super::names::NameIndex;
use super::price::{quote_from_vendor, PriceSource, Quote};
use super::talk::{Closed, Medium, PlaceQ, Role};
use super::*;

const ORE: u32 = 3411;
const HAY: u32 = 3712;

struct Prices;

impl PriceSource for Prices {
    fn quote(&self, template: u32) -> Option<Quote> {
        match template {
            ORE => quote_from_vendor(40, 20),
            HAY => quote_from_vendor(100, 60),
            _ => None,
        }
    }
}

fn names() -> NameIndex {
    NameIndex::build([(ORE, "Copper Ore"), (HAY, "Hay Bale")].into_iter())
}

fn item_name(t: u32) -> String {
    match t {
        ORE => "Copper Ore".into(),
        HAY => "Hay Bale".into(),
        _ => "goods".into(),
    }
}

/// Everything a step needs, owned.
struct Rig {
    me: &'static str,
    board: Board,
    names: NameIndex,
    rng: fastrand::Rng,
    surplus: Vec<Surplus>,
    needs: Vec<Need>,
    money: i64,
    audience: usize,
    rules: Rules,
    bots: Vec<&'static str>,
    wants: Option<(u32, u32)>,
    meet: Meet,
}

impl Rig {
    fn new(me: &'static str) -> Rig {
        Rig {
            me,
            board: Board::default(),
            names: names(),
            rng: fastrand::Rng::with_seed(7),
            surplus: vec![Surplus {
                template: ORE,
                count: 20,
            }],
            needs: Vec::new(),
            money: 100_000,
            audience: 3,
            rules: Rules {
                chat_ads: true,
                sell_to_players: false,
                max_ads_per_hour: 12,
                ad_interval_ms: 900_000,
            },
            bots: vec!["Bob", "Cy"],
            wants: Some((10, 3600)),
            meet: Meet::Mirage,
        }
    }

    fn step(&mut self, ads: &mut AdState, now: u64) {
        let bots = self.bots.clone();
        let is_bot = move |n: &str| bots.iter().any(|b| b.eq_ignore_ascii_case(n));
        let wants = self.wants;
        let interest = move |t: u32, _qty: u32, _ask: u32| wants.filter(|_| t == ORE);
        let meet = self.meet;
        let mut place = move |q: PlaceQ<'_>| match q {
            PlaceQ::Choose { .. } => Some(meet),
            PlaceQ::Told { mirage, .. } => Some(if mirage {
                Meet::Mirage
            } else {
                Meet::Town { x: 1.0, y: 2.0 }
            }),
        };
        let mut env = Env {
            now_ms: now,
            unix_ms: 1_000_000 + now,
            rules: self.rules,
            me: self.me,
            prices: &Prices,
            names: &self.names,
            item_name: &item_name,
            surplus: &self.surplus,
            needs: &self.needs,
            money: self.money,
            audience: self.audience,
            is_bot: &is_bot,
            trusted_companion: &|_| false,
            interest: &interest,
            place: &mut place,
            board: &self.board,
            rng: &mut self.rng,
        };
        ads.step(&mut env);
    }
}

/// Every speech that comes due within the next minute after `now`.
fn said(ads: &mut AdState, now: u64) -> Vec<Due> {
    let mut out = Vec::new();
    for t in (now..now + 60_000).step_by(500) {
        while let Some(d) = ads.next_due(t) {
            out.push(d);
        }
    }
    out
}

#[test]
fn an_unaffordable_order_does_not_block_other_ingredient_ads() {
    let mut rig = Rig::new("Ann");
    rig.money = 600;
    rig.surplus.clear();
    rig.needs = vec![
        Need {
            template: HAY,
            qty: 6,
            order_price: Some(500),
        },
        Need {
            template: ORE,
            qty: 12,
            order_price: Some(20),
        },
    ];
    let mut ads = AdState::default();
    rig.step(&mut ads, 1);
    let listing = ads.listing.expect("an affordable order");
    assert_eq!(
        (listing.template, listing.qty, listing.quote.ask),
        (ORE, 10, 20)
    );
}

#[test]
fn only_talk_about_buying_and_selling_is_kept() {
    let mut a = AdState::default();
    a.on_chat(2, "Bob", "anyone seen the dragon?", 1);
    a.on_chat(0, "Bob", "wts ore", 2);
    a.on_chat(-3, "Bob", "hi there", 3);
    assert!(!a.busy());
    a.on_chat(2, "Bob", "WTS Copper Ore x5 10s ea", 4);
    a.on_chat(-3, "Bob", "how much", 5);
    assert!(a.busy());
    assert!(worth_hearing(2, "wtb ore"));
    assert!(!worth_hearing(0, "wtb ore"));
    assert!(!worth_hearing(2, "good morning"));
}

#[test]
fn a_bot_with_goods_and_an_audience_posts_one_ad_then_waits() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    let l = *a.listing().expect("posted");
    assert_eq!((l.side, l.template, l.qty), (Side::Sell, ORE, 20));
    assert!(l.quote.floor <= l.quote.ask);
    let d = said(&mut a, 10_000);
    assert_eq!(d.len(), 1);
    assert!(matches!(
        d[0].speech,
        Speech::AdSell {
            template: ORE,
            qty: 20,
            ..
        }
    ));
    assert_eq!(&*d[0].to, "");
    assert_eq!(d[0].medium, Medium::Channel);
    assert_eq!(a.posted, 1);
    // the text is a selling ad whatever wording is picked
    for n in 0..lines::variants(&d[0].speech) {
        let t = lines::render_nth(
            &d[0].speech,
            &Words {
                item: "Copper Ore",
                place: "",
            },
            n,
        );
        assert!(grammar::parse(&t).sell_ad, "{t}");
    }
    // not again while it stands, nor after it lapsed until the interval is over
    r.step(&mut a, 60_000);
    assert_eq!(a.posted, 1);
    r.step(&mut a, 10_000 + LISTING_MS + 1);
    assert!(a.listing().is_none());
    assert_eq!(
        a.posted, 1,
        "the interval (15 minutes, less a quarter) is not over"
    );
    r.step(&mut a, 10_000 + 1_200_000);
    assert_eq!(a.posted, 2);
}

#[test]
fn nobody_to_read_it_nothing_to_sell_or_no_hourly_slot_means_no_ad() {
    // nobody in the zone group
    let mut r = Rig::new("Ann");
    r.audience = 0;
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    assert!(a.listing().is_none());
    // nothing to sell and nothing to buy
    let mut r = Rig::new("Ann");
    r.surplus.clear();
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    assert!(a.listing().is_none());
    // switched off
    let mut r = Rig::new("Ann");
    r.rules.chat_ads = false;
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    assert!(a.listing().is_none());
    // the hourly limit is shared by all bots
    let mut r = Rig::new("Ann");
    r.rules.max_ads_per_hour = 2;
    let mut posted = 0;
    for i in 0..5 {
        let mut a = AdState::default();
        r.step(&mut a, 10_000 + i);
        posted += a.posted;
    }
    assert_eq!(posted, 2);
    // and 0 means none
    let mut r = Rig::new("Ann");
    r.rules.max_ads_per_hour = 0;
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    assert_eq!(a.posted, 0);
}

#[test]
fn a_bot_that_needs_goods_posts_a_wanted_ad_it_can_pay() {
    let mut r = Rig::new("Ann");
    r.surplus.clear();
    r.needs = vec![Need {
        template: HAY,
        qty: 10,
        order_price: None,
    }];
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    let l = *a.listing().expect("posted");
    assert_eq!((l.side, l.template), (Side::Buy, HAY));
    let d = said(&mut a, 10_000);
    let Speech::AdBuy { unit, qty, .. } = d[0].speech else {
        panic!("{:?}", d[0].speech);
    };
    assert!(qty > 0 && qty <= 10);
    assert!(unit <= 100, "never over the vendor's price: {unit}");
    assert!(i64::from(unit) * i64::from(qty) <= r.money / 3 + i64::from(unit));
    // a poor bot does not post a wanted-ad
    let mut r2 = Rig::new("Ann");
    r2.surplus.clear();
    r2.needs = vec![Need {
        template: HAY,
        qty: 10,
        order_price: None,
    }];
    r2.money = 100;
    let mut a = AdState::default();
    r2.step(&mut a, 10_000);
    assert!(a.listing().is_none());
}

#[test]
fn a_guild_order_posts_the_agreed_price_without_a_random_spread() {
    let mut r = Rig::new("Ann");
    r.surplus.clear();
    r.needs = vec![Need {
        template: HAY,
        qty: 3,
        order_price: Some(90),
    }];
    let mut a = AdState::default();
    r.step(&mut a, 10_000);
    let listing = a.listing().expect("posted");
    assert_eq!(listing.quote.ask, 90);
    assert_eq!(listing.quote.ceiling, 90);
    let speech = said(&mut a, 10_000);
    assert!(matches!(
        speech[0].speech,
        Speech::AdBuy {
            unit: 90,
            qty: 3,
            ..
        }
    ));
}

fn post(r: &mut Rig, a: &mut AdState) {
    r.step(a, 10_000);
    said(a, 10_000);
}

#[test]
fn a_bot_buyer_who_answers_the_ad_gets_a_quote_then_a_deal_with_the_meeting_place() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    post(&mut r, &mut a);
    // Bob, a bot, replies in the channel
    a.on_chat(2, "Bob", "how much for the ore?", 20_000);
    assert!(
        !a.claims_whisper("Bob"),
        "a channel line is nobody's whisper"
    );
    r.step(&mut a, 20_100);
    let d = said(&mut a, 20_100);
    assert_eq!(d.len(), 1);
    assert_eq!(&*d[0].to, "Bob");
    assert_eq!(d[0].medium, Medium::Channel);
    let Speech::Quote {
        template,
        qty,
        unit,
    } = d[0].speech
    else {
        panic!("{:?}", d[0].speech);
    };
    assert_eq!((template, qty), (ORE, 20));
    assert_eq!(unit, a.listing().unwrap().quote.ask);
    assert!(a.talking());
    // he takes 8 at the asked price
    a.on_chat(2, "Bob", "Ann: ok ill take 8", 30_000);
    r.step(&mut a, 30_100);
    let d = said(&mut a, 30_100);
    assert!(
        matches!(
            d[0].speech,
            Speech::Confirm {
                qty: 8,
                meet: Meet::Mirage,
                ..
            }
        ),
        "{:?}",
        d[0].speech
    );
    let deal = a.deal().expect("a deal");
    assert_eq!(
        (deal.role, &*deal.peer, deal.qty, deal.unit),
        (Role::Seller, "Bob", 8, unit)
    );
    assert_eq!(deal.total(), i64::from(unit) * 8);
    assert_eq!(a.agreed, 1);
    // the board knows where the two meet
    assert_eq!(
        r.board.meet_for("Ann", "Bob", 1_100_000),
        Some(Meet::Mirage)
    );
    // the trade is made: the units leave the listing
    a.finish_deal(Closed::Done);
    assert!(a.deal().is_none());
    assert_eq!(a.listing().map(|l| l.qty), Some(12));
}

#[test]
fn a_second_buyer_while_a_deal_is_pending_is_told_it_is_gone() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    post(&mut r, &mut a);
    a.on_chat(2, "Bob", "Ann: ill take 8", 20_000);
    r.step(&mut a, 20_100);
    assert!(a.deal().is_some());
    said(&mut a, 20_100);
    a.on_chat(2, "Cy", "Ann: how much for the ore", 30_000);
    r.step(&mut a, 30_100);
    let d = said(&mut a, 30_100);
    assert_eq!(d.len(), 1);
    assert_eq!(&*d[0].to, "Cy");
    assert_eq!(d[0].speech, Speech::Decline(Decline::SoldOut));
    assert_eq!(a.deal().map(|d| &*d.peer), Some("Bob"));
}

#[test]
fn a_whisper_in_a_talk_is_claimed_and_an_unrelated_one_is_not() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    // no listing: nobody's whisper is ours
    a.on_chat(-3, "Bob", "how much for the ore", 1_000);
    assert!(!a.claims_whisper("Bob"));
    post(&mut r, &mut a);
    a.on_chat(-3, "Bob", "how much for the ore", 20_000);
    assert!(a.claims_whisper("Bob"));
    assert!(!a.claims_whisper("Cy"));
    r.step(&mut a, 20_100);
    let d = said(&mut a, 20_100);
    assert_eq!(
        d[0].medium,
        Medium::Whisper,
        "a whisper is answered by whisper"
    );
    // after the step the open talk keeps the claim
    assert!(a.claims_whisper("bob"));
    // small talk is not claimed
    a.on_chat(-3, "Cy", "hello there, nice cloak", 21_000);
    assert!(!a.claims_whisper("Cy"));
}

#[test]
fn players_are_only_served_when_selling_to_them_is_on() {
    for (allowed, expect_quote) in [(false, false), (true, true)] {
        let mut r = Rig::new("Ann");
        r.rules.sell_to_players = allowed;
        let mut a = AdState::default();
        post(&mut r, &mut a);
        // "Zed" is not one of our bots
        a.on_chat(2, "Zed", "Ann: how much for the ore?", 20_000);
        r.step(&mut a, 20_100);
        let d = said(&mut a, 20_100);
        assert_eq!(d.len(), 1, "{allowed}");
        if expect_quote {
            assert!(matches!(d[0].speech, Speech::Quote { .. }));
            assert!(a.talking());
        } else {
            // a polite no, and no deal can come of it
            assert_eq!(d[0].speech, Speech::Decline(Decline::SoldOut));
            assert!(!a.talking());
            a.on_chat(2, "Zed", "Ann: ok ill take 8", 30_000);
            r.step(&mut a, 30_100);
            assert!(a.deal().is_none());
        }
    }
}

#[test]
fn lines_for_other_people_or_about_other_goods_are_not_ours() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    post(&mut r, &mut a);
    a.on_chat(2, "Bob", "Cy: how much for the ore", 20_000);
    a.on_chat(2, "Bob", "how much for the hay", 20_001);
    a.on_chat(2, "Bob", "ok deal", 20_002);
    r.step(&mut a, 20_100);
    assert!(said(&mut a, 20_100).is_empty());
    assert!(!a.talking());
}

#[test]
fn a_buyer_bot_answers_a_selling_ad_it_has_a_use_for() {
    // Ann wants ore (wants Some), Bob advertises it at 30s: she takes it
    let mut taken = 0;
    for seed in 0..40 {
        let mut r = Rig::new("Ann");
        r.rng = fastrand::Rng::with_seed(seed);
        r.surplus.clear();
        r.bots = vec!["Bob"];
        let mut a = AdState::default();
        a.on_chat(2, "Bob", "WTS Copper Ore x20 - 30s ea, PST", 1_000);
        r.step(&mut a, 1_100);
        for d in said(&mut a, 1_100) {
            assert_eq!(&*d.to, "Bob");
            assert!(
                matches!(
                    d.speech,
                    Speech::Take {
                        qty: 10,
                        unit: None
                    } | Speech::Ask { template: ORE }
                ),
                "{:?}",
                d.speech
            );
            taken += 1;
        }
    }
    assert!(
        (8..=35).contains(&taken),
        "sometimes, not always: {taken} of 40"
    );
    // no use for hay: silence
    let mut r = Rig::new("Ann");
    r.surplus.clear();
    let mut a = AdState::default();
    a.on_chat(2, "Bob", "WTS Hay Bale x20 - 30s ea", 1_000);
    r.step(&mut a, 1_100);
    assert!(said(&mut a, 1_100).is_empty());
    // a price far over the ceiling: silence
    let mut r = Rig::new("Ann");
    r.surplus.clear();
    r.rng = fastrand::Rng::with_seed(1);
    for i in 0..20 {
        let mut a = AdState::default();
        a.on_chat(2, "Bob", "WTS Copper Ore x20 - 90g ea", 1_000 + i);
        r.step(&mut a, 1_100);
        assert!(said(&mut a, 1_100).is_empty());
    }
}

#[test]
fn a_seller_bot_answers_a_wanted_ad_it_can_serve_at_a_price_over_its_floor() {
    let mut answered = 0;
    for seed in 0..40 {
        let mut r = Rig::new("Ann");
        r.rng = fastrand::Rng::with_seed(seed);
        // (nobody to read an advertisement of its own)
        r.audience = 0;
        let mut a = AdState::default();
        // floor is 24c, ask 31c
        a.on_chat(2, "Bob", "WTB Copper Ore x10, paying 40c ea. PST", 1_000);
        r.step(&mut a, 1_100);
        for d in said(&mut a, 1_100) {
            let Speech::Offer {
                template,
                qty,
                unit,
            } = d.speech
            else {
                panic!("{:?}", d.speech);
            };
            assert_eq!((template, qty), (ORE, 10));
            assert_eq!(unit, 40, "the buyer's price is above the ask");
            answered += 1;
        }
        // under the floor: no answer
        let mut a = AdState::default();
        a.on_chat(2, "Bob", "WTB Copper Ore x10, paying 5c ea. PST", 1_000);
        r.step(&mut a, 1_100);
        assert!(said(&mut a, 1_100).is_empty());
        // something it does not have: no answer
        let mut a = AdState::default();
        a.on_chat(2, "Bob", "WTB Hay Bale x10, paying 90c ea. PST", 1_000);
        r.step(&mut a, 1_100);
        assert!(said(&mut a, 1_100).is_empty());
    }
    assert!((8..=35).contains(&answered), "sometimes: {answered} of 40");
}

#[test]
fn a_bot_with_its_own_advertisement_still_answers_the_ad_that_matches() {
    let mut r = Rig::new("Ann");
    r.surplus = vec![
        Surplus {
            template: ORE,
            count: 20,
        },
        Surplus {
            template: HAY,
            count: 5,
        },
    ];
    let mut a = AdState::default();
    r.step(&mut a, 1_000);
    assert!(a.listing().is_some(), "it advertised");
    let own = a.listing().map(|l| l.template).unwrap_or(0);
    // somebody wants the other item: the bot answers although its own advertisement stands
    let other = if own == ORE { HAY } else { ORE };
    let name = item_name(other);
    let mut answered = 0;
    for seed in 0..40 {
        let mut r2 = Rig::new("Ann");
        r2.surplus = r.surplus.clone();
        r2.rng = fastrand::Rng::with_seed(seed);
        r2.audience = 0;
        let mut b = AdState::default();
        r2.step(&mut b, 1_000);
        b.listing = Some(Listing {
            side: Side::Sell,
            template: own,
            qty: 5,
            quote: Quote {
                ask: 30,
                floor: 20,
                ceiling: 40,
            },
            posted_ms: 1_000,
        });
        b.on_chat(
            2,
            "Bob",
            &format!("WTB {name} x3, paying 99s ea. PST"),
            2_000,
        );
        r2.step(&mut b, 2_100);
        if said(&mut b, 2_100)
            .iter()
            .any(|d| matches!(d.speech, Speech::Offer { template, .. } if template == other))
        {
            answered += 1;
        }
    }
    assert!(answered > 5, "it answers now and then: {answered} of 40");
}

#[test]
fn of_two_sellers_that_want_each_others_goods_only_one_answers() {
    // Ann and Zed both advertise hay and both want ore; Bob (not selling) advertises ore. Zed
    // sorts after Ann, so Ann leaves the selling advertisement of Zed alone and Zed does not
    // leave hers: they never answer each other at the same moment.
    let listing = Listing {
        side: Side::Sell,
        template: HAY,
        qty: 5,
        quote: Quote {
            ask: 30,
            floor: 20,
            ceiling: 40,
        },
        posted_ms: 1_000,
    };
    let answered = |me: &'static str, other: &'static str| {
        let mut n = 0;
        for seed in 0..40 {
            let mut r = Rig::new(me);
            r.rng = fastrand::Rng::with_seed(seed);
            r.surplus.clear();
            r.audience = 0;
            r.bots = vec![other];
            let mut a = AdState {
                listing: Some(listing),
                ..AdState::default()
            };
            a.on_chat(2, other, "WTS Copper Ore x20 - 30s ea, PST", 1_000);
            r.step(&mut a, 1_100);
            n += said(&mut a, 1_100).len();
        }
        n
    };
    assert_eq!(
        answered("Ann", "Zed"),
        0,
        "the one that sorts first stays out"
    );
    assert!(answered("Zed", "Ann") > 5, "the other one answers");
}

#[test]
fn nothing_is_said_faster_than_a_person_types() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    post(&mut r, &mut a);
    a.on_chat(2, "Bob", "how much for the ore?", 20_000);
    r.step(&mut a, 20_000);
    // not at once
    assert!(a.next_due(20_000).is_none());
    assert!(a.next_due(20_500).is_none());
    let first = a.next_due(25_000).expect("due after a few seconds");
    assert!(matches!(first.speech, Speech::Quote { .. }));
    // and not two lines within the gap
    a.on_chat(2, "Bob", "ok ill take 5", 25_000);
    r.step(&mut a, 25_000);
    assert!(a.next_due(25_100).is_none());
}

#[test]
fn two_bots_talk_to_a_deal_through_their_own_lines() {
    // Ann sells, Cy buys; every line passes through the chat text
    let mut deals = 0;
    for seed in 0..30u64 {
        let mut ann = Rig::new("Ann");
        ann.rng = fastrand::Rng::with_seed(seed);
        ann.bots = vec!["Cy"];
        let mut cy = Rig::new("Cy");
        cy.rng = fastrand::Rng::with_seed(seed + 100);
        cy.surplus.clear();
        cy.bots = vec!["Ann"];
        cy.audience = 0;
        let (mut a, mut c) = (AdState::default(), AdState::default());
        let mut now = 10_000;
        ann.step(&mut a, now);
        let words = Words {
            item: "Copper Ore",
            place: "Marianople",
        };
        let mut wire: Vec<(bool, Due)> = Vec::new(); // (from ann, due)
        for _ in 0..40 {
            now += 3_000;
            ann.step(&mut a, now);
            cy.step(&mut c, now);
            while let Some(d) = a.next_due(now) {
                wire.push((true, d));
            }
            while let Some(d) = c.next_due(now) {
                wire.push((false, d));
            }
            for (from_ann, d) in wire.drain(..) {
                let t = lines::render(&d.speech, &words, &mut ann.rng);
                let (kind, text) = match d.medium {
                    Medium::Whisper => (-3, t),
                    Medium::Channel => (
                        2,
                        lines::addressed((!d.to.is_empty()).then_some(&*d.to), &t),
                    ),
                };
                if from_ann {
                    c.on_chat(kind, "Ann", &text, now);
                } else {
                    a.on_chat(kind, "Cy", &text, now);
                }
            }
            if a.deal().is_some() && c.deal().is_some() {
                break;
            }
        }
        // Cy only answers sometimes; when both agreed they agree on everything
        if let (Some(sa), Some(cb)) = (a.deal(), c.deal()) {
            deals += 1;
            assert_eq!(sa.role, Role::Seller);
            assert_eq!(cb.role, Role::Buyer);
            assert_eq!(
                (sa.qty, sa.unit, sa.meet),
                (cb.qty, cb.unit, cb.meet),
                "seed {seed}"
            );
            assert_eq!(&*sa.peer, "Cy");
            assert_eq!(&*cb.peer, "Ann");
            assert!(
                i64::from(sa.unit) >= 24,
                "never under the floor: {}",
                sa.unit
            );
        }
    }
    assert!(deals >= 5, "only {deals} of 30 pairs reached a deal");
}

#[test]
fn a_deal_that_is_not_traded_in_time_falls_through() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    post(&mut r, &mut a);
    a.on_chat(2, "Bob", "Ann: ill take 8", 20_000);
    r.step(&mut a, 20_100);
    assert!(a.deal().is_some());
    r.step(&mut a, 20_100 + talk::AGREED_MS + 1);
    assert!(a.deal().is_none());
    assert!(!a.talking());
}

#[test]
fn the_state_stays_bounded() {
    let mut r = Rig::new("Ann");
    let mut a = AdState::default();
    for i in 0..50 {
        a.on_chat(2, &format!("Bot{i}"), "WTS Copper Ore x20 - 30s ea", i);
    }
    assert!(a.inbox.len() <= 6);
    r.step(&mut a, 10_000);
    assert!(a.talks.len() <= 3 && a.due.len() <= 4);
}
