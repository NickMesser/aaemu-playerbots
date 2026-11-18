//! Which crop to grow.
//!
//! A bot that farms plants what the server is short of first: the items the operator or the
//! population director wishes for (`ScarcityView::wished`), then the items the bots' price notes
//! and the trade chat say are scarce (`ScarcityView::scarcity`), and otherwise what pays best per
//! hour. Every crop the data knows ([`Crop`]) that the bot can plant on dry land is [evaluated]:
//!
//! ```text
//! score = (0.05 + profit + need) * time * reach * held
//!   profit = (value of the produce - half the seed price) per hour / 2000 copper, in 0..1
//!   need   = per produce item: (3 if wished + 2 * scarcity) / (1 + units other plots grow / 40)
//!   time   = 1 / (1 + hours to ripe / 6)
//!   reach  = 1 / (1 + distance to the seed vendor / 2500 m), 1 when the bot holds seeds
//!   held   = 1.25 when the bot holds seeds (no trip to a vendor)
//! ```
//!
//! so a wished item beats any profit, a crop that ripens in minutes beats one that takes days, and
//! the more of an item is already growing (the [`board`](super::board)), the less another bot
//! plants of it. The crops come from the world data ([`aa_world::Farming`]); everything else the
//! planner reads about the bot and the world is behind [`Env`], so the choice is tested without
//! a world.
//!
//! [evaluated]: evaluate

use aa_world::Crop;

use super::site::Site;

/// Units already on the way at which the need for an item has halved.
pub const DAMP_UNITS: f32 = 40.0;
/// The need of a wished item.
pub const WISH_NEED: f32 = 3.0;
/// The need of an item per unit of scarcity (0 to 1).
pub const SCARCE_NEED: f32 = 2.0;
/// Copper per hour at which the profit term is 1.
pub const PROFIT_SCALE: f32 = 2000.0;
/// The most seeds a run plants.
pub const RUN_MAX: u8 = 5;

/// Why a crop was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// The item is on the wish list.
    Wished(u32),
    /// The item is scarce.
    Scarce(u32),
    /// It pays best.
    Profit,
}

/// A seed vendor the bot can walk to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vendor {
    /// The NPC template.
    pub npc: u32,
    /// Where one of its spawns stands.
    pub pos: [f32; 3],
    /// How far it is from the bot.
    pub dist: f32,
}

/// What the planner reads about the bot and the world.
pub trait Env {
    /// Yield when the plot is raised for recurring livestock products.
    fn livestock_yields(&self, _crop: &Crop) -> Option<&[aa_world::Yield]> {
        None
    }
    /// Seeds `seed` in the bag.
    fn have(&self, seed: u32) -> i64;
    /// What a vendor asks for one `item`.
    fn price(&self, item: u32) -> u32;
    /// What one `item` is worth in copper.
    fn value(&self, item: u32) -> f32;
    /// How scarce `item` is, 0 to 1.
    fn scarcity(&self, item: u32) -> f32;
    /// Whether `item` is on the wish list.
    fn wished(&self, item: u32) -> bool;
    /// Units of `item` the other bots' plots will give.
    fn growing(&self, item: u32) -> f32;
    /// The nearest vendor within reach that sells `seed`.
    fn vendor(&self, seed: u32) -> Option<Vendor>;
    /// Whether the bot has a place within reach that takes the crop.
    fn place_for(&self, crop: &Crop) -> bool;
    /// Whether the server refused the seed lately.
    fn bad_seed(&self, seed: u32) -> bool;
    /// The bot's money in copper.
    fn money(&self) -> i64;
    /// Whether the bot may mint seeds with a GM command.
    fn gm(&self) -> bool;
}

/// A crop worth growing, and how it scored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pick {
    /// The seed.
    pub seed: u32,
    /// The doodad that grows.
    pub doodad: u32,
    /// Its planting radius (thousandths of a metre).
    pub radius_mm: u16,
    /// The farm kinds that take it (a [`FarmKind::bit`](aa_world::FarmKind::bit) mask).
    pub kinds: u8,
    /// Milliseconds to ripe at the configured growth rate.
    pub grow_ms: u64,
    /// The score.
    pub score: f32,
    /// What drove the choice.
    pub why: Why,
    /// The part of the score that is need (wished or scarce), 0 for a pure profit pick.
    pub need: f32,
    /// Seeds in the bag.
    pub have: u32,
    /// The seed's price.
    pub price: u32,
    /// The vendor, when the bot has to buy.
    pub vendor: Option<Vendor>,
}

impl Pick {
    /// Whether the seeds must be bought (the bag holds none).
    pub fn must_buy(&self) -> bool {
        self.have == 0
    }

    /// Whether the bot can pay for one seed.
    pub fn affordable(&self, money: i64) -> bool {
        money >= i64::from(self.price.max(1))
    }
}

/// How well one crop suits the bot now; `None` when it cannot be grown at all (not plantable,
/// refused lately, no place, no seeds and no way to get any, nothing it gives is worth anything).
pub fn evaluate(crop: &Crop, env: &dyn Env, rate: f32) -> Option<Pick> {
    let livestock = env.livestock_yields(crop);
    if (!crop.plantable_on_land() && livestock.is_none())
        || env.bad_seed(crop.seed)
        || !env.place_for(crop)
    {
        return None;
    }
    let have = u32::try_from(env.have(crop.seed).max(0)).unwrap_or(u32::MAX);
    let price = env.price(crop.seed);
    let vendor = if have == 0 {
        env.vendor(crop.seed)
    } else {
        None
    };
    if have == 0 {
        let can_buy = vendor.is_some() && env.money() >= i64::from(price.max(1));
        if !can_buy && !env.gm() {
            return None;
        }
    }
    let mut value = 0.0f32;
    let mut need = 0.0f32;
    let mut best = (0.0f32, Why::Profit);
    let mut products = 0;
    for y in livestock
        .unwrap_or(&crop.produce)
        .iter()
        .filter(|y| y.item != crop.seed)
    {
        products += 1;
        value += env.value(y.item) * y.average();
        let damp = 1.0 / (1.0 + env.growing(y.item) / DAMP_UNITS);
        let wish = if env.wished(y.item) { WISH_NEED } else { 0.0 };
        let n = (wish + SCARCE_NEED * env.scarcity(y.item).clamp(0.0, 1.0)) * damp;
        need += n;
        if n > best.0 {
            best = (
                n,
                if wish > 0.0 {
                    Why::Wished(y.item)
                } else {
                    Why::Scarce(y.item)
                },
            );
        }
    }
    if products == 0 {
        return None;
    }
    let grow_ms = crop.grow_ms_at(rate);
    let hours = grow_ms as f32 / 3_600_000.0;
    let cost = if have > 0 { 0.0 } else { price as f32 };
    let profit_rate = (value - cost * 0.5) / (hours + 0.25);
    if need <= 0.0 && profit_rate <= 0.0 {
        return None;
    }
    let profit = (profit_rate / PROFIT_SCALE).clamp(0.0, 1.0);
    let time = 1.0 / (1.0 + hours / 6.0);
    let reach = match &vendor {
        Some(v) => 1.0 / (1.0 + v.dist / 2_500.0),
        None => 1.0,
    };
    let held = if have > 0 { 1.25 } else { 1.0 };
    Some(Pick {
        seed: crop.seed,
        doodad: crop.doodad,
        radius_mm: crop.radius_mm,
        kinds: crop.kinds,
        grow_ms,
        score: (0.05 + profit + need) * time * reach * held,
        why: if need > 0.0 { best.1 } else { Why::Profit },
        need,
        have,
        price,
        vendor,
    })
}

/// The best crop for the bot, with a random spread of 10 percent so that bots with the same
/// wishes do not all take the same one.
pub fn choose(crops: &[Crop], env: &dyn Env, rate: f32, rng: &mut fastrand::Rng) -> Option<Pick> {
    let mut best: Option<Pick> = None;
    for crop in crops {
        let Some(mut pick) = evaluate(crop, env, rate) else {
            continue;
        };
        pick.score *= 0.9 + 0.2 * rng.f32();
        if best.is_none_or(|b| pick.score > b.score) {
            best = Some(pick);
        }
    }
    best
}

/// What one run does, decided from a [`Pick`] and a [`Site`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// The seed.
    pub seed: u32,
    /// The crop's doodad.
    pub doodad: u32,
    /// Its planting radius (thousandths of a metre).
    pub radius_mm: u16,
    /// Milliseconds to ripe at the configured growth rate.
    pub grow_ms: u64,
    /// Where to plant.
    pub site: Site,
    /// How many seeds to plant.
    pub want: u8,
    /// How many seeds to buy first.
    pub buy: u8,
    /// The vendor to buy from.
    pub vendor: Option<Vendor>,
    /// The seed's price.
    pub price: u32,
    /// How many seeds to mint with a GM command because there is no way to buy them (they are
    /// not the bot's own: what grows from them is not sold).
    pub mint: u8,
    /// Why the crop was chosen.
    pub why: Why,
}

/// Turns a pick into a plan for `site` with room for `room` more plots: plants as many as the
/// place, the bag, the money and [`RUN_MAX`] allow. `None` when not even one seed can be had.
pub fn make_plan(pick: &Pick, site: Site, room: u32, money: i64, gm: bool) -> Option<Plan> {
    let cap = u8::try_from(room.min(u32::from(RUN_MAX))).unwrap_or(RUN_MAX);
    if cap == 0 {
        return None;
    }
    let (mut want, mut buy, mut mint) = (0u8, 0u8, 0u8);
    if pick.have > 0 {
        // the seeds in the bag first, the rest bought if a vendor is near and the money is there
        want = u8::try_from(pick.have.min(u32::from(cap))).unwrap_or(cap);
    } else if let Some(_v) = pick.vendor.filter(|_| pick.affordable(money)) {
        let afford = money / i64::from(pick.price.max(1));
        want = u8::try_from(afford.clamp(1, i64::from(cap))).unwrap_or(1);
        buy = want;
    } else if gm {
        want = cap.min(3);
        mint = want;
    }
    if want == 0 {
        return None;
    }
    Some(Plan {
        seed: pick.seed,
        doodad: pick.doodad,
        radius_mm: pick.radius_mm,
        grow_ms: pick.grow_ms,
        site,
        want,
        buy,
        vendor: pick.vendor,
        price: pick.price,
        mint,
        why: pick.why,
    })
}

/// Caps `plan` to the seeds the bot's `labor` pays for when every seed costs `cost` points (a
/// wild patch). A GM bot keeps its plan (the farm machine tops its labor up); `None` when not
/// one seed is paid for.
pub fn labor_cap(mut plan: Plan, labor: i16, cost: i16, gm: bool) -> Option<Plan> {
    if cost <= 0 || gm {
        return Some(plan);
    }
    let n = u8::try_from((labor.max(0) / cost).min(i16::from(RUN_MAX))).unwrap_or(0);
    if n == 0 {
        return None;
    }
    plan.want = plan.want.min(n);
    plan.buy = plan.buy.min(plan.want);
    plan.mint = plan.mint.min(plan.want);
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_world::{FarmKind, Yield};
    use std::collections::HashMap;

    fn crop(seed: u32, doodad: u32, grow_s: u32, produce: &[(u32, u16, u16)]) -> Crop {
        Crop {
            seed,
            doodad,
            group: 12,
            grow_ms: grow_s * 1000,
            radius_mm: 500,
            restrict_zone: 0,
            ripe: vec![(9, 13980)].into(),
            produce: produce
                .iter()
                .map(|&(item, min, max)| Yield { item, min, max })
                .collect(),
            kinds: FarmKind::Farm.bit(),
        }
    }

    #[derive(Default)]
    struct Fake {
        have: HashMap<u32, i64>,
        price: HashMap<u32, u32>,
        value: HashMap<u32, f32>,
        scarce: HashMap<u32, f32>,
        wished: Vec<u32>,
        growing: HashMap<u32, f32>,
        vendors: HashMap<u32, f32>,
        no_place: Vec<u32>,
        bad: Vec<u32>,
        money: i64,
        gm: bool,
    }

    impl Env for Fake {
        fn have(&self, seed: u32) -> i64 {
            self.have.get(&seed).copied().unwrap_or(0)
        }
        fn price(&self, item: u32) -> u32 {
            self.price.get(&item).copied().unwrap_or(10)
        }
        fn value(&self, item: u32) -> f32 {
            self.value.get(&item).copied().unwrap_or(0.0)
        }
        fn scarcity(&self, item: u32) -> f32 {
            self.scarce.get(&item).copied().unwrap_or(0.0)
        }
        fn wished(&self, item: u32) -> bool {
            self.wished.contains(&item)
        }
        fn growing(&self, item: u32) -> f32 {
            self.growing.get(&item).copied().unwrap_or(0.0)
        }
        fn vendor(&self, seed: u32) -> Option<Vendor> {
            self.vendors.get(&seed).map(|&dist| Vendor {
                npc: 1,
                pos: [dist, 0.0, 0.0],
                dist,
            })
        }
        fn place_for(&self, c: &Crop) -> bool {
            !self.no_place.contains(&c.seed)
        }
        fn bad_seed(&self, seed: u32) -> bool {
            self.bad.contains(&seed)
        }
        fn money(&self) -> i64 {
            self.money
        }
        fn gm(&self) -> bool {
            self.gm
        }
    }

    // potato: seed 1 -> item 100 (and the seed back), 10 minutes; rice: seed 2 -> item 200, 2.9 h
    fn crops() -> Vec<Crop> {
        vec![
            crop(1, 11, 600, &[(100, 2, 4), (1, 1, 2)]),
            crop(2, 12, 10_320, &[(200, 3, 5), (2, 1, 1)]),
        ]
    }

    fn shop() -> Fake {
        let mut f = Fake {
            money: 10_000,
            ..Fake::default()
        };
        f.vendors.insert(1, 500.0);
        f.vendors.insert(2, 500.0);
        f.value.insert(100, 80.0);
        f.value.insert(200, 120.0);
        f
    }

    #[test]
    fn without_a_need_the_quick_crop_wins() {
        let f = shop();
        let mut rng = fastrand::Rng::with_seed(1);
        let pick = choose(&crops(), &f, 1.0, &mut rng).unwrap();
        assert_eq!(pick.seed, 1);
        assert_eq!(pick.why, Why::Profit);
        assert_eq!(pick.need, 0.0);
        assert_eq!(pick.grow_ms, 600_000);
    }

    #[test]
    fn a_wish_beats_the_profit() {
        let mut f = shop();
        f.wished.push(200);
        let mut rng = fastrand::Rng::with_seed(1);
        for _ in 0..50 {
            let pick = choose(&crops(), &f, 1.0, &mut rng).unwrap();
            assert_eq!(pick.seed, 2, "rice is wished");
            assert_eq!(pick.why, Why::Wished(200));
            assert!(pick.need >= WISH_NEED - 1e-4);
        }
    }

    #[test]
    fn a_scarce_item_pulls_less_than_a_wish_and_more_than_nothing() {
        let mut f = shop();
        f.scarce.insert(200, 0.9);
        let e = evaluate(&crops()[1], &f, 1.0).unwrap();
        assert_eq!(e.why, Why::Scarce(200));
        assert!((e.need - 1.8).abs() < 1e-4);
        let calm = evaluate(&crops()[1], &shop(), 1.0).unwrap();
        assert!(e.score > calm.score);
        f.wished.push(200);
        let w = evaluate(&crops()[1], &f, 1.0).unwrap();
        assert!(w.score > e.score);
    }

    #[test]
    fn what_is_already_growing_damps_the_need() {
        let mut f = shop();
        f.wished.push(100);
        let fresh = evaluate(&crops()[0], &f, 1.0).unwrap();
        f.growing.insert(100, 40.0);
        let crowded = evaluate(&crops()[0], &f, 1.0).unwrap();
        assert!((crowded.need - fresh.need / 2.0).abs() < 1e-4);
        f.growing.insert(100, 4_000.0);
        let flooded = evaluate(&crops()[0], &f, 1.0).unwrap();
        assert!(flooded.need < 0.05 * fresh.need);
    }

    #[test]
    fn the_growth_rate_shortens_the_wait_and_raises_the_score() {
        let f = shop();
        let slow = evaluate(&crops()[1], &f, 1.0).unwrap();
        let fast = evaluate(&crops()[1], &f, 10.0).unwrap();
        assert_eq!(fast.grow_ms, 1_032_000);
        assert!(fast.score > slow.score);
    }

    #[test]
    fn a_crop_needs_seeds_a_way_to_get_them_and_a_place() {
        let mut f = shop();
        // no vendor: no seeds
        f.vendors.remove(&1);
        assert!(evaluate(&crops()[0], &f, 1.0).is_none());
        // ... unless the bag holds some, or a GM may mint them
        f.have.insert(1, 3);
        assert!(evaluate(&crops()[0], &f, 1.0).is_some());
        f.have.clear();
        f.gm = true;
        assert!(evaluate(&crops()[0], &f, 1.0).is_some());
        f.gm = false;
        // a vendor but no money
        f.vendors.insert(1, 100.0);
        f.money = 5;
        assert!(evaluate(&crops()[0], &f, 1.0).is_none());
        f.money = 10;
        assert!(evaluate(&crops()[0], &f, 1.0).is_some());
        // no place, a refused seed
        f.no_place.push(1);
        assert!(evaluate(&crops()[0], &f, 1.0).is_none());
        f.no_place.clear();
        f.bad.push(1);
        assert!(evaluate(&crops()[0], &f, 1.0).is_none());
    }

    #[test]
    fn a_crop_that_gives_nothing_worth_having_is_left_alone() {
        let mut f = shop();
        f.value.clear();
        // no value, no need
        assert!(evaluate(&crops()[0], &f, 1.0).is_none());
        // a wish makes it worth it whatever it fetches
        f.wished.push(100);
        assert!(evaluate(&crops()[0], &f, 1.0).is_some());
        // a crop that only gives its own seed back is no crop
        let seedling = crop(5, 15, 60, &[(5, 1, 2)]);
        assert!(evaluate(&seedling, &f, 1.0).is_none());
        // livestock and trees that are felled are not plantable
        let mut cow = crop(6, 16, 60, &[(100, 1, 1)]);
        cow.group = 5;
        assert!(evaluate(&cow, &f, 1.0).is_none());
    }

    #[test]
    fn a_held_seed_and_a_near_vendor_are_worth_more() {
        let mut f = shop();
        let far = {
            f.vendors.insert(1, 3_000.0);
            evaluate(&crops()[0], &f, 1.0).unwrap().score
        };
        let near = {
            f.vendors.insert(1, 100.0);
            evaluate(&crops()[0], &f, 1.0).unwrap().score
        };
        assert!(near > far);
        f.have.insert(1, 2);
        let held = evaluate(&crops()[0], &f, 1.0).unwrap();
        assert!(held.score > near);
        assert!(held.vendor.is_none() && !held.must_buy());
    }

    #[test]
    fn the_bots_spread_over_equal_crops() {
        let twins = [
            crop(1, 11, 600, &[(100, 2, 4), (1, 1, 2)]),
            crop(3, 13, 600, &[(100, 2, 4), (3, 1, 2)]),
        ];
        let mut f = shop();
        f.vendors.insert(3, 500.0);
        let mut seen = [0, 0];
        let mut rng = fastrand::Rng::with_seed(5);
        for _ in 0..200 {
            let p = choose(&twins, &f, 1.0, &mut rng).unwrap();
            seen[usize::from(p.seed == 3)] += 1;
        }
        assert!(seen[0] > 40 && seen[1] > 40, "{seen:?}");
    }

    fn site() -> Site {
        Site {
            land: 0,
            index: 1,
            centre: [0.0, 0.0],
            radius: 5.0,
        }
    }

    #[test]
    fn a_plan_is_as_big_as_the_room_the_seeds_and_the_money_allow() {
        let f = shop();
        let pick = evaluate(&crops()[0], &f, 1.0).unwrap();
        // buy: money for 1000 seeds, room for 3
        let p = make_plan(&pick, site(), 3, 10_000, false).unwrap();
        assert_eq!((p.want, p.buy, p.mint), (3, 3, 0));
        // money for 2 seeds only
        let p = make_plan(&pick, site(), 5, 25, false).unwrap();
        assert_eq!((p.want, p.buy), (2, 2));
        // never more than a run
        let p = make_plan(&pick, site(), 50, 1_000_000, false).unwrap();
        assert_eq!(p.want, RUN_MAX);
        // no room, no plan
        assert!(make_plan(&pick, site(), 0, 10_000, false).is_none());
        // no money and no GM: no plan; a GM mints three
        assert!(make_plan(&pick, site(), 5, 0, false).is_none());
        let p = make_plan(&pick, site(), 5, 0, true).unwrap();
        assert_eq!((p.want, p.buy, p.mint), (3, 0, 3));
        // seeds in the bag are planted first and bought for nothing
        let mut f = shop();
        f.have.insert(1, 2);
        let held = evaluate(&crops()[0], &f, 1.0).unwrap();
        let p = make_plan(&held, site(), 5, 0, false).unwrap();
        assert_eq!((p.want, p.buy, p.mint), (2, 0, 0));
    }

    #[test]
    fn a_wild_plan_is_as_big_as_the_labor_pays_for() {
        let f = shop();
        let pick = evaluate(&crops()[0], &f, 1.0).unwrap();
        let p = make_plan(&pick, site(), 5, 10_000, false).unwrap();
        assert_eq!(p.want, 5);
        let capped = labor_cap(p, 25, 10, false).unwrap();
        assert_eq!(
            (capped.want, capped.buy),
            (2, 2),
            "25 labor pays for two seeds"
        );
        assert!(labor_cap(p, 9, 10, false).is_none());
        assert_eq!(labor_cap(p, 0, 10, true).unwrap().want, 5, "a GM tops up");
        assert_eq!(labor_cap(p, 0, 0, false).unwrap().want, 5, "a free seed");
    }
}
