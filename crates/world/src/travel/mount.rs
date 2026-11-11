//! Mounts and gliders: what a bot needs to summon a rideable mate and to fly.
//!
//! A **mount** is a summon item (`item_summon_mates`: `items.use_skill_id` applies the
//! `SpawnPet` special effect, type 13, and `item_summon_mates.npc_id` is the mate's NPC
//! template). A mate is rideable only when its model declares the driver seat
//! (`model_bindings` owner type `Model`, attach point 1): a battle pet or a cat has no seat and
//! cannot be mounted. The data keeps the cheapest rideable mount per level requirement
//! ([`Mounts::default_mount`]), which is what a GM-bootstrapped bot grants itself when its bag
//! holds none.
//!
//! A **glider** is a back-slot item (`item_backpacks.backpack_type_id = 2`) whose use skill
//! deploys the glider (the 1.2 data gives every one of them `skill_controller_id` 2687; see
//! `systems/skills/glider.rs` of AAEmu-1.2-Rust). [`Mounts::default_glider`] is the default a
//! bot grants itself when its bag holds none.
//!
//! The default item ids the live data resolves to are checked by the ignored real-data test
//! `mounts_of_the_real_data` in `crates/world/tests/travel_real_data.rs` (mount 4177, glider
//! 16392). A server whose data has no such rows simply gets an empty catalog and the bots do
//! not mount or glide.

use anyhow::Result;
use rusqlite::Connection;

use crate::geom::IdMap;
use crate::sql::{each_row, table_exists, RowExt};

/// `item_backpacks.backpack_type_id` of a glider (`data/items.rs` `backpack_type::GLIDER`).
pub const GLIDER_BACKPACK_TYPE: u32 = 2;
/// `model_bindings.attach_point_id` of the driver seat (`AttachPointKind.Driver`).
pub const DRIVER_ATTACH_POINT: u32 = 1;

/// One mount summon item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MountItem {
    /// Item template id (`item_summon_mates.item_id`).
    pub item: u32,
    /// Mate NPC template (`item_summon_mates.npc_id`).
    pub npc: u32,
    /// `items.use_skill_id`: the skill that summons the mate.
    pub use_skill: u32,
    /// `items.level_requirement` the summon item asks of the character.
    pub level_req: u8,
}

/// One glider item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GliderItem {
    /// Item template id (`item_backpacks.item_id` with `backpack_type_id = 2`).
    pub item: u32,
    /// `items.use_skill_id`: the skill that deploys the glider.
    pub use_skill: u32,
    /// `items.level_requirement`.
    pub level_req: u8,
}

/// The mount and glider catalog, loaded once with the [`crate::World`].
#[derive(Debug, Default)]
pub struct Mounts {
    mounts: IdMap<MountItem>,
    gliders: IdMap<GliderItem>,
}

impl Mounts {
    /// Reads the summon items and glider items from `compact.sqlite3`. A database without the
    /// tables (a small fixture) gets an empty catalog.
    pub(crate) fn load(conn: &Connection) -> Result<Mounts> {
        let mut mounts = Vec::new();
        if table_exists(conn, "item_summon_mates")? {
            // A mount needs a driver seat on the mate's model. The server's mount check reads
            // the seat list from `model_bindings`; without one it refuses every `CSMountMate`.
            let seats = driver_models(conn)?;
            each_row(
                conn,
                "SELECT m.item_id, m.npc_id, i.use_skill_id, i.level_requirement, \
                 n.model_id \
                 FROM item_summon_mates m \
                 JOIN items i ON i.id = m.item_id \
                 JOIN npcs n ON n.id = m.npc_id",
                |r| {
                    let item = r.u32(0);
                    let use_skill = r.u32(2);
                    let model = r.u32(4);
                    if use_skill != 0 && seats.contains(&model) {
                        mounts.push((
                            item,
                            MountItem {
                                item,
                                npc: r.u32(1),
                                use_skill,
                                level_req: r.u8(3),
                            },
                        ));
                    }
                },
            )?;
        }
        let mut gliders = Vec::new();
        if table_exists(conn, "item_backpacks")? {
            each_row(
                conn,
                "SELECT b.item_id, i.use_skill_id, i.level_requirement \
                 FROM item_backpacks b JOIN items i ON i.id = b.item_id \
                 WHERE b.backpack_type_id = 2",
                |r| {
                    let use_skill = r.u32(1);
                    if use_skill != 0 {
                        let item = r.u32(0);
                        gliders.push((
                            item,
                            GliderItem {
                                item,
                                use_skill,
                                level_req: r.u8(2),
                            },
                        ));
                    }
                },
            )?;
        }
        Ok(Mounts {
            mounts: IdMap::build(mounts),
            gliders: IdMap::build(gliders),
        })
    }

    /// The mount summon item `item`, if it is a rideable mount.
    pub fn mount(&self, item: u32) -> Option<&MountItem> {
        self.mounts.get(item)
    }

    /// The glider item `item`, if it is one.
    pub fn glider(&self, item: u32) -> Option<&GliderItem> {
        self.gliders.get(item)
    }

    /// Whether `item` is a rideable mount summon item.
    pub fn is_mount(&self, item: u32) -> bool {
        self.mounts.contains(item)
    }

    /// Whether `item` is a glider.
    pub fn is_glider(&self, item: u32) -> bool {
        self.gliders.contains(item)
    }

    /// The default rideable mount of a character of `level`: the lowest item id it may use.
    /// The 1.2 data resolves to 4177 (the Lilyut horse); a fixed price rule would pick a free
    /// event mount instead, which is not what a naturally playing bot would ride.
    pub fn default_mount(&self, level: u8) -> Option<u32> {
        self.mounts
            .values()
            .iter()
            .filter(|m| m.level_req <= level)
            .map(|m| m.item)
            .min()
    }

    /// The default glider of a character of `level`: the lowest item id it may use (the 1.2
    /// data resolves to 16385, Tooloop's old glider).
    pub fn default_glider(&self, level: u8) -> Option<u32> {
        self.gliders
            .values()
            .iter()
            .filter(|g| g.level_req <= level)
            .map(|g| g.item)
            .min()
    }

    /// Every rideable mount item, ascending by item id.
    pub fn iter_mounts(&self) -> impl Iterator<Item = (u32, &MountItem)> {
        self.mounts.iter()
    }

    /// Every glider item, ascending by item id.
    pub fn iter_gliders(&self) -> impl Iterator<Item = (u32, &GliderItem)> {
        self.gliders.iter()
    }

    /// Rideable mount items known.
    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    /// Glider items known.
    pub fn glider_count(&self) -> usize {
        self.gliders.len()
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        self.mounts.shallow_bytes() + self.gliders.shallow_bytes()
    }
}

/// The models that declare a driver seat.
fn driver_models(conn: &Connection) -> Result<std::collections::HashSet<u32>> {
    let mut models = std::collections::HashSet::new();
    if table_exists(conn, "model_bindings")? {
        each_row(
            conn,
            "SELECT DISTINCT owner_id FROM model_bindings \
             WHERE owner_type = 'Model' AND attach_point_id = 1",
            |r| {
                models.insert(r.u32(0));
            },
        )?;
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE items (id INT, use_skill_id INT, level_requirement INT, price INT);
             CREATE TABLE npcs (id INT, model_id INT);
             CREATE TABLE item_summon_mates (id INT, item_id INT, npc_id INT);
             CREATE TABLE item_backpacks (id INT, item_id INT, backpack_type_id INT);
             CREATE TABLE model_bindings (id INT, owner_id INT, owner_type TEXT,
                                          attach_point_id INT, horse_rein TEXT);
             INSERT INTO items VALUES
                 (4177, 10602, 0, 10000),
                 (8158, 10602, 0, 500000),
                 (4941, 11318, 0, 10000),
                 (16392, 12373, 0, 0),
                 (23383, 17406, 10, 30000),
                 (29796, 0, 0, 5000);
             INSERT INTO npcs VALUES (3599, 139), (5431, 450), (4944, 140);
             INSERT INTO item_summon_mates VALUES (1, 4177, 3599), (2, 8158, 5431),
                                                  (3, 4941, 4944);
             INSERT INTO item_backpacks VALUES (1, 16392, 2), (2, 23383, 2),
                                               (3, 29796, 2), (4, 16493, 1);
             INSERT INTO model_bindings VALUES (1, 139, 'Model', 1, 'f'),
                                               (2, 139, 'Model', 2, 'f'),
                                               (3, 450, 'Model', 1, 'f'),
                                               (4, 140, 'Model', 57, 'f');",
        )
        .unwrap();
        c
    }

    #[test]
    fn only_mates_with_a_driver_seat_are_mounts() {
        let m = Mounts::load(&db()).unwrap();
        assert_eq!(m.mount(4177).map(|m| m.npc), Some(3599));
        assert_eq!(m.mount(8158).map(|m| m.npc), Some(5431));
        // the cat's model has no driver point
        assert!(m.mount(4941).is_none());
        assert!(m.is_mount(4177));
        assert!(!m.is_mount(16392));
        assert_eq!(m.mount_count(), 2);
    }

    #[test]
    fn gliders_need_a_use_skill() {
        let m = Mounts::load(&db()).unwrap();
        assert_eq!(m.glider(16392).map(|g| g.use_skill), Some(12373));
        assert_eq!(m.glider(23383).map(|g| g.level_req), Some(10));
        // no use skill: a display or skin item, not a glider a bot can deploy
        assert!(m.glider(29796).is_none());
        assert!(m.is_glider(16392));
        assert_eq!(m.glider_count(), 2);
    }

    #[test]
    fn the_defaults_are_the_lowest_id_items_of_the_level() {
        let m = Mounts::load(&db()).unwrap();
        assert_eq!(m.default_mount(0), Some(4177));
        assert_eq!(m.default_mount(1), Some(4177));
        assert_eq!(m.default_glider(0), Some(16392));
        assert_eq!(m.default_glider(10), Some(16392));
        // a level requirement above the lowest-id glider skips it
        let c = db();
        c.execute("DELETE FROM item_backpacks WHERE item_id = 16392", [])
            .unwrap();
        let m = Mounts::load(&c).unwrap();
        assert_eq!(m.default_glider(0), None);
        assert_eq!(m.default_glider(10), Some(23383));
    }

    #[test]
    fn a_database_without_the_tables_has_no_catalog() {
        let m = Mounts::load(&Connection::open_in_memory().unwrap()).unwrap();
        assert_eq!(m.mount_count(), 0);
        assert_eq!(m.glider_count(), 0);
        assert_eq!(m.default_mount(50), None);
    }
}
