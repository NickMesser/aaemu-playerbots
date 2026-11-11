//! Route planning for trade runs: which pack to craft where, and which specialty NPC buys it.
//!
//! A *route* is a specialty workbench spawn (a doodad whose template offers pack crafts), one of
//! the pack crafts it offers, and the spawn of the NPC that buys the product best. Routes are
//! scored by the way the bot has to go (to the bench, then carrying the pack to the buyer) minus
//! what the sale pays ([`RouteData::pack_price`]), and the planner picks among the best few so that
//! many bots do not all queue at the same bench.
//!
//! The planner works on [`RouteData`], a narrow view of the static world, so the tests run on a
//! synthetic world; [`aa_world::World`] implements it.

use aa_world::{Craft, Vec3, World};

/// Routes kept after sorting.
const MAX_CANDIDATES: usize = 64;
/// The planner draws among this many of the best routes.
const PICK_AMONG: usize = 4;
/// Carrying the pack counts this much per metre (the way back is free, the way there is not).
const CARRY_WEIGHT: f32 = 1.25;
/// Copper of sale price that make up for one metre of the way: a 40 000 copper pack (4 gold)
/// outweighs 1 600 m, the best packs (24 gold) 9 600 m.
const COPPER_PER_METRE: f32 = 25.0;

/// The static facts the planner needs. Implemented by [`World`]; tests use a synthetic world.
pub trait RouteData {
    /// Calls `f(doodad template, position)` for every spawn of a doodad that offers pack crafts
    /// within `radius` metres (horizontal) of `center`.
    fn benches_within(&self, center: Vec3, radius: f32, f: &mut dyn FnMut(u32, Vec3));
    /// The pack crafts a bench template offers.
    fn pack_crafts_at(&self, bench: u32) -> &[u32];
    /// A craft.
    fn craft(&self, id: u32) -> Option<&Craft>;
    /// NPC templates that buy `item`.
    fn buyers(&self, item: u32) -> &[u32];
    /// Spawn positions of an NPC template.
    fn npc_spawns(&self, npc: u32) -> &[Vec3];
    /// What `buyer` pays for the pack `item` in copper (an estimate, 0 when unknown).
    fn pack_price(&self, item: u32, buyer: u32) -> u32;
}

impl RouteData for World {
    fn benches_within(&self, center: Vec3, radius: f32, f: &mut dyn FnMut(u32, Vec3)) {
        self.doodad_spawns_within(center, radius, |template, pos| {
            if !self.pack_crafts_at(template).is_empty() {
                f(template, pos);
            }
        });
    }

    fn pack_crafts_at(&self, bench: u32) -> &[u32] {
        World::pack_crafts_at(self, bench)
    }

    fn craft(&self, id: u32) -> Option<&Craft> {
        World::craft(self, id)
    }

    fn buyers(&self, item: u32) -> &[u32] {
        self.specialty_buyers(item)
    }

    fn npc_spawns(&self, npc: u32) -> &[Vec3] {
        World::npc_spawns(self, npc)
    }

    fn pack_price(&self, item: u32, buyer: u32) -> u32 {
        // 0: the origin zone group is not known here and the server's price does not depend on it
        self.pack_price_estimate(item, buyer, 0).unwrap_or(0)
    }
}

/// One planned trade run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// The craft to execute; 0 when the bot already carries the pack.
    pub craft: u32,
    /// The pack item (the craft's first product).
    pub item: u32,
    /// The bench doodad template; 0 when the bot already carries the pack.
    pub bench_tpl: u32,
    /// The buying NPC template.
    pub buyer_tpl: u32,
    /// Bench spawn (x, y).
    pub bench: [f32; 2],
    /// Buyer spawn (x, y).
    pub buyer: [f32; 2],
    /// What the buyer pays for the pack, in copper (estimate; 0 when unknown).
    pub price: u32,
    /// Straight-line distance from the bot to the bench.
    pub to_bench_m: f32,
    /// Straight-line distance from the bench to the buyer.
    pub carry_m: f32,
}

impl Plan {
    /// What the route costs, lower is better: the way to the bench plus the weighted way with the
    /// pack, less the sale price expressed in metres.
    pub fn score(&self) -> f32 {
        self.to_bench_m + buyer_cost(self.carry_m, self.price)
    }

    /// Total straight-line distance of the run.
    pub fn total_m(&self) -> f32 {
        self.to_bench_m + self.carry_m
    }
}

/// What a planning request wants.
pub struct Query<'a> {
    /// Where the bot is (x, y).
    pub from: [f32; 2],
    /// Longest total route (to the bench plus the carry) in metres; `f32::INFINITY` when the
    /// distances do not matter (GM `/move`).
    pub max_route_m: f32,
    /// Benches further than this from `from` are not looked at.
    pub radius_m: f32,
    /// Whether a craft may be chosen (materials and labor at hand, not quarantined).
    pub craft_ok: &'a dyn Fn(&Craft) -> bool,
    /// Whether a bench spawn (template, x, y) may be used.
    pub bench_ok: &'a dyn Fn(u32, [f32; 2]) -> bool,
    /// Whether a buyer NPC template may be used.
    pub buyer_ok: &'a dyn Fn(u32) -> bool,
}

fn d2(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// How often a route is sampled for a hostile zone (metres).
const ZONE_STEP_M: f32 = 300.0;

/// Whether the straight line from `from` to `to` passes through a zone `hostile_at` names. The
/// sample is coarse (every [`ZONE_STEP_M`]), like the danger check of the quest planner: a route
/// that only clips a hostile zone is still a route through its guards.
pub fn crosses_hostile_zone(
    from: [f32; 2],
    to: [f32; 2],
    hostile_at: &mut dyn FnMut(f32, f32) -> bool,
) -> bool {
    let d = d2(from, to);
    let n = ((d / ZONE_STEP_M).ceil() as usize).max(1);
    (0..=n).any(|i| {
        let k = i as f32 / n as f32;
        let x = from[0] + (to[0] - from[0]) * k;
        let y = from[1] + (to[1] - from[1]) * k;
        hostile_at(x, y)
    })
}

/// What carrying a pack `dist` metres to a buyer that pays `price` costs, in metres.
fn buyer_cost(dist: f32, price: u32) -> f32 {
    CARRY_WEIGHT * dist - price as f32 / COPPER_PER_METRE
}

/// The best spawn of any acceptable buyer of `item` seen from `from` (the nearest one when all
/// pay the same; a farther buyer wins when it pays enough more):
/// `(npc template, (x, y), price)`.
pub fn best_buyer<D: RouteData + ?Sized>(
    data: &D,
    item: u32,
    from: [f32; 2],
    buyer_ok: &dyn Fn(u32) -> bool,
) -> Option<(u32, [f32; 2], u32)> {
    let mut best: Option<(u32, [f32; 2], u32, f32)> = None;
    for &npc in data.buyers(item) {
        if !buyer_ok(npc) {
            continue;
        }
        let price = data.pack_price(item, npc);
        for sp in data.npc_spawns(npc) {
            let p = [sp.x, sp.y];
            let cost = buyer_cost(d2(from, p), price);
            if best.is_none_or(|b| cost < b.3) {
                best = Some((npc, p, price, cost));
            }
        }
    }
    best.map(|b| (b.0, b.1, b.2))
}

/// Every acceptable route for `q`, best first (at most [`MAX_CANDIDATES`]).
pub fn candidates<D: RouteData + ?Sized>(data: &D, q: &Query<'_>) -> Vec<Plan> {
    let mut out: Vec<Plan> = Vec::new();
    let center = Vec3::new(q.from[0], q.from[1], 0.0);
    data.benches_within(center, q.radius_m, &mut |tpl, pos| {
        let bench = [pos.x, pos.y];
        if !(q.bench_ok)(tpl, bench) {
            return;
        }
        let to_bench_m = d2(q.from, bench);
        for &id in data.pack_crafts_at(tpl) {
            let Some(c) = data.craft(id) else { continue };
            if !c.is_pack || c.product.0 == 0 || !(q.craft_ok)(c) {
                continue;
            }
            let Some((buyer_tpl, buyer, price)) = best_buyer(data, c.product.0, bench, q.buyer_ok)
            else {
                continue;
            };
            let plan = Plan {
                craft: id,
                item: c.product.0,
                bench_tpl: tpl,
                buyer_tpl,
                bench,
                buyer,
                price,
                to_bench_m,
                carry_m: d2(bench, buyer),
            };
            if plan.total_m() <= q.max_route_m {
                out.push(plan);
            }
        }
    });
    out.sort_by(|a, b| {
        a.score()
            .total_cmp(&b.score())
            .then(a.craft.cmp(&b.craft))
            .then(a.bench_tpl.cmp(&b.bench_tpl))
    });
    out.truncate(MAX_CANDIDATES);
    out
}

/// Draws one of the best routes (`cands` sorted best first), preferring different crafts:
/// among the first [`PICK_AMONG`] distinct crafts, uniformly.
pub fn pick(cands: &[Plan], rng: &mut fastrand::Rng) -> Option<Plan> {
    let mut crafts: smallvec::SmallVec<[Plan; PICK_AMONG]> = smallvec::SmallVec::new();
    for p in cands {
        if crafts.iter().all(|c| c.craft != p.craft) {
            crafts.push(*p);
            if crafts.len() == PICK_AMONG {
                break;
            }
        }
    }
    if crafts.is_empty() {
        None
    } else {
        Some(crafts[rng.usize(..crafts.len())])
    }
}

/// A plan for a bot that already carries pack `item`: only the walk to the best buyer.
pub fn carry_only<D: RouteData + ?Sized>(
    data: &D,
    item: u32,
    from: [f32; 2],
    buyer_ok: &dyn Fn(u32) -> bool,
) -> Option<Plan> {
    let (buyer_tpl, buyer, price) = best_buyer(data, item, from, buyer_ok)?;
    Some(Plan {
        craft: 0,
        item,
        bench_tpl: 0,
        buyer_tpl,
        bench: from,
        buyer,
        price,
        to_bench_m: 0.0,
        carry_m: d2(from, buyer),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustc_hash::FxHashMap;

    /// A tiny synthetic world.
    #[derive(Default)]
    pub struct Fake {
        pub benches: Vec<(u32, Vec3)>,
        pub bench_crafts: FxHashMap<u32, Vec<u32>>,
        pub crafts: FxHashMap<u32, Craft>,
        pub buyers: FxHashMap<u32, Vec<u32>>,
        pub spawns: FxHashMap<u32, Vec<Vec3>>,
        /// `(item, buyer)` -> price in copper.
        pub prices: FxHashMap<(u32, u32), u32>,
    }

    pub fn craft(id: u32, item: u32, labor: u16) -> Craft {
        Craft {
            id,
            name: format!("craft {id}").into(),
            skill_id: 16766,
            labor,
            product: (item, 1),
            materials: vec![(4747, 1), (30901, 50)].into_boxed_slice(),
            is_pack: true,
            req_doodad: 0,
            benches: Box::new([]),
            req_level: 10,
            actability_group: 31,
            actability_limit: 0,
        }
    }

    impl Fake {
        /// Benches 100 (at 1000,0) and 200 (at 0,3000); crafts 1 (item 11) and 2 (item 12) at
        /// bench 100, craft 3 (item 13) at bench 200; buyers 500 (item 11 and 12) at (1500,0)
        /// and (0, 9000), 501 (item 13) at (0, 3200).
        pub fn sample() -> Fake {
            let mut f = Fake::default();
            f.benches.push((100, Vec3::new(1000.0, 0.0, 0.0)));
            f.benches.push((200, Vec3::new(0.0, 3000.0, 0.0)));
            f.bench_crafts.insert(100, vec![1, 2]);
            f.bench_crafts.insert(200, vec![3]);
            f.crafts.insert(1, craft(1, 11, 60));
            f.crafts.insert(2, craft(2, 12, 60));
            f.crafts.insert(3, craft(3, 13, 60));
            f.buyers.insert(11, vec![500]);
            f.buyers.insert(12, vec![500]);
            f.buyers.insert(13, vec![501]);
            f.spawns.insert(
                500,
                vec![Vec3::new(0.0, 9000.0, 0.0), Vec3::new(1500.0, 0.0, 0.0)],
            );
            f.spawns.insert(501, vec![Vec3::new(0.0, 3200.0, 0.0)]);
            f
        }
    }

    impl RouteData for Fake {
        fn benches_within(&self, center: Vec3, radius: f32, f: &mut dyn FnMut(u32, Vec3)) {
            for (t, p) in &self.benches {
                if center.dist2d(*p) <= radius {
                    f(*t, *p);
                }
            }
        }
        fn pack_crafts_at(&self, bench: u32) -> &[u32] {
            self.bench_crafts.get(&bench).map_or(&[], |v| &v[..])
        }
        fn craft(&self, id: u32) -> Option<&Craft> {
            self.crafts.get(&id)
        }
        fn buyers(&self, item: u32) -> &[u32] {
            self.buyers.get(&item).map_or(&[], |v| &v[..])
        }
        fn npc_spawns(&self, npc: u32) -> &[Vec3] {
            self.spawns.get(&npc).map_or(&[], |v| &v[..])
        }
        fn pack_price(&self, item: u32, buyer: u32) -> u32 {
            self.prices.get(&(item, buyer)).copied().unwrap_or(0)
        }
    }

    fn open_query<'a>(
        from: [f32; 2],
        max_route_m: f32,
        radius_m: f32,
        craft_ok: &'a dyn Fn(&Craft) -> bool,
        bench_ok: &'a dyn Fn(u32, [f32; 2]) -> bool,
        buyer_ok: &'a dyn Fn(u32) -> bool,
    ) -> Query<'a> {
        Query {
            from,
            max_route_m,
            radius_m,
            craft_ok,
            bench_ok,
            buyer_ok,
        }
    }

    #[test]
    fn the_nearest_buyer_spawn_wins_and_routes_are_sorted() {
        let w = Fake::sample();
        let q = open_query(
            [0.0, 0.0],
            f32::INFINITY,
            50_000.0,
            &|_| true,
            &|_, _| true,
            &|_| true,
        );
        let c = candidates(&w, &q);
        assert_eq!(c.len(), 3);
        // craft 1 and 2: bench 1000 m away, buyer 500 m beyond it (the nearer of two spawns)
        assert_eq!((c[0].craft, c[0].bench_tpl, c[0].buyer_tpl), (1, 100, 500));
        assert_eq!(c[0].buyer, [1500.0, 0.0]);
        assert!((c[0].to_bench_m - 1000.0).abs() < 0.1);
        assert!((c[0].carry_m - 500.0).abs() < 0.1);
        assert_eq!(c[1].craft, 2);
        assert_eq!(c[2].craft, 3);
        assert!(c[0].score() <= c[1].score() && c[1].score() <= c[2].score());
        assert!((c[2].total_m() - 3200.0).abs() < 0.1);
    }

    #[test]
    fn the_walking_budget_and_the_radius_limit_the_routes() {
        let w = Fake::sample();
        let all = |_: &Craft| true;
        let any = |_: u32, _: [f32; 2]| true;
        let buy = |_: u32| true;
        let q = open_query([0.0, 0.0], 2000.0, 50_000.0, &all, &any, &buy);
        let c = candidates(&w, &q);
        assert_eq!(c.iter().map(|p| p.craft).collect::<Vec<_>>(), vec![1, 2]);
        // bench 200 is outside a 2 km radius
        let q = open_query([0.0, 0.0], f32::INFINITY, 2000.0, &all, &any, &buy);
        assert_eq!(candidates(&w, &q).len(), 2);
        // from the north bench the other one is out of the budget
        let q = open_query([0.0, 3000.0], 500.0, 50_000.0, &all, &any, &buy);
        let c = candidates(&w, &q);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].craft, c[0].bench_tpl, c[0].buyer_tpl), (3, 200, 501));
    }

    #[test]
    fn filters_remove_crafts_benches_and_buyers() {
        let w = Fake::sample();
        let no_two = |c: &Craft| c.id != 2;
        let any = |_: u32, _: [f32; 2]| true;
        let buy = |_: u32| true;
        let q = open_query([0.0, 0.0], f32::INFINITY, 50_000.0, &no_two, &any, &buy);
        assert_eq!(
            candidates(&w, &q)
                .iter()
                .map(|p| p.craft)
                .collect::<Vec<_>>(),
            vec![1, 3]
        );
        let all = |_: &Craft| true;
        let no_bench_100 = |t: u32, _: [f32; 2]| t != 100;
        let q = open_query(
            [0.0, 0.0],
            f32::INFINITY,
            50_000.0,
            &all,
            &no_bench_100,
            &buy,
        );
        assert_eq!(
            candidates(&w, &q)
                .iter()
                .map(|p| p.craft)
                .collect::<Vec<_>>(),
            vec![3]
        );
        let no_501 = |n: u32| n != 501;
        let q = open_query([0.0, 0.0], f32::INFINITY, 50_000.0, &all, &any, &no_501);
        assert_eq!(candidates(&w, &q).len(), 2);
        // a craft that is no pack is never planned
        let mut w2 = Fake::sample();
        w2.crafts.get_mut(&1).unwrap().is_pack = false;
        let q = open_query([0.0, 0.0], f32::INFINITY, 50_000.0, &all, &any, &buy);
        assert_eq!(candidates(&w2, &q).len(), 2);
    }

    #[test]
    fn a_craft_without_a_buyer_is_dropped() {
        let mut w = Fake::sample();
        w.buyers.remove(&13);
        let q = open_query(
            [0.0, 0.0],
            f32::INFINITY,
            50_000.0,
            &|_| true,
            &|_, _| true,
            &|_| true,
        );
        assert_eq!(candidates(&w, &q).len(), 2);
    }

    #[test]
    fn pick_prefers_distinct_crafts_among_the_best() {
        let w = Fake::sample();
        let q = open_query(
            [0.0, 0.0],
            f32::INFINITY,
            50_000.0,
            &|_| true,
            &|_, _| true,
            &|_| true,
        );
        let c = candidates(&w, &q);
        let mut rng = fastrand::Rng::with_seed(9);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            seen.insert(pick(&c, &mut rng).unwrap().craft);
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(pick(&[], &mut rng).is_none());
        // the same craft at two benches counts once
        let mut dup = c.clone();
        dup.insert(
            1,
            Plan {
                bench_tpl: 101,
                ..c[0]
            },
        );
        let mut counts = std::collections::BTreeMap::new();
        for _ in 0..3000 {
            *counts
                .entry(pick(&dup, &mut rng).unwrap().craft)
                .or_insert(0u32) += 1;
        }
        assert!(counts[&1] < 1300, "{counts:?}");
    }

    #[test]
    fn a_better_price_beats_a_shorter_way() {
        // two buyers of item 11: 500 is 1500 m from the bench, 502 is 3000 m farther
        let mut w = Fake::sample();
        w.buyers.insert(11, vec![500, 502]);
        w.spawns.insert(502, vec![Vec3::new(4500.0, 0.0, 0.0)]);
        let all = |_: &Craft| true;
        let any = |_: u32, _: [f32; 2]| true;
        let buy = |_: u32| true;
        let q = open_query([0.0, 0.0], f32::INFINITY, 2000.0, &all, &any, &buy);
        let near = candidates(&w, &q);
        assert_eq!(near[0].buyer_tpl, 500, "equal prices: the nearer buyer");
        // 502 pays 100 000 copper more: 4000 m of credit against 3000 m * 1.25 of extra carrying
        w.prices.insert((11, 502), 140_000);
        w.prices.insert((11, 500), 40_000);
        let rich = candidates(&w, &q);
        let best = rich.iter().find(|p| p.craft == 1).unwrap();
        assert_eq!((best.buyer_tpl, best.price), (502, 140_000));
        // the same price at both buyers: back to the nearest
        w.prices.insert((11, 500), 140_000);
        let same = candidates(&w, &q);
        assert_eq!(same.iter().find(|p| p.craft == 1).unwrap().buyer_tpl, 500);
        // a price makes a route more attractive than an unpriced one of the same length
        assert!(best.score() < near.iter().find(|p| p.craft == 1).unwrap().score());
        // a carried pack goes to the buyer that pays best too
        w.prices.insert((11, 500), 40_000);
        let p = carry_only(&w, 11, [1400.0, 0.0], &|_| true).unwrap();
        assert_eq!((p.buyer_tpl, p.price), (502, 140_000));
    }

    #[test]
    fn a_route_through_a_hostile_zone_is_found() {
        // a hostile strip between x 500 and 700: the straight line crosses it (the zone grid is
        // sampled along the line)
        let mut strip = |x: f32, _y: f32| (500.0..=700.0).contains(&x);
        assert!(crosses_hostile_zone([0.0, 0.0], [1_000.0, 0.0], &mut strip));
        let mut none = |_x: f32, _y: f32| false;
        assert!(!crosses_hostile_zone([0.0, 0.0], [1_000.0, 0.0], &mut none));
        // the endpoint counts too
        let mut at_end = |x: f32, _y: f32| x >= 990.0;
        assert!(crosses_hostile_zone(
            [0.0, 0.0],
            [1_000.0, 0.0],
            &mut at_end
        ));
        // a zero-length route still asks about the spot
        let mut here = |_x: f32, _y: f32| true;
        assert!(crosses_hostile_zone([5.0, 5.0], [5.0, 5.0], &mut here));
    }

    #[test]
    fn carry_only_goes_to_the_nearest_buyer() {
        let w = Fake::sample();
        let p = carry_only(&w, 11, [1400.0, 0.0], &|_| true).unwrap();
        assert_eq!((p.craft, p.bench_tpl, p.buyer_tpl), (0, 0, 500));
        assert_eq!(p.buyer, [1500.0, 0.0]);
        assert!((p.carry_m - 100.0).abs() < 0.1 && p.to_bench_m == 0.0);
        assert!(carry_only(&w, 11, [0.0, 0.0], &|n| n != 500).is_none());
        assert!(carry_only(&w, 99, [0.0, 0.0], &|_| true).is_none());
    }
}
