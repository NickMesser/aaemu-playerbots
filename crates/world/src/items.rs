//! Trade facts about every item template: what a vendor asks and pays, how the item binds, how
//! far it stacks and where the auction house files it.
//!
//! The market features (the auction house, trade-chat sales, selling to players) need to know
//! whether a bot may hand an item to somebody else at all and what it is worth, for 21 000
//! templates. [`ItemFacts`] keeps that in 20 bytes per item, in an [`IdMap`] (sorted ids, binary
//! search, no per-entry overhead): about half a megabyte for the whole 1.2 table, shared by all
//! bots. The English name is not repeated here; it lives in the name table behind
//! [`World::item_name`](crate::World::item_name).
//!
//! Quest items are marked too ([`ItemFacts::quest`]): an item that only exists for a quest is
//! never sold, asked for or traded away, whatever its vendor flags say.
//!
//! Source: the columns of `items` in the server's `compact.sqlite3`, plus the quest tables that
//! name the items a quest hands out.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::Connection;

use crate::geom::IdMap;
use crate::sql::{each_row, table_exists, RowExt};

/// `items.category_id` of the quest items (`item_categories` 64, "Quest").
pub const QUEST_CATEGORY: u32 = 64;

/// `quest_components.component_kind_id` of a quest's reward step (the server's quest component
/// kind `REWARD`, `systems/quests/model.rs`). Rewards are ordinary items the player keeps, so
/// the supply acts of a reward component do not make a quest item.
const REWARD_COMPONENT_KIND: u32 = 8;

/// The quest tables that name quest items, each as the tables it needs and a query for
/// `(item id, kind of the quest component that hands it out)`; the kind is 0 for the acts that
/// have no component. A table a database does not have skips its query (test fixtures carry only
/// `items`).
/// Quest gather targets (`quest_act_obj_item_gathers`, `quest_item_group_items`) and item-use
/// targets are left out on purpose: those are ordinary materials (hay, seeds) that a quest only
/// asks for.
const QUEST_ITEM_SOURCES: [(&[&str], &str); 3] = [
    // items that start a quest
    (
        &["quest_act_con_accept_items"],
        "SELECT item_id, 0 FROM quest_act_con_accept_items",
    ),
    // items a quest step hands out when something is done
    (
        &["quest_act_etc_item_obtains"],
        "SELECT item_id, 0 FROM quest_act_etc_item_obtains",
    ),
    // items a quest step supplies (the reward step's are skipped, see `quest_items_of_quests`)
    (
        &["quest_act_supply_items", "quest_acts", "quest_components"],
        "SELECT s.item_id, qc.component_kind_id FROM quest_act_supply_items s \
         JOIN quest_acts a ON a.act_detail_id = s.id AND a.act_detail_type = 'QuestActSupplyItem' \
         JOIN quest_components qc ON qc.id = a.quest_component_id",
    ),
];

/// What `items.bind_id` means (the server's `ItemBindType`, `data/items.rs`).
pub mod bind {
    /// Not bound: the item can be traded, mailed and auctioned.
    pub const NORMAL: u8 = 1;
    /// Bound when picked up (soul bound in the bag at once).
    pub const ON_PICKUP: u8 = 2;
    /// Bound when equipped.
    pub const ON_EQUIP: u8 = 3;
    /// Bound when unpacked (a trade pack's contents).
    pub const ON_UNPACK: u8 = 4;
    /// Bound when picked up, trade-pack flavour.
    pub const ON_PICKUP_PACK: u8 = 5;
    /// Bound when won at an auction.
    pub const ON_AUCTION_WIN: u8 = 6;
}

/// The trade facts of one item template. 20 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemFacts {
    /// What a vendor asks for one (`items.price`, copper).
    pub price: u32,
    /// What a vendor pays for one of the basic grade (`items.refund`, copper).
    pub refund: u32,
    /// Largest stack (`items.max_stack_size`); 1 for equipment.
    pub max_stack: u32,
    /// `items.bind_id`, see [`bind`].
    pub bind: u8,
    /// `items.level`.
    pub level: u8,
    /// The fixed grade the item always has (`items.fixed_grade`); 0 when it has none (an item
    /// without a fixed grade is graded when it is created).
    pub grade: u8,
    /// The auction house categories `(a, b, c)` (`items.auction_[abc]_category_id`) its searches
    /// filter by; `[0; 3]` when the item has none.
    pub auction_cat: [u8; 3],
    /// `items.sellable`: the vendors take it (the flag the server's store checks).
    pub sellable: bool,
    /// The item exists for a quest: it is in the quest item category, drops only during a quest
    /// (`items.loot_quest_id`) or a quest hands it out (see [`ItemTable::load`]).
    pub quest: bool,
}

impl ItemFacts {
    /// Whether a bot may ask for, sell, buy or hand the item to someone else (a player trade, the
    /// auction house): it is sellable, not bound ([`bind::NORMAL`]) and not a quest item. An item
    /// that binds on pickup, on equip or on unpack is treated as untradable, also while it is
    /// still unbound in the bag: the bot cannot see the item's bound flag.
    pub fn tradable(&self) -> bool {
        self.sellable && self.bind == bind::NORMAL && !self.quest
    }
}

/// The item templates the quest tables hand out (see [`QUEST_ITEM_SOURCES`]), except the supplies
/// of a reward step ([`REWARD_COMPONENT_KIND`]).
fn quest_items_of_quests(conn: &Connection) -> Result<HashSet<u32>> {
    let mut ids = HashSet::new();
    'sources: for (tables, sql) in QUEST_ITEM_SOURCES {
        for table in tables {
            if !table_exists(conn, table)? {
                continue 'sources;
            }
        }
        each_row(conn, sql, |r| {
            if r.u32(1) != REWARD_COMPONENT_KIND {
                ids.insert(r.u32(0));
            }
        })?;
    }
    Ok(ids)
}

/// [`ItemFacts`] of every item template.
#[derive(Debug, Default)]
pub struct ItemTable {
    facts: IdMap<ItemFacts>,
    proficiency: IdMap<(u32, u32)>,
}

impl ItemTable {
    /// Reads the trade columns of `items` and marks the quest items: those in the quest category
    /// ([`QUEST_CATEGORY`]), those that drop only during a quest, and those the quest tables
    /// hand out ([`ItemFacts::quest`]). A database without the quest tables marks the first two
    /// kinds only.
    pub fn load(conn: &Connection) -> Result<ItemTable> {
        let mut rows: Vec<(u32, ItemFacts)> = Vec::with_capacity(22_000);
        each_row(
            conn,
            "SELECT id, price, refund, bind_id, max_stack_size, sellable, level, fixed_grade, \
             auction_a_category_id, auction_b_category_id, auction_c_category_id, \
             category_id, loot_quest_id FROM items",
            |r| {
                rows.push((
                    r.u32(0),
                    ItemFacts {
                        price: r.u32(1),
                        refund: r.u32(2),
                        bind: r.u8(3),
                        max_stack: r.u32(4),
                        sellable: r.flag(5),
                        level: r.u8(6),
                        // (-1: no fixed grade)
                        grade: r.u8(7),
                        auction_cat: [r.u8(8), r.u8(9), r.u8(10)],
                        quest: r.u32(11) == QUEST_CATEGORY || r.u32(12) > 0,
                    },
                ));
            },
        )?;
        let handed_out = quest_items_of_quests(conn)?;
        for (id, facts) in &mut rows {
            facts.quest |= handed_out.contains(id);
        }
        let mut requirements = Vec::new();
        let has_proficiency: bool = conn.query_row(
            "SELECT count(*)=2 FROM pragma_table_info('items')
             WHERE name IN ('actability_group_id','actability_requirement')",
            [],
            |r| r.get(0),
        )?;
        if has_proficiency {
            each_row(
                conn,
                "SELECT id,actability_group_id,actability_requirement FROM items
                 WHERE actability_group_id > 0 AND actability_requirement > 0",
                |r| requirements.push((r.u32(0), (r.u32(1), r.u32(2)))),
            )?;
        }
        Ok(ItemTable {
            facts: IdMap::build(rows),
            proficiency: IdMap::build(requirements),
        })
    }

    /// The facts of item `template`.
    pub fn get(&self, template: u32) -> Option<&ItemFacts> {
        self.facts.get(template)
    }

    /// Minimum points in a vocation required to equip or use an item.
    pub fn proficiency_requirement(&self, template: u32) -> Option<(u32, u32)> {
        self.proficiency.get(template).copied()
    }

    /// Every item as `(template, facts)` in ascending template order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &ItemFacts)> {
        self.facts.iter()
    }

    /// Number of item templates.
    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.facts.shallow_bytes() + self.proficiency.shallow_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with only the `items` table.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INT, price INT, refund INT, bind_id INT, max_stack_size INT, \
               sellable NUM, level INT, fixed_grade INT, auction_a_category_id INT, \
               auction_b_category_id INT, auction_c_category_id INT, category_id INT, \
               loot_quest_id INT);
             INSERT INTO items VALUES (3411, 40, 20, 1, 1000, 't', 1, -1, 7, 24, 36, 20, 0);
             INSERT INTO items VALUES (100, 9, 4, 2, 1, 't', 5, 3, NULL, NULL, NULL, 20, NULL);
             INSERT INTO items VALUES (101, 9, 4, 1, 20, 'f', 5, 0, 1, 2, 3, 20, 0);
             INSERT INTO items VALUES (102, 0, 0, 3, 1, 't', 50, 2, 4, 5, 6, 20, 0);
             -- quest items otherwise sellable and unbound: by category, by quest-only loot
             INSERT INTO items VALUES (103, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 64, 0);
             INSERT INTO items VALUES (104, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 55);",
        )
        .unwrap();
        conn
    }

    /// [`conn`] plus the quest tables and ordinary items named in them.
    fn conn_with_quests() -> Connection {
        let conn = conn();
        conn.execute_batch(
            "INSERT INTO items VALUES (105, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 0);
             INSERT INTO items VALUES (106, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 0);
             INSERT INTO items VALUES (107, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 0);
             INSERT INTO items VALUES (108, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 0);
             INSERT INTO items VALUES (3712, 5, 2, 1, 20, 't', 1, 0, 1, 2, 3, 20, 0);
             CREATE TABLE quest_act_con_accept_items (id INT, item_id INT);
             INSERT INTO quest_act_con_accept_items VALUES (1, 105);
             CREATE TABLE quest_act_etc_item_obtains (id INT, item_id INT);
             INSERT INTO quest_act_etc_item_obtains VALUES (1, 106);
             CREATE TABLE quest_act_supply_items (id INT, item_id INT);
             INSERT INTO quest_act_supply_items VALUES (1, 107), (2, 108);
             CREATE TABLE quest_acts (id INT, quest_component_id INT, act_detail_id INT, \
               act_detail_type TEXT);
             INSERT INTO quest_acts VALUES (1, 10, 1, 'QuestActSupplyItem');
             INSERT INTO quest_acts VALUES (2, 11, 2, 'QuestActSupplyItem');
             -- a supply act id that collides with a supply row but is another act kind
             INSERT INTO quest_acts VALUES (3, 11, 1, 'QuestActObjItemGather');
             CREATE TABLE quest_components (id INT, component_kind_id INT);
             INSERT INTO quest_components VALUES (10, 3), (11, 8);
             -- gather targets are ordinary materials a quest only asks for
             CREATE TABLE quest_act_obj_item_gathers (id INT, item_id INT, count INT);
             INSERT INTO quest_act_obj_item_gathers VALUES (1, 3712, 5);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn the_columns_are_read_leniently() {
        let t = ItemTable::load(&conn()).unwrap();
        assert_eq!(t.len(), 6);
        let ore = t.get(3411).unwrap();
        assert_eq!(
            *ore,
            ItemFacts {
                price: 40,
                refund: 20,
                max_stack: 1000,
                bind: 1,
                level: 1,
                grade: 0,
                auction_cat: [7, 24, 36],
                sellable: true,
                quest: false,
            }
        );
        // NULL categories read as none, a fixed grade is kept
        let bop = t.get(100).unwrap();
        assert_eq!((bop.auction_cat, bop.grade, bop.level), ([0; 3], 3, 5));
        assert!(t.get(999).is_none());
        assert_eq!(t.bytes(), 6 * (4 + std::mem::size_of::<ItemFacts>()));
    }

    #[test]
    fn only_unbound_sellable_items_are_tradable() {
        let t = ItemTable::load(&conn()).unwrap();
        assert!(t.get(3411).unwrap().tradable());
        // binds when picked up
        assert!(!t.get(100).unwrap().tradable());
        // unbound but the vendors do not take it
        assert!(!t.get(101).unwrap().tradable());
        // binds when equipped
        assert!(!t.get(102).unwrap().tradable());
    }

    #[test]
    fn quest_items_are_not_tradable() {
        // without the quest tables the items table alone marks them
        let t = ItemTable::load(&conn()).unwrap();
        for quest_item in [103, 104] {
            let f = t.get(quest_item).unwrap();
            assert!(f.quest, "item {quest_item}");
            assert!(f.sellable && f.bind == bind::NORMAL);
            assert!(!f.tradable(), "item {quest_item}");
        }
        assert!(!t.get(3411).unwrap().quest);
    }

    #[test]
    fn the_quest_tables_name_more_quest_items() {
        let t = ItemTable::load(&conn_with_quests()).unwrap();
        // started a quest, obtained in a quest step, supplied in a quest step
        for quest_item in [103, 104, 105, 106, 107] {
            assert!(t.get(quest_item).unwrap().quest, "item {quest_item}");
            assert!(!t.get(quest_item).unwrap().tradable(), "item {quest_item}");
        }
        // a reward is an ordinary item, and so is a gather target (Hay Bale)
        for plain in [108, 3712, 3411] {
            assert!(!t.get(plain).unwrap().quest, "item {plain}");
            assert!(t.get(plain).unwrap().tradable(), "item {plain}");
        }
    }

    #[test]
    fn an_item_is_twenty_bytes() {
        assert_eq!(std::mem::size_of::<ItemFacts>(), 20);
    }
}
