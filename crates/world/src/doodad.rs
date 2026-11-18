//! Doodad templates (`doodad_almighties` + their `doodad_funcs`): what a doodad does when
//! used, which crafts a workbench offers, and which doodads yield a given item.
//!
//! Table semantics follow the server's `systems/npcs/doodad/{data,phase}.rs` (AAEmu Rust,
//! GPL-3.0-or-later): the start group is `doodad_func_group_kind_id` 1, benches are
//! `DoodadFuncCraftPack` / `DoodadFuncCraftStart` functions.

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// What a doodad can do, as a bit set over all its function groups.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DoodadFlags(pub u16);

impl DoodadFlags {
    /// Gives a quest when used (`DoodadFuncQuest`).
    pub const QUEST: DoodadFlags = DoodadFlags(1 << 0);
    /// Crafting bench (`DoodadFuncCraftPack` / `DoodadFuncCraftStart`).
    pub const CRAFT: DoodadFlags = DoodadFlags(1 << 1);
    /// Plain use (`DoodadFuncUse` / `DoodadFuncFakeUse`): doors, levers, chests, quest props.
    pub const USE: DoodadFlags = DoodadFlags(1 << 2);
    /// Yields items (`DoodadFuncLootItem` / `DoodadFuncLootPack`).
    pub const LOOT: DoodadFlags = DoodadFlags(1 << 3);
    /// Harvest / mining / felling style gathering.
    pub const GATHER: DoodadFlags = DoodadFlags(1 << 4);
    /// Sells something (`DoodadFuncPurchase`).
    pub const PURCHASE: DoodadFlags = DoodadFlags(1 << 5);
    /// Active sport fishing school or fish stand.
    pub const FISHING: DoodadFlags = DoodadFlags(1 << 6);
    /// Opens the player's mailbox.
    pub const MAILBOX: DoodadFlags = DoodadFlags(1 << 7);

    /// The empty set.
    pub const NONE: DoodadFlags = DoodadFlags(0);

    /// Whether every flag of `other` is in `self`.
    pub const fn has(self, other: DoodadFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// Union.
    pub const fn with(self, other: DoodadFlags) -> DoodadFlags {
        DoodadFlags(self.0 | other.0)
    }
}

impl std::fmt::Debug for DoodadFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const NAMES: [(DoodadFlags, &str); 8] = [
            (DoodadFlags::QUEST, "quest"),
            (DoodadFlags::CRAFT, "craft"),
            (DoodadFlags::USE, "use"),
            (DoodadFlags::LOOT, "loot"),
            (DoodadFlags::GATHER, "gather"),
            (DoodadFlags::PURCHASE, "purchase"),
            (DoodadFlags::FISHING, "fishing"),
            (DoodadFlags::MAILBOX, "mailbox"),
        ];
        f.write_str("DoodadFlags[")?;
        let mut first = true;
        for (flag, name) in NAMES {
            if self.has(flag) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        f.write_str("]")
    }
}

/// One doodad template.
#[derive(Clone, Debug, PartialEq)]
pub struct DoodadTemplate {
    /// `doodad_almighties.id`.
    pub id: u32,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
    /// What it can do.
    pub flags: DoodadFlags,
    /// The skill a client starts to use it: the skill of its first function that names one (start
    /// group first) -- the function's `func_skill_id`, else the `fake_skill_id` of a
    /// `DoodadFuncFakeUse` or the `skill_id` of a `DoodadFuncUse` (quest props such as signposts
    /// and levers only have the latter). 0 = none (the client's default interaction applies).
    pub use_skill: u32,
}

impl DoodadTemplate {
    /// Whether this doodad is a crafting bench.
    pub fn offers_crafts(&self) -> bool {
        self.flags.has(DoodadFlags::CRAFT)
    }
}

/// Doodad data resolved at load time.
#[derive(Debug, Default)]
pub struct DoodadData {
    /// Templates by id.
    pub templates: IdMap<DoodadTemplate>,
    /// `(doodad template, craft id)` pairs: crafts offered by `DoodadFuncCraftPack` /
    /// `DoodadFuncCraftStart` functions.
    pub bench_pairs: Vec<(u32, u32)>,
    /// `(doodad template, is_pack, func row id)` of every `DoodadFuncLootItem` (false) /
    /// `DoodadFuncLootPack` (true) function.
    loot_funcs: Vec<(u32, bool, u32)>,
    /// Sorted doodad templates a backpack turns into when it is put down or dropped on death
    /// (`put_down_backpack_effects.backpack_doodad_id`): the doodads a pack can be picked up
    /// from again (`DoodadFuncRecoverItem`).
    pack_doodads: Vec<u32>,
    pub fish_sales: Vec<FishSale>,
    pub fish_schools: FxHashMap<u32, Vec<u32>>,
    pub sport_fish: FxHashSet<u32>,
    pub fish_chum: FxHashMap<u32, Vec<FishChum>>,
    pub mailbox_phases: FxHashSet<(u32, u32)>,
}

#[derive(Debug, Clone, Copy)]
pub struct FishSale {
    pub stand: u32,
    pub phase: u32,
    pub item: u32,
    pub skill: u32,
    pub copper: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct FishChum {
    pub item: u32,
    pub skill: u32,
    pub next_phase: u32,
}

const GATHER_FUNCS: [&str; 17] = [
    "DoodadFuncOreMine",
    "DoodadFuncRockMine",
    "DoodadFuncCropHarvest",
    "DoodadFuncCerealHarvest",
    "DoodadFuncFruitPick",
    "DoodadFuncFiberCollect",
    "DoodadFuncPlantCollect",
    "DoodadFuncMedicalingredientMine",
    "DoodadFuncHarvest",
    "DoodadFuncCutdown",
    "DoodadFuncCutdowning",
    "DoodadFuncDig",
    "DoodadFuncSpiceCollect",
    "DoodadFuncSoilCollect",
    "DoodadFuncCrystalCollect",
    "DoodadFuncGassExtract",
    "DoodadFuncTreeByproductsCollect",
];

/// Loads templates, bench pairs and loot function rows.
pub fn load(conn: &Connection, texts: &Texts) -> Result<DoodadData> {
    let mut names: Vec<(u32, Box<str>)> = Vec::new();
    each_row(conn, "SELECT id, name FROM doodad_almighties", |r| {
        let id = r.u32(0);
        names.push((id, texts.name_or(TextTable::Doodad, id, r.text(1))));
    })?;

    let mut craft_packs: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT craft_pack_id, craft_id FROM craft_pack_crafts",
        |r| craft_packs.entry(r.u32(0)).or_default().push(r.u32(1)),
    )?;
    let mut pack_of_func: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT id, craft_pack_id FROM doodad_func_craft_packs",
        |r| {
            pack_of_func.insert(r.u32(0), r.u32(1));
        },
    )?;
    let mut start_crafts: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT doodad_func_craft_start_id, craft_id FROM doodad_func_craft_start_crafts",
        |r| start_crafts.entry(r.u32(0)).or_default().push(r.u32(1)),
    )?;

    // The skills of plain-use functions that carry no `func_skill_id` of their own.
    let mut fake_use_skill: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT id, fake_skill_id FROM doodad_func_fake_uses WHERE fake_skill_id != 0",
        |r| {
            fake_use_skill.insert(r.u32(0), r.u32(1));
        },
    )?;
    let mut use_func_skill: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT id, skill_id FROM doodad_func_uses WHERE skill_id != 0",
        |r| {
            use_func_skill.insert(r.u32(0), r.u32(1));
        },
    )?;

    let mut flags: FxHashMap<u32, DoodadFlags> = FxHashMap::default();
    let mut use_skill: FxHashMap<u32, u32> = FxHashMap::default();
    let mut bench_pairs: Vec<(u32, u32)> = Vec::new();
    let mut loot_funcs = Vec::new();
    let mut mailbox_phases = FxHashSet::default();
    each_row(
        conn,
        "SELECT g.doodad_almighty_id, f.actual_func_type, f.actual_func_id, f.func_skill_id, g.id \
         FROM doodad_funcs f JOIN doodad_func_groups g ON g.id = f.doodad_func_group_id \
         ORDER BY g.doodad_almighty_id, g.doodad_func_group_kind_id, g.id, f.id",
        |r| {
            let doodad = r.u32(0);
            let ty = r.text(1);
            let actual = r.u32(2);
            let mut add = DoodadFlags::NONE;
            match ty {
                "DoodadFuncQuest" => add = DoodadFlags::QUEST,
                "DoodadFuncCraftPack" => {
                    add = DoodadFlags::CRAFT;
                    if let Some(crafts) = pack_of_func.get(&actual).and_then(|p| craft_packs.get(p))
                    {
                        bench_pairs.extend(crafts.iter().map(|c| (doodad, *c)));
                    }
                }
                "DoodadFuncCraftStart" => {
                    add = DoodadFlags::CRAFT;
                    if let Some(crafts) = start_crafts.get(&actual) {
                        bench_pairs.extend(crafts.iter().map(|c| (doodad, *c)));
                    }
                }
                "DoodadFuncUse" | "DoodadFuncFakeUse" => add = DoodadFlags::USE,
                "DoodadFuncLootItem" => {
                    add = DoodadFlags::LOOT;
                    loot_funcs.push((doodad, false, actual));
                }
                "DoodadFuncLootPack" => {
                    add = DoodadFlags::LOOT;
                    loot_funcs.push((doodad, true, actual));
                }
                "DoodadFuncPurchase" => add = DoodadFlags::PURCHASE,
                "DoodadFuncNaviOpenMailbox" => {
                    add = DoodadFlags::MAILBOX;
                    mailbox_phases.insert((doodad, r.u32(4)));
                }
                t if GATHER_FUNCS.contains(&t) => add = DoodadFlags::GATHER,
                _ => {}
            }
            let e = flags.entry(doodad).or_default();
            *e = e.with(add);
            let skill = match r.u32(3) {
                0 => match ty {
                    "DoodadFuncFakeUse" => fake_use_skill.get(&actual).copied().unwrap_or(0),
                    "DoodadFuncUse" => use_func_skill.get(&actual).copied().unwrap_or(0),
                    _ => 0,
                },
                direct => direct,
            };
            if skill != 0 {
                use_skill.entry(doodad).or_insert(skill);
            }
        },
    )?;
    bench_pairs.sort_unstable();
    bench_pairs.dedup();

    // (the table is missing from a database without the put-down effect: no pack doodads then)
    let mut pack_doodads: Vec<u32> = Vec::new();
    let _ = each_row(
        conn,
        "SELECT backpack_doodad_id FROM put_down_backpack_effects WHERE backpack_doodad_id != 0",
        |r| pack_doodads.push(r.u32(0)),
    );
    pack_doodads.sort_unstable();
    pack_doodads.dedup();

    let mut fish_sales = Vec::new();
    let _ = each_row(
        conn,
        "SELECT DISTINCT g.doodad_almighty_id, g.id, b.item_id, f.func_skill_id, i.refund
         FROM doodad_funcs f JOIN doodad_func_groups g ON g.id = f.doodad_func_group_id
         JOIN doodad_func_buy_fish_items b ON b.doodad_func_buy_fish_id = f.actual_func_id
         JOIN items i ON i.id = b.item_id
         WHERE f.actual_func_type = 'DoodadFuncBuyFish' AND f.func_skill_id > 0 AND i.refund >= 0",
        |r| {
            fish_sales.push(FishSale {
                stand: r.u32(0),
                phase: r.u32(1),
                item: r.u32(2),
                skill: r.u32(3),
                copper: r.u32(4),
            });
        },
    );
    let mut fish_schools = FxHashMap::<u32, Vec<u32>>::default();
    let _ = each_row(
        conn,
        "SELECT DISTINCT p.doodad_func_group_id, n.member_id, g.doodad_almighty_id
         FROM doodad_phase_funcs p JOIN doodad_func_fish_schools f ON f.id = p.actual_func_id
         JOIN npc_spawner_npcs n ON n.npc_spawner_id = f.npc_spawner_id
         JOIN doodad_func_groups g ON g.id = p.doodad_func_group_id
         WHERE p.actual_func_type = 'DoodadFuncFishSchool'",
        |r| {
            fish_schools.entry(r.u32(0)).or_default().push(r.u32(1));
            let flag = flags.entry(r.u32(2)).or_default();
            *flag = flag.with(DoodadFlags::FISHING);
        },
    );
    for stand in &fish_sales {
        let flag = flags.entry(stand.stand).or_default();
        *flag = flag.with(DoodadFlags::FISHING);
    }
    let mut fish_chum = FxHashMap::<u32, Vec<FishChum>>::default();
    let _ = each_row(
        conn,
        "SELECT DISTINCT g.id, g.doodad_almighty_id, f.next_phase, h.skill_id, i.id
         FROM doodad_funcs f JOIN doodad_func_groups g ON g.id = f.doodad_func_group_id
         JOIN doodad_func_skill_hits h ON h.id = f.actual_func_id
         JOIN items i ON i.use_skill_id = h.skill_id
         WHERE f.actual_func_type = 'DoodadFuncSkillHit' AND i.use_skill_id > 0",
        |r| {
            let next_phase = r.u32(2);
            if fish_schools.contains_key(&next_phase) {
                fish_chum.entry(r.u32(0)).or_default().push(FishChum {
                    item: r.u32(4),
                    skill: r.u32(3),
                    next_phase,
                });
                let flag = flags.entry(r.u32(1)).or_default();
                *flag = flag.with(DoodadFlags::FISHING);
            }
        },
    );

    let templates = names
        .into_iter()
        .map(|(id, name)| {
            (
                id,
                DoodadTemplate {
                    id,
                    name,
                    flags: flags.get(&id).copied().unwrap_or_default(),
                    use_skill: use_skill.get(&id).copied().unwrap_or(0),
                },
            )
        })
        .collect();
    let sport_fish = fish_schools.values().flatten().copied().collect();
    Ok(DoodadData {
        templates: IdMap::build(templates),
        bench_pairs,
        loot_funcs,
        pack_doodads,
        fish_sales,
        fish_schools,
        sport_fish,
        fish_chum,
        mailbox_phases,
    })
}

impl DoodadData {
    /// Whether `template` is what a put-down or dropped backpack turns into.
    pub fn is_pack_doodad(&self, template: u32) -> bool {
        self.pack_doodads.binary_search(&template).is_ok()
    }

    /// For each wanted item, the doodad templates that yield it (via `DoodadFuncLootItem` or
    /// `DoodadFuncLootPack`) and for which `keep(doodad)` holds (typically "has a spawn").
    pub fn item_sources(
        &self,
        conn: &Connection,
        items: &FxHashSet<u32>,
        keep: impl Fn(u32) -> bool,
    ) -> Result<FxHashMap<u32, Box<[u32]>>> {
        let mut by_item_func: FxHashMap<u32, Vec<u32>> = FxHashMap::default(); // loot item row -> items
        each_row(
            conn,
            "SELECT id, item_id FROM doodad_func_loot_items",
            |r| {
                if items.contains(&r.u32(1)) {
                    by_item_func.entry(r.u32(0)).or_default().push(r.u32(1));
                }
            },
        )?;
        let mut pack_of_func: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, loot_pack_id FROM doodad_func_loot_packs",
            |r| {
                pack_of_func.insert(r.u32(0), r.u32(1));
            },
        )?;
        let mut items_of_pack: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT loot_pack_id, item_id, drop_rate, always_drop FROM loots",
            |r| {
                if items.contains(&r.u32(1)) && (r.int(2) > 0 || r.flag(3)) {
                    items_of_pack.entry(r.u32(0)).or_default().push(r.u32(1));
                }
            },
        )?;

        let mut out: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for &(doodad, is_pack, func) in &self.loot_funcs {
            if !keep(doodad) {
                continue;
            }
            let wanted: &[u32] = if is_pack {
                pack_of_func
                    .get(&func)
                    .and_then(|p| items_of_pack.get(p))
                    .map_or(&[], |v| &v[..])
            } else {
                by_item_func.get(&func).map_or(&[], |v| &v[..])
            };
            for &item in wanted {
                out.entry(item).or_default().push(doodad);
            }
        }
        Ok(out
            .into_iter()
            .map(|(item, mut doodads)| {
                doodads.sort_unstable();
                doodads.dedup();
                (item, doodads.into_boxed_slice())
            })
            .collect())
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.templates.shallow_bytes()
            + self
                .templates
                .values()
                .iter()
                .map(|d| d.name.len())
                .sum::<usize>()
            + self.bench_pairs.capacity() * 8
            + self.loot_funcs.capacity() * std::mem::size_of::<(u32, bool, u32)>()
            + self.pack_doodads.capacity() * 4
    }
}

#[cfg(test)]
#[test]
#[ignore = "needs AA_COMPACT"]
fn r208022_catalog_contains_sport_fish_stands_and_active_schools() {
    let path = std::env::var_os("AA_COMPACT").expect("AA_COMPACT");
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let data = load(&conn, &Texts::default()).unwrap();
    assert!(data
        .fish_sales
        .iter()
        .any(|sale| { sale.item == 27602 && sale.copper == 400000 && sale.skill != 0 }));
    assert!(data.fish_schools.values().any(|fish| !fish.is_empty()));
    assert!(data
        .fish_chum
        .values()
        .flatten()
        .any(|chum| chum.item == 27313 && chum.skill == 21693));
}

#[cfg(test)]
pub(crate) mod fixture {
    use super::*;

    /// Creates the doodad-related tables with a bench (doodad 4233 offering craft 6226 through
    /// pack 68), a gatherable flower (doodad 1598 looting item 14400) and a quest sign (5047).
    pub fn create(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE doodad_almighties (id INT, name TEXT);
             CREATE TABLE doodad_func_groups (id INT, doodad_almighty_id INT, doodad_func_group_kind_id INT);
             CREATE TABLE doodad_funcs (id INT, doodad_func_group_id INT, actual_func_id INT,
               actual_func_type TEXT, func_skill_id INT);
             CREATE TABLE craft_pack_crafts (id INT, craft_pack_id INT, craft_id INT);
             CREATE TABLE doodad_func_craft_packs (id INT, craft_pack_id INT);
             CREATE TABLE doodad_func_craft_start_crafts (id INT, doodad_func_craft_start_id INT, craft_id INT);
             CREATE TABLE doodad_func_loot_items (id INT, item_id INT);
             CREATE TABLE doodad_func_loot_packs (id INT, loot_pack_id INT);
             CREATE TABLE doodad_func_fake_uses (id INT, skill_id INT, fake_skill_id INT);
             CREATE TABLE doodad_func_uses (id INT, skill_id INT);
             CREATE TABLE loots (loot_pack_id INT, item_id INT, drop_rate INT, always_drop TEXT);
             INSERT INTO doodad_almighties VALUES (4233, 'bench'), (1598, 'flower'), (5047, 'sign');
             INSERT INTO doodad_func_groups VALUES (10, 4233, 1), (11, 4233, 2), (12, 1598, 1), (13, 5047, 1);
             INSERT INTO doodad_funcs VALUES
               (1, 10, 102, 'DoodadFuncCraftPack', NULL),
               (2, 11, 31, 'DoodadFuncCraftPack', 55),
               (3, 12, 837, 'DoodadFuncLootItem', 77),
               (4, 13, 192, 'DoodadFuncQuest', NULL);
             INSERT INTO craft_pack_crafts VALUES (1, 68, 6226), (2, 68, 6227);
             INSERT INTO doodad_func_craft_packs VALUES (102, 68), (31, 68);
             INSERT INTO doodad_func_loot_items VALUES (837, 14400);",
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_benches_and_use_skill() {
        let conn = Connection::open_in_memory().unwrap();
        fixture::create(&conn);
        let d = load(&conn, &Texts::empty()).unwrap();
        let bench = d.templates.get(4233).unwrap();
        assert!(bench.offers_crafts());
        assert_eq!(bench.use_skill, 55);
        assert_eq!(d.bench_pairs, vec![(4233, 6226), (4233, 6227)]);
        let flower = d.templates.get(1598).unwrap();
        assert!(flower.flags.has(DoodadFlags::LOOT) && !flower.offers_crafts());
        assert_eq!(flower.use_skill, 77);
        assert!(d.templates.get(5047).unwrap().flags.has(DoodadFlags::QUEST));
    }

    #[test]
    fn use_skills_come_from_the_func_then_the_fake_use_then_the_use_row() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE doodad_almighties (id INT, name TEXT);
             CREATE TABLE doodad_func_groups (id INT, doodad_almighty_id INT, doodad_func_group_kind_id INT);
             CREATE TABLE doodad_funcs (id INT, doodad_func_group_id INT, actual_func_id INT,
               actual_func_type TEXT, func_skill_id INT);
             CREATE TABLE craft_pack_crafts (id INT, craft_pack_id INT, craft_id INT);
             CREATE TABLE doodad_func_craft_packs (id INT, craft_pack_id INT);
             CREATE TABLE doodad_func_craft_start_crafts (id INT, doodad_func_craft_start_id INT, craft_id INT);
             CREATE TABLE doodad_func_fake_uses (id INT, skill_id INT, fake_skill_id INT);
             CREATE TABLE doodad_func_uses (id INT, skill_id INT);
             INSERT INTO doodad_almighties VALUES (1451, 'a'), (77, 'b'), (88, 'c'), (55, 'd');
             INSERT INTO doodad_func_groups VALUES (1, 1451, 1), (2, 77, 1), (3, 88, 1), (4, 88, 2), (5, 55, 1);
             INSERT INTO doodad_funcs VALUES
               (10, 1, 842, 'DoodadFuncFakeUse', NULL),
               (11, 2, 5, 'DoodadFuncLootItem', 321),
               (12, 3, 6, 'DoodadFuncUse', NULL),
               (13, 4, 9, 'DoodadFuncFakeUse', NULL),
               (14, 5, 1, 'DoodadFuncQuest', NULL);
             INSERT INTO doodad_func_fake_uses VALUES (842, NULL, 14907), (9, NULL, 1);
             INSERT INTO doodad_func_uses VALUES (6, 555);",
        )
        .unwrap();
        let d = load(&conn, &Texts::empty()).unwrap();
        let skill = |id| d.templates.get(id).unwrap().use_skill;
        assert_eq!(skill(1451), 14907, "the fake use's skill");
        assert_eq!(skill(77), 321, "the function's own skill");
        // the start group's Use row wins over the later group's fake use
        assert_eq!(skill(88), 555);
        assert_eq!(skill(55), 0);
    }

    #[test]
    fn item_sources_follow_loot_functions() {
        let conn = Connection::open_in_memory().unwrap();
        fixture::create(&conn);
        let d = load(&conn, &Texts::empty()).unwrap();
        let wanted: FxHashSet<u32> = [14400, 99].into_iter().collect();
        let src = d.item_sources(&conn, &wanted, |_| true).unwrap();
        assert_eq!(src.get(&14400).map(|s| &s[..]), Some(&[1598][..]));
        assert!(!src.contains_key(&99));
        let none = d.item_sources(&conn, &wanted, |_| false).unwrap();
        assert!(none.is_empty());
    }
}
