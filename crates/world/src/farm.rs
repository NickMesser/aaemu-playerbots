//! Farming: what a seed grows into, what the harvest gives, who sells the seeds and where the
//! public farms are.
//!
//! Table semantics follow the server's `systems/npcs/doodad/{data,phase,phase_funcs}.rs` and
//! `public_farm.rs` (AAEmu Rust, GPL-3.0-or-later):
//!
//! * a **seed** is an item with a row in `item_spawn_doodads`; `CSCreateDoodad` with it plants that
//!   doodad (the crop). The crop goes through *phases* (`doodad_func_groups`); a `DoodadFuncGrowth`
//!   (delay, next phase) moves it on, and the server's total growth time is the sum of all growth
//!   delays of the doodad (the doodad's own `growth_time` when there are none), divided by the
//!   world's `GrowthRate` and shortened to 73 % under a matching climate;
//! * a phase is **ripe** when it has a `DoodadFuncUse` with a harvest skill whose next phase hands
//!   out items (`DoodadFuncLootPack` / `DoodadFuncLootItem`); the skill (`CSStartSkill` on the
//!   doodad) is the harvest, its `consume_lp` is the labor it costs. Ripe crops that are not
//!   harvested wither into another ripe phase after about two days; both are in [`Crop::ripe`];
//! * the **public farms** are the sub zones 966, 967, 968, 974 and 998 (hard-coded in the server's
//!   `PublicFarmManager`); which crops each kind allows is `farm_group_doodads`.
//!
//! The farm area polygons come from the client's `subzone_area.xml` (baked by `aa-bake` into
//! `farm_areas.json`); a world without them has no public farm and bots plant on their own land.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::baked::HousingAreaPoly;
use crate::geom::IdMap;
use crate::housing::contains;
#[cfg(test)]
use crate::npc::NpcRoles;
use crate::npc::NpcTemplate;
use crate::sql::{each_row, table_exists, RowExt};

/// The sub zones the server treats as public farms (`PublicFarmManager.Load`, hard-coded there)
/// and the kind of farm each one is.
pub const FARM_SUB_ZONES: [(u32, FarmKind); 5] = [
    (966, FarmKind::Farm),
    (967, FarmKind::Ranch),
    (968, FarmKind::Nursery),
    (974, FarmKind::Stable),
    (998, FarmKind::Farm),
];

/// The most plots one character may keep on public farms of a kind (`farm_groups.count`:
/// fields 10, nurseries 5, ranches 5, stables 5). The bots' own `max_plots` is lower by default.
pub fn farm_limit(kind: FarmKind) -> u32 {
    match kind {
        FarmKind::Farm => 10,
        _ => 5,
    }
}

/// A kind of public farm (`FarmGroupKind` of the server).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FarmKind {
    /// Field crops (potato, rice, ...).
    Farm = 1,
    /// Trees (grapevine, fig, ...).
    Nursery = 2,
    /// Livestock (chick, duckling, ...).
    Ranch = 3,
    /// Mount young (not a bot business).
    Stable = 4,
}

impl FarmKind {
    /// The kind of `farm_groups.id` / `FarmGroupKind`.
    pub fn from_group(id: u32) -> Option<FarmKind> {
        match id {
            1 => Some(FarmKind::Farm),
            2 => Some(FarmKind::Nursery),
            3 => Some(FarmKind::Ranch),
            4 => Some(FarmKind::Stable),
            _ => None,
        }
    }

    /// The kind of a public farm sub zone, if it is one.
    pub fn of_sub_zone(sub_zone: u32) -> Option<FarmKind> {
        FARM_SUB_ZONES
            .iter()
            .find(|(id, _)| *id == sub_zone)
            .map(|(_, k)| *k)
    }

    /// The bit of the kind in [`Crop::kinds`].
    pub const fn bit(self) -> u8 {
        1 << (self as u8)
    }

    /// Name for logs and the web UI.
    pub const fn name(self) -> &'static str {
        match self {
            FarmKind::Farm => "farm",
            FarmKind::Nursery => "nursery",
            FarmKind::Ranch => "ranch",
            FarmKind::Stable => "stable",
        }
    }
}

/// One item a harvest can hand out (a loot pack entry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Yield {
    /// Item template.
    pub item: u32,
    /// Fewest items per harvest.
    pub min: u16,
    /// Most items per harvest.
    pub max: u16,
}

/// A non-destructive interaction in an authored livestock phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StockAction {
    Feed(u32),
    Treat(u32),
    Collect(u32),
}

impl StockAction {
    pub fn skill(self) -> u32 {
        match self {
            Self::Feed(skill) | Self::Treat(skill) | Self::Collect(skill) => skill,
        }
    }
}

#[derive(Debug, Default)]
struct Livestock {
    actions: FxHashMap<u32, StockAction>,
    yields: Vec<Yield>,
}

impl Yield {
    /// The average count per harvest.
    pub fn average(self) -> f32 {
        (f32::from(self.min) + f32::from(self.max)) / 2.0
    }
}

/// One thing a seed grows into.
#[derive(Clone, Debug, PartialEq)]
pub struct Crop {
    /// The seed item (`item_spawn_doodads.item_id`).
    pub seed: u32,
    /// The doodad that is planted.
    pub doodad: u32,
    /// Doodad group (`doodad_almighties.group_id`; 40 = marine crops, which need water).
    pub group: u8,
    /// Growth time in milliseconds at `GrowthRate` 1.0 and without the climate bonus.
    pub grow_ms: u32,
    /// Planting radius (`sim_radius`, thousandths of a metre): crops keep this much room.
    pub radius_mm: u16,
    /// The doodad may only be planted in this zone (`restrict_zone_id`, 0 = anywhere).
    pub restrict_zone: u32,
    /// The ripe phases and the harvest skill each one answers to, `(phase, skill)`.
    pub ripe: Box<[(u32, u32)]>,
    /// What a harvest gives (the seed itself usually among it).
    pub produce: Box<[Yield]>,
    /// The public farm kinds that allow this crop, a [`FarmKind::bit`] mask.
    pub kinds: u8,
}

impl Crop {
    /// Whether `phase` is one in which the crop can be harvested.
    pub fn is_ripe(&self, phase: u32) -> bool {
        self.ripe.iter().any(|(p, _)| *p == phase)
    }

    /// The skill that harvests the crop in `phase`.
    pub fn harvest_skill(&self, phase: u32) -> Option<u32> {
        self.ripe.iter().find(|(p, _)| *p == phase).map(|(_, s)| *s)
    }

    /// The skill that harvests the crop (that of its first ripe phase).
    pub fn skill(&self) -> u32 {
        self.ripe.first().map_or(0, |(_, s)| *s)
    }

    /// Whether a public farm of `kind` lets the character plant this crop.
    pub fn allowed_in(&self, kind: FarmKind) -> bool {
        self.kinds & kind.bit() != 0
    }

    /// Milliseconds from planting to the ripe phase when the server's growth rate is `rate`
    /// (without the climate bonus: a matching climate only makes it earlier).
    pub fn grow_ms_at(&self, rate: f32) -> u64 {
        let rate = if rate.is_finite() && rate > 0.0 {
            rate
        } else {
            1.0
        };
        (f64::from(self.grow_ms) / f64::from(rate)).round() as u64
    }

    /// The harvest yield of `item`, if the crop gives it.
    pub fn yield_of(&self, item: u32) -> Option<Yield> {
        self.produce.iter().copied().find(|y| y.item == item)
    }

    /// Whether the doodad needs water (marine crops).
    pub fn needs_water(&self) -> bool {
        self.group == 40
    }

    /// Whether a bot can plant it on dry land at all: a field crop (group 12), a herb (3) or a
    /// nursery tree (2 with a nursery in [`Crop::kinds`]; the other trees are felled), with a
    /// harvest and a growth time, no zone restriction and no need for water. Livestock (group 5),
    /// marine crops and the doodads that only look like seeds are left out.
    pub fn plantable_on_land(&self) -> bool {
        let family = match self.group {
            12 | 3 => true,
            2 => self.allowed_in(FarmKind::Nursery),
            _ => false,
        };
        family
            && !self.ripe.is_empty()
            && self.grow_ms > 0
            && self.restrict_zone == 0
            && !self.needs_water()
            && !self.allowed_in(FarmKind::Stable)
    }
}

/// A public farm polygon.
#[derive(Clone, Debug, PartialEq)]
pub struct FarmArea {
    /// The kind of farm.
    pub kind: FarmKind,
    /// The sub zone id (966, 967, ...).
    pub sub_zone: u32,
    /// The zone key the polygon belongs to.
    pub zone_key: u32,
    /// The polygon, world x/y.
    pub points: Box<[[f32; 2]]>,
    /// `[min_x, min_y, max_x, max_y]`.
    pub bounds: [f32; 4],
    /// The centre of the bounds.
    pub centre: [f32; 2],
}

impl FarmArea {
    /// Whether `(x, y)` is inside the polygon.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        let b = &self.bounds;
        x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3] && contains(&self.points, x, y)
    }

    /// The distance from `(x, y)` to the nearest polygon edge (0 on it); negative inside would be
    /// more useful for margins, see [`FarmArea::depth`].
    fn edge_distance(&self, x: f32, y: f32) -> f32 {
        let n = self.points.len();
        let mut best = f32::MAX;
        for i in 0..n {
            let a = self.points[i];
            let b = self.points[(i + 1) % n];
            best = best.min(segment_distance(a, b, [x, y]));
        }
        best
    }

    /// How far inside the polygon `(x, y)` lies: the distance to the nearest edge, 0 outside.
    pub fn depth(&self, x: f32, y: f32) -> f32 {
        if self.contains(x, y) {
            self.edge_distance(x, y)
        } else {
            0.0
        }
    }

    /// The polygon's area in square metres.
    pub fn area_m2(&self) -> f32 {
        let n = self.points.len();
        let mut sum = 0.0f64;
        for i in 0..n {
            let (a, b) = (self.points[i], self.points[(i + 1) % n]);
            sum += f64::from(a[0]) * f64::from(b[1]) - f64::from(b[0]) * f64::from(a[1]);
        }
        (sum.abs() / 2.0) as f32
    }
}

fn segment_distance(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (a[0] + t * dx, a[1] + t * dy);
    ((p[0] - cx).powi(2) + (p[1] - cy).powi(2)).sqrt()
}

/// Everything the bots know about farming. Built by [`Farming::load`] with the rest of the world.
#[derive(Debug, Default)]
pub struct Farming {
    /// Plantable crops, sorted by seed item.
    crops: Vec<Crop>,
    by_doodad: FxHashMap<u32, u32>,
    /// Crop indexes by item the harvest gives (without the crop's own seed).
    by_produce: FxHashMap<u32, Vec<u32>>,
    /// NPC templates that sell a seed (merchants with a spawn in the world).
    vendors: FxHashMap<u32, Box<[u32]>>,
    livestock: FxHashMap<u32, Livestock>,
    areas: Vec<FarmArea>,
}

impl Farming {
    /// Reads the crops and the seed vendors. `npcs` and `has_spawn` decide which merchants count:
    /// an NPC template with the merchant role and at least one spawn.
    pub fn load(
        conn: &Connection,
        npcs: &IdMap<NpcTemplate>,
        has_spawn: &dyn Fn(u32) -> bool,
    ) -> Result<Farming> {
        if !table_exists(conn, "item_spawn_doodads")? {
            return Ok(Farming::default());
        }
        let mut seeds: Vec<(u32, u32)> = Vec::new();
        each_row(
            conn,
            "SELECT item_id, doodad_id FROM item_spawn_doodads",
            |r| seeds.push((r.u32(0), r.u32(1))),
        )
        .context("read item_spawn_doodads")?;
        let wanted: FxHashSet<u32> = seeds.iter().map(|s| s.1).collect();

        struct Almighty {
            group: u8,
            growth_ms: u32,
            radius: u16,
            restrict: u32,
        }
        let mut almighty: FxHashMap<u32, Almighty> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, group_id, growth_time, sim_radius, restrict_zone_id FROM doodad_almighties",
            |r| {
                let id = r.u32(0);
                if wanted.contains(&id) {
                    almighty.insert(
                        id,
                        Almighty {
                            group: r.u8(1),
                            growth_ms: r.u32(2),
                            radius: r.u16(3),
                            restrict: r.u32(4),
                        },
                    );
                }
            },
        )
        .context("read doodad_almighties")?;

        // phases (func groups) of the planted doodads
        let mut groups_of: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        let mut owner_of_group: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, doodad_almighty_id FROM doodad_func_groups",
            |r| {
                let (g, d) = (r.u32(0), r.u32(1));
                if wanted.contains(&d) {
                    groups_of.entry(d).or_default().push(g);
                    owner_of_group.insert(g, d);
                }
            },
        )
        .context("read doodad_func_groups")?;

        // the delays of the growth funcs
        let mut growth_delay: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(conn, "SELECT id, delay FROM doodad_func_growths", |r| {
            growth_delay.insert(r.u32(0), r.u32(1));
        })
        .context("read doodad_func_growths")?;

        // what a phase hands out: loot packs and single loot items
        let mut pack_of_func: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, loot_pack_id FROM doodad_func_loot_packs",
            |r| {
                pack_of_func.insert(r.u32(0), r.u32(1));
            },
        )
        .context("read doodad_func_loot_packs")?;
        let mut item_of_func: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, item_id FROM doodad_func_loot_items",
            |r| {
                item_of_func.insert(r.u32(0), r.u32(1));
            },
        )
        .context("read doodad_func_loot_items")?;
        let mut loots: FxHashMap<u32, Vec<Yield>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT loot_pack_id, item_id, min_amount, max_amount FROM loots",
            |r| {
                loots.entry(r.u32(0)).or_default().push(Yield {
                    item: r.u32(1),
                    min: r.u16(2).max(1),
                    max: r.u16(3).max(r.u16(2)).max(1),
                });
            },
        )
        .context("read loots")?;

        // the funcs of each phase: (type, actual id, skill, next phase)
        type FuncRow = (String, u32, u32, u32);
        let mut funcs: FxHashMap<u32, Vec<FuncRow>> = FxHashMap::default();
        let mut add_funcs = |sql: &str, with_skill: bool| -> Result<()> {
            each_row(conn, sql, |r| {
                let group = r.u32(0);
                if owner_of_group.contains_key(&group) {
                    let (skill, next) = if with_skill {
                        (r.u32(4), r.u32(5))
                    } else {
                        (0, 0)
                    };
                    funcs.entry(group).or_default().push((
                        r.text(2).to_owned(),
                        r.u32(1),
                        skill,
                        next,
                    ));
                }
            })
            .context("read doodad funcs")
        };
        add_funcs(
            "SELECT doodad_func_group_id, actual_func_id, actual_func_type, 0, func_skill_id, \
             next_phase FROM doodad_funcs",
            true,
        )?;
        add_funcs(
            "SELECT doodad_func_group_id, actual_func_id, actual_func_type FROM doodad_phase_funcs",
            false,
        )?;

        // which public farms allow which crop
        let mut kinds_of: FxHashMap<u32, u8> = FxHashMap::default();
        if table_exists(conn, "farm_group_doodads")? {
            each_row(
                conn,
                "SELECT farm_group_id, doodad_id FROM farm_group_doodads",
                |r| {
                    if let Some(kind) = FarmKind::from_group(r.u32(0)) {
                        *kinds_of.entry(r.u32(1)).or_default() |= kind.bit();
                    }
                },
            )
            .context("read farm_group_doodads")?;
        }

        // what the loot of a phase is
        let produce_of = |group: u32| -> Vec<Yield> {
            let mut out: Vec<Yield> = Vec::new();
            for (ty, actual, _, _) in funcs.get(&group).map_or(&[][..], |v| &v[..]) {
                let found: Vec<Yield> = match ty.as_str() {
                    "DoodadFuncLootPack" => pack_of_func
                        .get(actual)
                        .and_then(|p| loots.get(p))
                        .cloned()
                        .unwrap_or_default(),
                    "DoodadFuncLootItem" => item_of_func
                        .get(actual)
                        .map(|&item| {
                            vec![Yield {
                                item,
                                min: 1,
                                max: 1,
                            }]
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                for y in found {
                    if !out.contains(&y) {
                        out.push(y);
                    }
                }
            }
            out
        };

        let mut crops: Vec<Crop> = Vec::new();
        let mut livestock = FxHashMap::default();
        for &(seed, doodad) in &seeds {
            let Some(a) = almighty.get(&doodad) else {
                continue;
            };
            let mut phases = groups_of.get(&doodad).cloned().unwrap_or_default();
            phases.sort_unstable();
            let growth: u32 = phases
                .iter()
                .flat_map(|g| funcs.get(g).into_iter().flatten())
                .filter(|(ty, ..)| ty == "DoodadFuncGrowth")
                .map(|(_, id, ..)| growth_delay.get(id).copied().unwrap_or(0))
                .fold(0u32, |sum, d| sum.saturating_add(d));
            let grow_ms = if growth > 0 { growth } else { a.growth_ms };
            // ripe: a use func with a skill whose next phase hands out items
            let mut ripe: Vec<(u32, u32)> = Vec::new();
            let mut produce: Vec<Yield> = Vec::new();
            let mut stock = Livestock::default();
            for &g in &phases {
                for (ty, _, skill, next) in funcs.get(&g).into_iter().flatten() {
                    if ty != "DoodadFuncUse" || *skill == 0 || *next == 0 {
                        continue;
                    }
                    let got = produce_of(*next);
                    if a.group == 5 && owner_of_group.get(next) == Some(&doodad) {
                        let action = match *skill {
                            13820 | 13821 | 20595 | 20678 => Some(StockAction::Feed(*skill)),
                            13794 => Some(StockAction::Treat(*skill)),
                            13800..=13802 | 16771 | 20606 | 24243 if !got.is_empty() => {
                                for y in &got {
                                    if !stock.yields.contains(y) {
                                        stock.yields.push(*y);
                                    }
                                }
                                Some(StockAction::Collect(*skill))
                            }
                            _ => None,
                        };
                        if let Some(action) = action {
                            stock.actions.entry(g).or_insert(action);
                        }
                    }
                    if got.is_empty() {
                        continue;
                    }
                    if !ripe.contains(&(g, *skill)) {
                        ripe.push((g, *skill));
                    }
                    if got.len() > produce.len() {
                        produce = got;
                    }
                }
            }
            if ripe.is_empty() || grow_ms == 0 {
                continue;
            }
            if !stock.yields.is_empty() && a.restrict == 0 {
                livestock.insert(doodad, stock);
            }
            crops.push(Crop {
                seed,
                doodad,
                group: a.group,
                grow_ms,
                radius_mm: a.radius,
                restrict_zone: a.restrict,
                ripe: ripe.into_boxed_slice(),
                produce: produce.into_boxed_slice(),
                kinds: kinds_of.get(&doodad).copied().unwrap_or(0),
            });
        }
        crops.sort_by_key(|c| (c.seed, c.doodad));
        crops.dedup_by_key(|c| c.seed);

        // the seed vendors: a merchant NPC (last `merchants` row wins, like the server's loader)
        let mut pack_of_npc: FxHashMap<u32, u32> = FxHashMap::default();
        let mut order: Vec<u32> = Vec::new();
        if table_exists(conn, "merchants")? {
            each_row(
                conn,
                "SELECT npc_id, merchant_pack_id FROM merchants",
                |r| {
                    let npc = r.u32(0);
                    if pack_of_npc.insert(npc, r.u32(1)).is_none() {
                        order.push(npc);
                    }
                },
            )
            .context("read merchants")?;
        }
        let seed_set: FxHashSet<u32> = crops.iter().map(|c| c.seed).collect();
        let mut packs_selling: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        if table_exists(conn, "merchant_goods")? {
            each_row(
                conn,
                "SELECT merchant_pack_id, item_id, grade_id FROM merchant_goods",
                |r| {
                    // (the server sells a grade 0 item only when the row's grade is 0 or NULL)
                    if seed_set.contains(&r.u32(1)) && r.u32(2) == 0 {
                        packs_selling.entry(r.u32(1)).or_default().push(r.u32(0));
                    }
                },
            )
            .context("read merchant_goods")?;
        }
        let mut vendors: FxHashMap<u32, Box<[u32]>> = FxHashMap::default();
        for (seed, packs) in packs_selling {
            let mut npcs_selling: Vec<u32> = order
                .iter()
                .copied()
                .filter(|npc| {
                    pack_of_npc.get(npc).is_some_and(|p| packs.contains(p))
                        && npcs.get(*npc).is_some_and(NpcTemplate::is_merchant)
                        && has_spawn(*npc)
                })
                .collect();
            npcs_selling.sort_unstable();
            npcs_selling.dedup();
            if !npcs_selling.is_empty() {
                vendors.insert(seed, npcs_selling.into_boxed_slice());
            }
        }

        let by_doodad = crops
            .iter()
            .enumerate()
            .map(|(i, c)| (c.doodad, i as u32))
            .collect();
        let mut by_produce: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for (i, c) in crops.iter().enumerate() {
            for y in c.produce.iter().filter(|y| y.item != c.seed) {
                by_produce.entry(y.item).or_default().push(i as u32);
            }
        }
        Ok(Farming {
            crops,
            by_doodad,
            by_produce,
            vendors,
            livestock,
            areas: Vec::new(),
        })
    }

    /// Adds the public farm polygons (`farm_areas.json`); polygons of an unknown sub zone or with
    /// fewer than three corners are dropped.
    pub fn with_areas(mut self, polys: &[HousingAreaPoly]) -> Farming {
        self.areas = polys
            .iter()
            .filter(|p| p.points.len() >= 3)
            .filter_map(|p| {
                let kind = FarmKind::of_sub_zone(p.id)?;
                let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                for [x, y] in &p.points {
                    b = [b[0].min(*x), b[1].min(*y), b[2].max(*x), b[3].max(*y)];
                }
                Some(FarmArea {
                    kind,
                    sub_zone: p.id,
                    zone_key: p.zone_key,
                    points: p.points.clone().into_boxed_slice(),
                    bounds: b,
                    centre: [(b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0],
                })
            })
            .collect();
        self
    }

    /// Every plantable crop, sorted by seed.
    pub fn crops(&self) -> &[Crop] {
        &self.crops
    }

    /// The crop a seed grows.
    pub fn crop_of_seed(&self, seed: u32) -> Option<&Crop> {
        self.crops
            .binary_search_by_key(&seed, |c| c.seed)
            .ok()
            .map(|i| &self.crops[i])
    }

    /// The crop whose doodad is `doodad` (a planted crop in view).
    pub fn crop_of_doodad(&self, doodad: u32) -> Option<&Crop> {
        self.by_doodad
            .get(&doodad)
            .map(|&i| &self.crops[i as usize])
    }

    /// Whether `doodad` is the doodad of a crop.
    pub fn is_crop_doodad(&self, doodad: u32) -> bool {
        self.by_doodad.contains_key(&doodad)
    }

    /// Milk, eggs or wool collected without removing this animal.
    pub fn livestock_yields(&self, doodad: u32) -> Option<&[Yield]> {
        self.livestock.get(&doodad).map(|stock| &stock.yields[..])
    }

    pub fn stock_action(&self, doodad: u32, phase: u32) -> Option<StockAction> {
        self.livestock.get(&doodad)?.actions.get(&phase).copied()
    }

    /// The crops whose harvest gives `item` (their own seed does not count as produce).
    pub fn crops_yielding(&self, item: u32) -> impl Iterator<Item = &Crop> {
        self.by_produce
            .get(&item)
            .into_iter()
            .flatten()
            .map(|&i| &self.crops[i as usize])
    }

    /// The NPC templates that sell `seed` (merchants with a spawn in the world), by template id.
    pub fn vendors_of(&self, seed: u32) -> &[u32] {
        self.vendors.get(&seed).map_or(&[], |v| &v[..])
    }

    /// Whether any vendor sells `seed`.
    pub fn is_sold(&self, seed: u32) -> bool {
        self.vendors.contains_key(&seed)
    }

    /// The public farm polygons.
    pub fn areas(&self) -> &[FarmArea] {
        &self.areas
    }

    /// The public farm that contains `(x, y)`.
    pub fn area_at(&self, x: f32, y: f32) -> Option<&FarmArea> {
        self.areas.iter().find(|a| a.contains(x, y))
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.crops.capacity() * std::mem::size_of::<Crop>()
            + self
                .crops
                .iter()
                .map(|c| c.ripe.len() * 8 + c.produce.len() * 6)
                .sum::<usize>()
            + self.by_doodad.capacity() * 8
            + self
                .by_produce
                .values()
                .map(|v| v.capacity() * 4 + 32)
                .sum::<usize>()
            + self
                .vendors
                .values()
                .map(|v| v.len() * 4 + 32)
                .sum::<usize>()
            + self.areas.capacity() * std::mem::size_of::<FarmArea>()
            + self.areas.iter().map(|a| a.points.len() * 8).sum::<usize>()
            + self
                .livestock
                .values()
                .map(|s| {
                    s.actions.capacity() * 12
                        + s.yields.capacity() * std::mem::size_of::<Yield>()
                        + 48
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed database: a potato (doodad 10: growth 60 s + 540 s, ripe phase 21 with harvest
    /// skill 13980, loot pack 7 with potatoes and the seed), a tree (doodad 11, growth_time only,
    /// allowed in nurseries), a weed that has no growth time, and a merchant NPC 100 that sells
    /// the potato seed.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE item_spawn_doodads (id INT, item_id INT, doodad_id INT);
             CREATE TABLE doodad_almighties (id INT, group_id INT, growth_time INT, sim_radius INT, restrict_zone_id INT);
             CREATE TABLE doodad_func_groups (id INT, doodad_almighty_id INT, doodad_func_group_kind_id INT);
             CREATE TABLE doodad_funcs (id INT, doodad_func_group_id INT, actual_func_id INT,
               actual_func_type TEXT, next_phase INT, func_skill_id INT);
             CREATE TABLE doodad_phase_funcs (id INT, doodad_func_group_id INT, actual_func_id INT, actual_func_type TEXT);
             CREATE TABLE doodad_func_growths (id INT, delay INT, next_phase INT);
             CREATE TABLE doodad_func_loot_packs (id INT, loot_pack_id INT);
             CREATE TABLE doodad_func_loot_items (id INT, item_id INT);
             CREATE TABLE loots (loot_pack_id INT, item_id INT, min_amount INT, max_amount INT);
             CREATE TABLE farm_group_doodads (id INT, farm_group_id INT, doodad_id INT, item_id INT);
             CREATE TABLE merchants (id INT, npc_id INT, merchant_pack_id INT);
             CREATE TABLE merchant_goods (id INT, merchant_pack_id INT, item_id INT, grade_id INT);
             INSERT INTO item_spawn_doodads VALUES (1, 500, 10), (2, 501, 11), (3, 502, 12), (4, 503, 13);
             INSERT INTO doodad_almighties VALUES (10, 12, 600000, 500, NULL), (11, 2, 20580000, 1000, 0),
               (12, 12, NULL, 500, 0), (13, 12, 1000, 500, 77);
             INSERT INTO doodad_func_groups VALUES (1, 10, 1), (2, 10, 2), (3, 10, 2), (4, 10, 2), (5, 10, 2),
               (20, 11, 1), (21, 11, 2), (22, 11, 2), (30, 12, 1), (31, 12, 2),
               (40, 13, 1), (41, 13, 2), (42, 13, 2);
             INSERT INTO doodad_phase_funcs VALUES (1, 1, 100, 'DoodadFuncGrowth'), (2, 2, 101, 'DoodadFuncGrowth'),
               (3, 4, 102, 'DoodadFuncLootPack'), (4, 22, 103, 'DoodadFuncLootItem'), (5, 42, 104, 'DoodadFuncLootItem');
             INSERT INTO doodad_funcs VALUES (1, 3, 1, 'DoodadFuncUse', 4, 13980), (2, 1, 2, 'DoodadFuncUse', 5, 13625),
               (3, 21, 3, 'DoodadFuncUse', 22, 13994), (4, 31, 4, 'DoodadFuncUse', 30, 13980),
               (5, 41, 5, 'DoodadFuncUse', 42, 13980);
             INSERT INTO doodad_func_growths VALUES (100, 60000, 2), (101, 540000, 3);
             INSERT INTO doodad_func_loot_packs VALUES (102, 7);
             INSERT INTO doodad_func_loot_items VALUES (103, 900), (104, 901);
             INSERT INTO loots VALUES (7, 600, 2, 4), (7, 601, 1, 1), (7, 500, 1, 1);
             INSERT INTO farm_group_doodads VALUES (1, 1, 10, 500), (2, 2, 11, 501), (3, 9, 10, 500);
             INSERT INTO merchants VALUES (1, 100, 5), (2, 101, 6), (3, 102, 5);
             INSERT INTO merchant_goods VALUES (1, 5, 500, NULL), (2, 6, 501, 0), (3, 5, 502, 0), (4, 6, 500, 2);",
        )
        .unwrap();
        conn
    }

    fn npcs() -> IdMap<NpcTemplate> {
        let mk = |id: u32, merchant: bool| NpcTemplate {
            id,
            name: "npc".into(),
            level: 1,
            faction_id: 1,
            kind: 1,
            grade: 1,
            aggressive: false,
            roles: if merchant {
                NpcRoles::MERCHANT
            } else {
                NpcRoles::NONE
            },
            is_quest_giver: false,
        };
        IdMap::build(vec![
            (100, mk(100, true)),
            (101, mk(101, false)),
            (102, mk(102, true)),
        ])
    }

    #[test]
    fn crops_come_from_the_phase_graph() {
        let conn = fixture();
        let f = Farming::load(&conn, &npcs(), &|npc| npc != 102).unwrap();
        let seeds: Vec<u32> = f.crops().iter().map(|c| c.seed).collect();
        assert_eq!(
            seeds,
            [500, 501, 503],
            "the weed without a growth time or a harvest is no crop"
        );
        let potato = f.crop_of_seed(500).unwrap();
        assert_eq!(potato.doodad, 10);
        assert_eq!(potato.grow_ms, 600_000, "the growth delays add up");
        assert_eq!(potato.grow_ms_at(1.0), 600_000);
        assert_eq!(potato.grow_ms_at(4.0), 150_000);
        assert_eq!(potato.grow_ms_at(0.0), 600_000, "a bad rate counts as 1");
        assert_eq!(potato.ripe.as_ref(), [(3, 13980)]);
        assert!(potato.is_ripe(3) && !potato.is_ripe(2));
        assert_eq!(potato.harvest_skill(3), Some(13980));
        assert_eq!(potato.skill(), 13980);
        assert_eq!(potato.radius_mm, 500);
        assert!(potato.allowed_in(FarmKind::Farm) && !potato.allowed_in(FarmKind::Nursery));
        assert!(potato.plantable_on_land());
        assert_eq!(
            potato.produce.as_ref(),
            [
                Yield {
                    item: 600,
                    min: 2,
                    max: 4
                },
                Yield {
                    item: 601,
                    min: 1,
                    max: 1
                },
                Yield {
                    item: 500,
                    min: 1,
                    max: 1
                }
            ]
        );
        assert_eq!(potato.yield_of(600).unwrap().average(), 3.0);
        assert!(potato.yield_of(7).is_none());
    }

    #[test]
    fn a_tree_without_growth_funcs_uses_the_doodad_growth_time() {
        let conn = fixture();
        let f = Farming::load(&conn, &npcs(), &|_| true).unwrap();
        let tree = f.crop_of_seed(501).unwrap();
        assert_eq!(tree.grow_ms, 20_580_000);
        assert_eq!(
            tree.produce.as_ref(),
            [Yield {
                item: 900,
                min: 1,
                max: 1
            }]
        );
        assert!(tree.allowed_in(FarmKind::Nursery) && !tree.allowed_in(FarmKind::Farm));
        assert_eq!(f.crop_of_doodad(11).unwrap().seed, 501);
        assert!(f.is_crop_doodad(10) && !f.is_crop_doodad(12));
    }

    #[test]
    fn crops_with_a_zone_restriction_are_not_plantable() {
        let conn = fixture();
        let f = Farming::load(&conn, &npcs(), &|_| true).unwrap();
        let c = f.crop_of_seed(503).unwrap();
        assert_eq!(c.restrict_zone, 77);
        assert!(!c.plantable_on_land());
        // a doodad with no growth time at all is no crop
        assert!(f.crop_of_seed(502).is_none());
    }

    #[test]
    fn the_produce_index_leaves_out_the_seed() {
        let conn = fixture();
        let f = Farming::load(&conn, &npcs(), &|_| true).unwrap();
        let from: Vec<u32> = f.crops_yielding(600).map(|c| c.seed).collect();
        assert_eq!(from, [500]);
        assert_eq!(
            f.crops_yielding(500).count(),
            0,
            "the seed is not the produce"
        );
        assert_eq!(f.crops_yielding(12345).count(), 0);
    }

    #[test]
    fn vendors_are_merchants_with_a_spawn_that_sell_the_seed_at_grade_zero() {
        let conn = fixture();
        let f = Farming::load(&conn, &npcs(), &|npc| npc != 102).unwrap();
        // 100 sells pack 5 (seed 500); 102 has no spawn; 101 is not a merchant
        assert_eq!(f.vendors_of(500), [100]);
        assert!(f.is_sold(500));
        assert!(
            f.vendors_of(501).is_empty(),
            "pack 6 belongs to a non-merchant"
        );
        assert!(!f.is_sold(501));
        let all = Farming::load(&conn, &npcs(), &|_| true).unwrap();
        assert_eq!(all.vendors_of(500), [100, 102]);
    }

    fn square(id: u32, x: f32, y: f32, side: f32) -> HousingAreaPoly {
        HousingAreaPoly {
            id,
            zone_key: 7,
            points: vec![[x, y], [x + side, y], [x + side, y + side], [x, y + side]],
        }
    }

    #[test]
    fn farm_areas_know_their_kind_and_depth() {
        let f = Farming::default().with_areas(&[
            square(966, 100.0, 100.0, 40.0),
            square(968, 300.0, 100.0, 20.0),
            square(12345, 500.0, 100.0, 20.0),
            HousingAreaPoly {
                id: 966,
                zone_key: 7,
                points: vec![[0.0, 0.0], [1.0, 1.0]],
            },
        ]);
        assert_eq!(
            f.areas().len(),
            2,
            "an unknown sub zone and a line are dropped"
        );
        let field = f.area_at(110.0, 110.0).unwrap();
        assert_eq!((field.kind, field.sub_zone), (FarmKind::Farm, 966));
        assert_eq!(f.area_at(310.0, 110.0).unwrap().kind, FarmKind::Nursery);
        assert!(f.area_at(500.0, 110.0).is_none());
        assert_eq!(field.centre, [120.0, 120.0]);
        assert!((field.depth(120.0, 120.0) - 20.0).abs() < 1e-3);
        assert!((field.depth(103.0, 120.0) - 3.0).abs() < 1e-3);
        assert_eq!(field.depth(90.0, 120.0), 0.0, "outside is no depth");
        assert!((field.area_m2() - 1600.0).abs() < 1e-2);
        assert_eq!(FarmKind::of_sub_zone(998), Some(FarmKind::Farm));
        assert_eq!(FarmKind::of_sub_zone(1), None);
        assert_eq!(farm_limit(FarmKind::Farm), 10);
        assert_eq!(farm_limit(FarmKind::Nursery), 5);
    }

    #[test]
    fn a_database_without_seeds_has_no_crops() {
        let conn = Connection::open_in_memory().unwrap();
        let f = Farming::load(&conn, &IdMap::default(), &|_| true).unwrap();
        assert!(f.crops().is_empty() && f.areas().is_empty());
        assert!(f.bytes() < 1024);
    }
}
