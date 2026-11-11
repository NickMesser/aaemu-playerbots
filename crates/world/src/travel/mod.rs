//! Static travel knowledge for the bots' three travel abilities, loaded once with the [`World`]:
//!
//! * [`portal`]: the Teleport Book / recall / Memory Tome system (recall points, world gates,
//!   respawn points, the stones an open portal costs, the items, skills and NPCs involved);
//! * [`dungeon`] and [`instance`]: instance dungeons (the entrance doodads in `main_world`, the
//!   level / party / entry rules) and the instance worlds they load (spawn point, NPC and doodad
//!   spawns, the exit doodads);
//! * [`boat`]: boat summon scrolls, the slaves they summon, seats and top speed.
//!
//! Everything is read-only after load and is shared by all bots through `Arc<World>`; the
//! structures are flat `Vec`s of small `Copy`-style structs, so hundreds of bots add nothing.
//!
//! The data comes from `compact.sqlite3` (tables named at each loader) and from the server's
//! data files (`Portal/*.json`, `Worlds/<name>/*.json`, `Worlds/world_spawns.json`). Fixed ids
//! the bots rely on (the Teleport Book, its skills, the gate NPCs, ...) are checked against the
//! database at load; a missing one only logs a warning.

pub mod boat;
pub mod dungeon;
pub mod instance;
pub mod mirage;
pub mod mount;
pub mod portal;
pub mod zonegroup;

mod funcs;

use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::FxHashMap;

pub use boat::{
    BoatScroll, Boats, ChildSlave, HullSize, ShipClass, ShipFamily, ShipModelData, SlaveTemplate,
};
pub use dungeon::{Dungeon, Dungeons, Entrance, DEFAULT_ENTER_COUNT, ENTRY_WINDOW_SECS};
pub use instance::{
    InstanceDoodad, InstanceExit, InstanceNpc, InstanceSpawn, InstanceWorld, InstanceWorlds,
};
pub use mirage::Mirage;
pub use mount::{GliderItem, MountItem, Mounts};
pub use portal::{
    MemoryTome, PortalKit, PortalReagent, Portals, RecallPoint, RespawnPoint, WorldGate,
};
pub use zonegroup::ZoneGroups;

use crate::doodad::DoodadData;
use crate::geom::{IdMap, StrTable, Vec3};
use crate::npc::NpcTemplate;
use crate::skill::Skills;
use crate::spatial::SpawnIndex;
use crate::sql::{each_row, RowExt};
use crate::text::Texts;
use crate::World;

/// A skill's timing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SkillTiming {
    /// `skills.id`.
    pub id: u32,
    /// Cast time in milliseconds.
    pub cast_ms: u32,
    /// Cooldown in milliseconds.
    pub cooldown_ms: u32,
}

impl SkillTiming {
    /// The timing of skill `id` from the loaded skills; the id alone (with a warning naming
    /// `what`) when the skill is not in `compact.skills`.
    pub(crate) fn of(skills: &Skills, id: u32, what: &str) -> SkillTiming {
        match skills.get(id) {
            Some(s) => SkillTiming {
                id,
                cast_ms: s.casting_time,
                cooldown_ms: s.cooldown_time,
            },
            None => {
                tracing::warn!("travel: {what} skill {id} is not in compact.skills");
                SkillTiming {
                    id,
                    ..SkillTiming::default()
                }
            }
        }
    }
}

/// What the travel loaders read from the already loaded parts of the world.
pub(crate) struct TravelInputs<'a> {
    pub conn: &'a Connection,
    pub texts: &'a Texts,
    /// The server's `app/Data` directory.
    pub server_data: &'a Path,
    /// Name of the main world directory (`"main_world"`); not an instance world.
    pub main_world: &'a str,
    pub npcs: &'a IdMap<NpcTemplate>,
    pub doodads: &'a DoodadData,
    pub skills: &'a Skills,
    /// Doodad spawns of the main world.
    pub doodad_spawns: &'a SpawnIndex,
    /// Item ids with English names.
    pub items: &'a StrTable,
}

/// All travel data.
#[derive(Debug, Default)]
pub struct Travel {
    /// Recall points, world gates, respawn points, open-portal reagents, Memory Tomes.
    pub portals: Portals,
    /// Instance dungeons and the instance worlds.
    pub dungeons: Dungeons,
    /// Boat scrolls and slave templates.
    pub boats: Boats,
    /// Mount summon items and gliders.
    pub mounts: Mounts,
    /// Mirage Isle, the market island (`None` when the data has no such portal).
    pub mirage: Option<Mirage>,
    /// Zone key to zone group: who hears the trade chat.
    pub zone_groups: ZoneGroups,
}

impl Travel {
    pub(crate) fn load(inp: &TravelInputs<'_>) -> Result<Travel> {
        let started = std::time::Instant::now();
        let funcs = funcs::DoodadTravelFuncs::load(inp.conn)?;
        let portals = Portals::load(inp, &funcs)?;
        let worlds = InstanceWorlds::load(inp, &funcs)?;
        let dungeons = Dungeons::load(inp, &funcs, worlds)?;
        let boats = Boats::load(inp)?;
        let mounts = Mounts::load(inp.conn)?;
        let mirage = Mirage::load(inp, &funcs, dungeons.worlds());
        let travel = Travel {
            portals,
            dungeons,
            boats,
            mounts,
            mirage,
            zone_groups: ZoneGroups::load(inp.conn),
        };
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            recall_points = travel.portals.recall_points().len(),
            world_gates = travel.portals.world_gates().len(),
            memory_tomes = travel.portals.memory_tomes().len(),
            dungeons = travel.dungeons.dungeons().len(),
            instance_worlds = travel.dungeons.worlds().all().len(),
            boat_scrolls = travel.boats.scrolls().len(),
            mounts = travel.mounts.mount_count(),
            gliders = travel.mounts.glider_count(),
            memory_kib = travel.bytes() / 1024,
            "travel data loaded"
        );
        Ok(travel)
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        self.portals.bytes()
            + self.dungeons.bytes()
            + self.boats.bytes()
            + self.mounts.bytes()
            + self.mirage.as_ref().map_or(0, Mirage::bytes)
            + self.zone_groups.bytes()
    }

    /// Heap bytes per part, for the memory report.
    pub fn memory_breakdown(&self) -> [(&'static str, usize); 6] {
        [
            ("travel: portals", self.portals.bytes()),
            ("travel: dungeons + instance worlds", self.dungeons.bytes()),
            ("travel: boats", self.boats.bytes()),
            ("travel: mounts + gliders", self.mounts.bytes()),
            (
                "travel: Mirage Isle",
                self.mirage.as_ref().map_or(0, Mirage::bytes),
            ),
            ("travel: zone groups", self.zone_groups.bytes()),
        ]
    }
}

// ---- Delegating accessors on the World --------------------------------------------------

impl World {
    // Portals

    /// Recall points (`Portal/recalls.json`): the places a Teleport Book entry can name, in
    /// file order.
    pub fn recall_points(&self) -> &[RecallPoint] {
        self.travel().portals.recall_points()
    }

    /// The recall point nearest to `pos` (horizontal distance).
    pub fn nearest_recall(&self, pos: Vec3) -> Option<&RecallPoint> {
        self.travel().portals.nearest_recall(pos)
    }

    /// The stones an open portal consumes: same-continent (`true`) or cross-continent (`false`)
    /// reagents, the server takes the first the character can afford.
    pub fn portal_reagents(&self, same_continent: bool) -> &[PortalReagent] {
        self.travel().portals.reagents(same_continent)
    }

    // Instance dungeons

    /// Instance dungeons whose entrance doodad spawns in the main world, ordered by zone group.
    pub fn dungeons(&self) -> &[Dungeon] {
        self.travel().dungeons.dungeons()
    }

    /// The dungeon an entrance doodad template leads into (the first when the doodad has several
    /// entries; see [`Dungeons::by_entrance_doodad`]).
    pub fn dungeon_by_entrance_doodad(&self, template: u32) -> Option<&Dungeon> {
        self.travel().dungeons.dungeon_by_entrance_doodad(template)
    }

    /// All dungeons an entrance doodad template leads into, lowest zone group first.
    pub fn dungeons_by_entrance_doodad(&self, template: u32) -> impl Iterator<Item = &Dungeon> {
        self.travel().dungeons.by_entrance_doodad(template)
    }

    /// Whether a doodad template is a dungeon door: an entrance doodad in the main world or an
    /// exit doodad of an instance world. Cheap (two binary searches).
    pub fn is_dungeon_door(&self, template: u32) -> bool {
        self.travel().dungeons.is_door(template)
    }

    /// The instance world a dungeon loads.
    pub fn dungeon_world(&self, dungeon: &Dungeon) -> Option<&InstanceWorld> {
        self.travel().dungeons.world_of(dungeon)
    }

    /// The instance world that the `zone_key` of an `SCLoadInstance` belongs to.
    pub fn instance_world_by_zone_key(&self, zone_key: u16) -> Option<&InstanceWorld> {
        self.travel().dungeons.world_by_zone_key(zone_key)
    }

    /// An instance world by its directory name (`instance_cuttingwind_deadmine`).
    pub fn instance_world(&self, name: &str) -> Option<&InstanceWorld> {
        self.travel().dungeons.world(name)
    }

    // Mirage Isle

    /// Mirage Isle, the market island: its portal doodad in the towns, the arrival point and the
    /// exit doodad (`None` when the server data has no such portal).
    pub fn mirage(&self) -> Option<&Mirage> {
        self.travel().mirage.as_ref()
    }

    /// Whether a doodad template is the island's portal (main world) or its exit door. Bots keep
    /// these in view although they offer nothing else to use.
    pub fn is_mirage_door(&self, template: u32) -> bool {
        self.mirage().is_some_and(|m| {
            m.portal.doodad == template || m.exit.is_some_and(|(t, _, _)| t == template)
        })
    }

    /// The zone group (`zones.group_id`) at (x, y): whose trade, shout and group-finding chat the
    /// character there reads. `None` where the baked zone grid has no zone.
    pub fn zone_group_at(&self, x: f32, y: f32) -> Option<u32> {
        self.travel().zone_groups.of_key(self.zone_key_at(x, y)?)
    }

    // Boats

    /// Boat summon scrolls, cheapest first.
    pub fn boat_scrolls(&self) -> &[BoatScroll] {
        self.travel().boats.scrolls()
    }

    /// A slave template known to the boat data (the slaves boat scrolls summon).
    pub fn slave_template(&self, id: u32) -> Option<&SlaveTemplate> {
        self.travel().boats.slave_template(id)
    }

    /// The boat a scroll item summons.
    pub fn boat_for_scroll(&self, item: u32) -> Option<&SlaveTemplate> {
        self.travel().boats.boat_for_scroll(item)
    }

    /// Mount summon items and gliders.
    pub fn mounts(&self) -> &Mounts {
        &self.travel().mounts
    }
}

// ---- Shared helpers ---------------------------------------------------------------------

/// English texts of one `localized_texts` column (`idx -> en_us`, empty ones left out).
pub(crate) fn localized(conn: &Connection, table: &str, column: &str) -> FxHashMap<u32, Box<str>> {
    let mut out = FxHashMap::default();
    // `table` / `column` are literals of the loaders, never user input.
    let sql = format!(
        "SELECT idx, en_us FROM localized_texts WHERE tbl_name = '{table}' \
         AND tbl_column_name = '{column}' AND en_us IS NOT NULL AND en_us <> ''"
    );
    // A compact without `localized_texts` just leaves everything unlocalized.
    let _ = each_row(conn, &sql, |r| {
        let text = r.text(1).trim();
        if !text.is_empty() {
            out.insert(r.u32(0), text.into());
        }
    });
    out
}

/// Ids of `ids` that do not exist in `table.id`; logs a warning naming `what` for each.
pub(crate) fn warn_missing(conn: &Connection, table: &str, what: &str, ids: &[u32]) -> Vec<u32> {
    let list = ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let mut found = Vec::new();
    // `table` is a literal of the loaders.
    let read = each_row(
        conn,
        &format!("SELECT id FROM {table} WHERE id IN ({list})"),
        |r| found.push(r.u32(0)),
    );
    if let Err(e) = read {
        tracing::warn!("travel: cannot check {what} ids in {table}: {e:#}");
        return Vec::new();
    }
    let missing: Vec<u32> = ids.iter().copied().filter(|i| !found.contains(i)).collect();
    for id in &missing {
        tracing::warn!("travel: {what} {id} is not in compact.{table}");
    }
    missing
}

/// Case-insensitive member lookup in a JSON object (Newtonsoft semantics of the server files).
pub(crate) fn member<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    obj.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// A numeric member as `f32` (0 when missing or not a number).
pub(crate) fn member_f32(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> f32 {
    member(obj, key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0) as f32
}

/// A numeric member as `u32` (0 when missing, negative or not a number).
pub(crate) fn member_u32(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> u32 {
    member(obj, key)
        .and_then(serde_json::Value::as_f64)
        .map_or(0, |v| v.clamp(0.0, f64::from(u32::MAX)) as u32)
}

/// A string member (empty when missing or not a string).
pub(crate) fn member_str<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> &'a str {
    member(obj, key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

/// Reads a server JSON(C) file holding an array of objects. A missing file is an empty list
/// (logged); an unreadable or malformed one is an error.
pub(crate) fn read_object_array(
    path: &Path,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    use anyhow::Context;
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("travel: {} not found", path.display());
            return Ok(Vec::new());
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let values: Vec<serde_json::Value> =
        crate::jsonc::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(values
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        })
        .collect())
}

/// Heap bytes of a `Box<str>` list.
pub(crate) fn str_bytes<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    names.map(str::len).sum()
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;

    /// A fresh, unique temp directory (removed by the caller).
    pub fn temp_dir(tag: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("aa-world-travel-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub fn write(dir: &std::path::Path, name: &str, text: &str) {
        if let Some(parent) = dir.join(name).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(dir.join(name), text).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{temp_dir, write};
    use super::*;

    #[test]
    fn object_arrays_accept_jsonc_and_report_bad_files() {
        let dir = temp_dir("objects");
        write(
            &dir,
            "ok.json",
            "\u{feff}[ // comment\n { \"Id\": 7, \"name\": \"x\", \"X\": 1.5, }, 5, [1], ]",
        );
        write(&dir, "bad.json", "[ { \"Id\": ");
        write(&dir, "empty.json", "  \n");
        let ok = read_object_array(&dir.join("ok.json")).unwrap();
        // non-objects are skipped; member names match case-insensitively
        assert_eq!(ok.len(), 1);
        assert_eq!(member_u32(&ok[0], "id"), 7);
        assert_eq!(member_str(&ok[0], "NAME"), "x");
        assert_eq!(member_f32(&ok[0], "x"), 1.5);
        assert_eq!(member_f32(&ok[0], "missing"), 0.0);
        let err = read_object_array(&dir.join("bad.json")).unwrap_err();
        assert!(format!("{err:#}").contains("bad.json"), "{err:#}");
        assert!(read_object_array(&dir.join("empty.json"))
            .unwrap()
            .is_empty());
        assert!(read_object_array(&dir.join("absent.json"))
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_ids_are_reported_and_texts_are_read() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INT);
             INSERT INTO items VALUES (1), (3);
             CREATE TABLE localized_texts (id INT, tbl_name TEXT, tbl_column_name TEXT, idx INT, en_us TEXT);
             INSERT INTO localized_texts VALUES (1, 'slaves', 'name', 15, ' Rowboat '), (2, 'slaves', 'name', 16, ''),
               (3, 'slaves', 'description', 15, 'ignored');",
        )
        .unwrap();
        assert_eq!(
            warn_missing(&conn, "items", "test item", &[1, 2, 3, 4]),
            vec![2, 4]
        );
        // an unreadable table checks nothing instead of failing the load
        assert!(warn_missing(&conn, "nonexistent", "test item", &[1]).is_empty());
        let names = localized(&conn, "slaves", "name");
        assert_eq!(names.len(), 1);
        assert_eq!(names.get(&15).map(|n| &**n), Some("Rowboat"));
        assert!(localized(&Connection::open_in_memory().unwrap(), "slaves", "name").is_empty());
    }
}
