//! Housing: buildable designs (with their price class) and which designs each housing area
//! allows, plus point-in-polygon lookup over the baked area outlines.
//!
//! Table semantics follow the server's `systems/housing/data.rs` (AAEmu Rust,
//! GPL-3.0-or-later): `housing_areas -> housing_groups -> housing_group_categories`.

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::baked::HousingAreaPoly;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// A one-action construction step that consumes one type of bag material.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildAction {
    pub skill: u32,
    pub material: u32,
    pub quantity: u32,
}

/// A house design a character can place with the matching design item.
#[derive(Clone, Debug, PartialEq)]
pub struct HousingDesign {
    /// `housings.id` (the design id sent in `CSCreateHouse`).
    pub design: u32,
    /// The design item (`item_housings.item_id`) that must be held to build it.
    pub item: u32,
    /// `housings.category_id`; `housing_group_categories` lists the categories each area allows.
    pub category: u32,
    /// Tax in copper (`taxations.tax`).
    pub tax: u32,
    /// A regular player design: taxed and listed (`taxations.show`). Tax-free special designs
    /// (castle buildings, lodestones, event houses) are not standard and sort last.
    pub standard: bool,
    /// Number of build steps (`housing_build_steps`).
    pub steps: u8,
    /// Authored action for a design whose construction takes only one supported cast.
    pub build_action: Option<BuildAction>,
    /// Radius in metres of the garden around the house (`housings.garden_radius`): the free
    /// ground the server's placement check needs, 4.0 for a Scarecrow Garden, 7.5 for a cottage.
    pub garden_radius: f32,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
}

/// Designs and area rules.
#[derive(Debug, Default)]
pub struct Housing {
    /// Standard designs first, then by `(tax, steps, design)`.
    designs: Vec<HousingDesign>,
    /// `housing_areas.id -> housing_group_id`.
    area_group: FxHashMap<u32, u32>,
    /// `housing_group_id -> allowed category ids`.
    group_categories: FxHashMap<u32, Box<[u32]>>,
}

impl Housing {
    /// Loads `housings`, `taxations`, `housing_build_steps`, `item_housings`, `housing_areas` and
    /// `housing_group_categories`. Designs without an existing design item, or of a category no
    /// housing area accepts (castle parts, turrets), are not buildable and are left out.
    pub fn load(
        conn: &Connection,
        texts: &Texts,
        item_exists: &dyn Fn(u32) -> bool,
    ) -> Result<Housing> {
        let mut steps: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT housing_id, COUNT(*) FROM housing_build_steps GROUP BY housing_id",
            |r| {
                steps.insert(r.u32(0), r.u32(1));
            },
        )?;
        let mut actions = FxHashMap::default();
        each_row(
            conn,
            "SELECT b.housing_id, MAX(b.skill_id), MAX(e.consume_item_id), MAX(e.consume_item_count)
             FROM housing_build_steps b JOIN skill_effects e ON e.skill_id = b.skill_id
             WHERE b.step = 0 AND b.num_actions = 1 AND e.consume_item_id > 0
               AND e.consume_item_count > 0
               AND COALESCE(LOWER(e.consume_source_item), 'f') IN ('f', 'false', '0')
             GROUP BY b.housing_id HAVING COUNT(*) = 1",
            |r| {
                let design = r.u32(0);
                if steps.get(&design) == Some(&1) {
                    actions.insert(
                        design,
                        BuildAction {
                            skill: r.u32(1),
                            material: r.u32(2),
                            quantity: r.u32(3),
                        },
                    );
                }
            },
        )?;
        let mut area_group = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, housing_group_id FROM housing_areas",
            |r| {
                area_group.insert(r.u32(0), r.u32(1));
            },
        )?;
        let mut cats: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT housing_group_id, category_id FROM housing_group_categories",
            |r| cats.entry(r.u32(0)).or_default().push(r.u32(1)),
        )?;
        let group_categories: FxHashMap<u32, Box<[u32]>> = cats
            .into_iter()
            .map(|(g, mut v)| {
                v.sort_unstable();
                v.dedup();
                (g, v.into_boxed_slice())
            })
            .collect();
        // Categories no housing area accepts (castle parts, siege turrets, ...) cannot be placed
        // with `CSCreateHouse`.
        let placeable: FxHashSet<u32> = group_categories.values().flatten().copied().collect();

        let mut item_of_design: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT design_id, item_id FROM item_housings ORDER BY rowid",
            |r| {
                if item_exists(r.u32(1)) {
                    item_of_design.entry(r.u32(0)).or_insert(r.u32(1));
                }
            },
        )?;
        let mut designs = Vec::new();
        each_row(
            conn,
            "SELECT h.id, h.name, h.category_id, COALESCE(t.tax, 0), COALESCE(t.\"SHOW\", 'f'), h.garden_radius \n             FROM housings h LEFT JOIN taxations t ON t.id = h.taxation_id",
            |r| {
                let id = r.u32(0);
                if !placeable.contains(&r.u32(2)) {
                    return;
                }
                if let Some(&item) = item_of_design.get(&id) {
                    designs.push(HousingDesign {
                        design: id,
                        item,
                        category: r.u32(2),
                        tax: r.u32(3),
                        standard: r.flag(4),
                        steps: steps.get(&id).copied().unwrap_or(0).min(255) as u8,
                        build_action: actions.get(&id).copied(),
                        garden_radius: r.f32(5),
                        name: texts.name_or(TextTable::Housing, id, r.text(1)),
                    });
                }
            },
        )?;
        designs.sort_by_key(|d| (!d.standard, d.tax, d.steps, d.design));

        Ok(Housing {
            designs,
            area_group,
            group_categories,
        })
    }

    /// Buildable designs: standard player designs cheapest first, then the special ones.
    pub fn designs(&self) -> &[HousingDesign] {
        &self.designs
    }

    /// The design with this id.
    pub fn design(&self, design: u32) -> Option<&HousingDesign> {
        self.designs.iter().find(|d| d.design == design)
    }

    /// Whether housing area `area_id` accepts houses of `category` (the static group rule; the
    /// per-account "houseless" rules are enforced by the server).
    pub fn area_allows(&self, area_id: u32, category: u32) -> bool {
        self.area_group
            .get(&area_id)
            .and_then(|g| self.group_categories.get(g))
            .is_some_and(|cats| cats.binary_search(&category).is_ok())
    }

    /// Number of known housing areas.
    pub fn area_count(&self) -> usize {
        self.area_group.len()
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.designs.capacity() * std::mem::size_of::<HousingDesign>()
            + self.designs.iter().map(|d| d.name.len()).sum::<usize>()
            + self.area_group.capacity() * 12
            + self
                .group_categories
                .values()
                .map(|v| v.len() * 4 + 24)
                .sum::<usize>()
    }
}

/// Bounding boxes of the baked area polygons for fast point lookup.
#[derive(Debug, Default)]
pub struct AreaIndex {
    /// `[min_x, min_y, max_x, max_y]` per polygon, parallel to the baked polygon list.
    boxes: Vec<[f32; 4]>,
}

impl AreaIndex {
    /// Precomputes the bounding boxes.
    pub fn build(polys: &[HousingAreaPoly]) -> AreaIndex {
        let boxes = polys
            .iter()
            .map(|p| {
                let mut b = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
                for [x, y] in &p.points {
                    b = [b[0].min(*x), b[1].min(*y), b[2].max(*x), b[3].max(*y)];
                }
                b
            })
            .collect();
        AreaIndex { boxes }
    }

    /// The `housing_areas.id` of the first polygon containing `(x, y)`.
    pub fn at(&self, polys: &[HousingAreaPoly], x: f32, y: f32) -> Option<u32> {
        self.boxes
            .iter()
            .zip(polys)
            .find(|(b, p)| {
                x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3] && contains(&p.points, x, y)
            })
            .map(|(_, p)| p.id)
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.boxes.capacity() * 16
    }
}

/// Even-odd point-in-polygon test.
pub fn contains(points: &[[f32; 2]], x: f32, y: f32) -> bool {
    let mut inside = false;
    let n = points.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let [xi, yi] = points[i];
        let [xj, yj] = points[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE housings (id INT, name TEXT, category_id INT, taxation_id INT, garden_radius REAL);
             CREATE TABLE taxations (id INT, tax INT, SHOW NUM);
             CREATE TABLE housing_build_steps (id INT, housing_id INT, step INT, skill_id INT, num_actions INT);
             CREATE TABLE skill_effects (skill_id INT, consume_item_id INT, consume_item_count INT, consume_source_item TEXT);
             CREATE TABLE item_housings (id INT, item_id INT, design_id INT);
             CREATE TABLE housing_areas (id INT, housing_group_id INT);
             CREATE TABLE housing_group_categories (id INT, housing_group_id INT, category_id INT);
             INSERT INTO taxations VALUES (1, 0, 'f'), (2, 100000, 't'), (8, 50000, 't');
             INSERT INTO housings VALUES (172, 'small house', 1, 2, 7.5), (267, 'scarecrow', 16, 8, 4.0), (5, 'no item', 1, 2, 7.5), (6, 'no tax row', 17, 99, NULL), (8, 'siege part', 3, 2, 12.0), (9, 'free', 1, 1, 9);
             INSERT INTO housing_build_steps VALUES (1, 172, 0, 18553, 1), (2, 172, 1, 18553, 1), (3, 172, 2, 18553, 1), (4, 267, 0, 18553, 1);
             INSERT INTO skill_effects VALUES (18553, 8337, 1, 'f');
             INSERT INTO item_housings VALUES (106, 21166, 172), (58, 15596, 267), (59, 999, 5), (60, 7, 6), (61, 8, 8), (62, 9, 9);
             INSERT INTO housing_areas VALUES (52, 1), (60, 14), (61, 11);
             INSERT INTO housing_group_categories VALUES (1, 1, 1), (2, 1, 16), (3, 14, 1), (4, 14, 17);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn designs_sorted_cheapest_first_and_need_an_item() {
        let conn = fixture();
        let mut texts = Texts::empty();
        texts.insert(TextTable::Housing, 267, "Scarecrow Garden");
        // item 999 does not exist (design 5) and no area accepts category 3 (design 8).
        let h = Housing::load(&conn, &texts, &|i| i != 999).unwrap();
        let ids: Vec<u32> = h.designs().iter().map(|d| d.design).collect();
        // Standard designs cheapest first; the tax-free ones (6 has no tax row, 9 is free) last.
        assert_eq!(ids, vec![267, 172, 6, 9]);
        assert!(h.design(267).unwrap().standard && !h.design(9).unwrap().standard);
        let d = h.design(267).unwrap();
        assert_eq!((d.item, d.category, d.tax, d.steps), (15596, 16, 50000, 1));
        assert_eq!(&*d.name, "Scarecrow Garden");
        assert_eq!(h.design(172).unwrap().steps, 3);
        assert_eq!(h.design(172).unwrap().build_action, None);
        assert_eq!(
            d.build_action,
            Some(BuildAction {
                skill: 18553,
                material: 8337,
                quantity: 1
            })
        );
        // the footprint comes from the data (a missing value reads as 0, an integer as a float)
        assert_eq!(d.garden_radius, 4.0);
        assert_eq!(h.design(172).unwrap().garden_radius, 7.5);
        assert_eq!(h.design(6).unwrap().garden_radius, 0.0);
        assert_eq!(h.design(9).unwrap().garden_radius, 9.0);
        assert_eq!(h.design(6).unwrap().tax, 0);
    }

    #[test]
    fn area_rules_follow_group_categories() {
        let conn = fixture();
        let h = Housing::load(&conn, &Texts::empty(), &|_| true).unwrap();
        assert!(h.area_allows(52, 1) && h.area_allows(52, 16));
        assert!(!h.area_allows(52, 17));
        assert!(h.area_allows(60, 17) && !h.area_allows(60, 16));
        assert!(!h.area_allows(61, 1)); // group without categories
        assert!(!h.area_allows(999, 1)); // unknown area
        assert_eq!(h.area_count(), 3);
    }

    #[test]
    fn point_in_polygon_and_area_lookup() {
        let square = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let l_shape = vec![
            [20.0, 0.0],
            [30.0, 0.0],
            [30.0, 5.0],
            [25.0, 5.0],
            [25.0, 10.0],
            [20.0, 10.0],
        ];
        assert!(contains(&square, 5.0, 5.0));
        assert!(!contains(&square, 11.0, 5.0));
        assert!(contains(&l_shape, 22.0, 8.0));
        assert!(!contains(&l_shape, 28.0, 8.0)); // inside the bounding box, outside the L
        let polys = vec![
            HousingAreaPoly {
                id: 1,
                zone_key: 0,
                points: square,
            },
            HousingAreaPoly {
                id: 2,
                zone_key: 0,
                points: l_shape,
            },
        ];
        let idx = AreaIndex::build(&polys);
        assert_eq!(idx.at(&polys, 5.0, 5.0), Some(1));
        assert_eq!(idx.at(&polys, 22.0, 8.0), Some(2));
        assert_eq!(idx.at(&polys, 28.0, 8.0), None);
        assert_eq!(idx.at(&polys, -5.0, 5.0), None);
        assert!(!contains(&[[0.0, 0.0], [1.0, 1.0]], 0.5, 0.5));
    }
}
