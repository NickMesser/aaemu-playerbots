//! The [`World`]: everything static the bots need to know, loaded once and shared read-only
//! (`Arc<World>`).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rustc_hash::FxHashMap;

use crate::auctioneers::Auctioneers;
use crate::baked::{Baked, QuestSphere};
use crate::chars::Chars;
use crate::doodad::{DoodadData, DoodadTemplate};
use crate::economy::{Craft, Economy};
use crate::event::Events;
use crate::faction::{Factions, Relation};
use crate::farm::Farming;
use crate::gear::GearData;
use crate::geom::{IdMap, StrTable, Vec3};
use crate::housing::{AreaIndex, Housing, HousingDesign};
use crate::items::{ItemFacts, ItemTable};
use crate::npc::{self, NpcTemplate};
use crate::quest::{Quest, QuestStats, Quests};
use crate::quest_atlas::QuestAtlas;
use crate::reqs::StartGates;
use crate::skill::{SkillInfo, Skills};
use crate::spatial::SpawnIndex;
use crate::spawns::{self, Kind, SpawnStats};
use crate::sql::{each_row, open_readonly, RowExt};
use crate::text::{TextTable, Texts};
use crate::travel::{Travel, TravelInputs};
use crate::uses::{ItemUses, SkillNeed};
use crate::water::{WaterMap, NAV_FLOOR_M};

/// Where the world data lives.
#[derive(Clone, Debug)]
pub struct WorldPaths {
    /// The server's `compact.sqlite3` (opened read-only).
    pub compact: PathBuf,
    /// The server's `app/Data` directory (`Worlds/<world>/*.json`, `CharTemplates.json`).
    pub server_data: PathBuf,
    /// Directory written by `playerbots bake` (terrain, map, housing areas, quest spheres,
    /// zones); `None` runs without baked data.
    pub baked: Option<PathBuf>,
    /// World name, e.g. `"main_world"`.
    pub world: String,
}

/// Zone names and the zone id / zone key mapping.
#[derive(Debug, Default)]
struct Zones {
    /// `(zone_key, English name)` sorted by key.
    names: Vec<(u16, Box<str>)>,
    /// `zones.id -> zone_key`.
    key_of_id: IdMap<u16>,
    /// `(zone_key, zone group, faction)` sorted by key: the zone group (what the conflict state
    /// packets name) and the faction that owns the zone (0 = nobody: a conflict zone, the sea).
    info: Vec<(u16, u16, u32)>,
    /// Sorted zone groups that take part in the zone conflicts (`conflict_zones`, not closed).
    conflict_groups: Vec<u16>,
    /// Sorted zone keys whose `zones.closed` is set (Aubre Cradle, Airain Rock, Rainbow Field 1,
    /// ...): no ordinary player may be there, so no bot plans a quest there.
    closed: Vec<u16>,
}

/// What the static data says about a zone (`zones` row).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneInfo {
    /// The zone group (the id of `SCConflictZoneStatePacket`).
    pub group: u16,
    /// The faction that owns the zone; 0 when nobody does. A character of the alliance whose
    /// mother faction this is cannot be attacked in the zone (the server's home-zone rule).
    pub faction: u32,
}

impl Zones {
    fn load(conn: &Connection, texts: &Texts) -> Result<Zones> {
        let mut names: Vec<(u16, Box<str>)> = Vec::new();
        let mut ids = Vec::new();
        let mut info: Vec<(u16, u16, u32)> = Vec::new();
        let mut closed: Vec<u16> = Vec::new();
        each_row(
            conn,
            "SELECT id, name, zone_key, group_id, faction_id, closed FROM zones",
            |r| {
                let (id, key) = (r.u32(0), r.u16(2));
                info.push((key, r.u16(3), r.u32(4)));
                if r.flag(5) {
                    closed.push(key);
                }
                let name: Box<str> = texts
                    .get(TextTable::Zone, id)
                    .or_else(|| texts.get(TextTable::ZoneGroup, r.u32(3)))
                    .unwrap_or_else(|| r.text(1))
                    .into();
                ids.push((id, key));
                if !names.iter().any(|n| n.0 == key) {
                    names.push((key, name));
                }
            },
        )?;
        names.sort_by_key(|n| n.0);
        info.sort_by_key(|i| i.0);
        info.dedup_by_key(|i| i.0);
        closed.sort_unstable();
        closed.dedup();
        // (an old database without the table has no conflict zones)
        let mut conflict_groups: Vec<u16> = Vec::new();
        let _ = each_row(
            conn,
            "SELECT zone_group_id FROM conflict_zones WHERE closed IN ('f', '0') OR closed = 0",
            |r| conflict_groups.push(r.u16(0)),
        );
        conflict_groups.sort_unstable();
        conflict_groups.dedup();
        Ok(Zones {
            names,
            key_of_id: IdMap::build(ids),
            info,
            conflict_groups,
            closed,
        })
    }

    fn info(&self, key: u16) -> Option<ZoneInfo> {
        let i = self.info.binary_search_by_key(&key, |i| i.0).ok()?;
        let (_, group, faction) = self.info[i];
        Some(ZoneInfo { group, faction })
    }

    fn name(&self, key: u16) -> Option<&str> {
        self.names
            .binary_search_by_key(&key, |n| n.0)
            .ok()
            .map(|i| &*self.names[i].1)
    }

    fn closed(&self, key: u16) -> bool {
        self.closed.binary_search(&key).is_ok()
    }

    fn bytes(&self) -> usize {
        self.names.capacity() * std::mem::size_of::<(u16, Box<str>)>()
            + self.names.iter().map(|n| n.1.len()).sum::<usize>()
            + self.key_of_id.shallow_bytes()
            + self.info.capacity() * std::mem::size_of::<(u16, u16, u32)>()
            + self.conflict_groups.capacity() * 2
            + self.closed.capacity() * 2
    }
}

/// Static world knowledge, immutable after [`World::load`].
pub struct World {
    baked: Baked,
    factions: Factions,
    chars: Chars,
    skills: Skills,
    npcs: IdMap<NpcTemplate>,
    doodads: DoodadData,
    npc_spawns: SpawnIndex,
    doodad_spawns: SpawnIndex,
    /// Where the auction house NPCs stand.
    pub(crate) auctioneers: Auctioneers,
    npc_spawn_stats: SpawnStats,
    doodad_spawn_stats: SpawnStats,
    quests: Quests,
    start_gates: StartGates,
    economy: Economy,
    housing: Housing,
    areas: AreaIndex,
    items: StrTable,
    /// Trade facts (price, binding, stacking, auction categories, quest item) of every item
    /// template.
    item_facts: ItemTable,
    item_uses: ItemUses,
    zones: Zones,
    /// Doodad templates (that spawn) yielding a gather item.
    gather_doodads: FxHashMap<u32, Box<[u32]>>,
    /// `(quest, component) -> index into baked.quest_spheres`.
    spheres: FxHashMap<(u32, u32), u32>,
    /// Portals, instance dungeons and boats (the travel abilities' static data).
    travel: Travel,
    /// Tower defence events ("rifts"): their places, levels and kill targets.
    events: Events,
    /// Crops, seed vendors and the public farms.
    farm: Farming,
    /// Equippable items, lunagems, sockets, gear merchants (the gear feature).
    gear: GearData,
    /// The sea mask boats are planned on, built from the terrain at the first [`World::water`].
    water: OnceLock<Option<WaterMap>>,
    /// The same for the large hulls (4.5 m of water), built at the first [`World::water_deep`].
    water_deep: OnceLock<Option<WaterMap>>,
    /// Where the quests are, built at the first [`World::quest_atlas`].
    quest_atlas: OnceLock<QuestAtlas>,
    /// Spots inside each conflict zone group, built at the first [`World::zone_group_spots`].
    group_spots: OnceLock<FxHashMap<u16, Box<[[f32; 2]]>>>,
    /// Places worth seeing, built at the first [`World::landmarks`].
    pub(crate) landmarks: OnceLock<crate::landmarks::Landmarks>,
}

// Bots share the world through `Arc<World>` across tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<World>();
};

impl World {
    /// Loads the world: opens `compact.sqlite3` read-only, reads the spawn files and the baked
    /// extras, compiles quests and builds the indexes. Logs counts and the load time.
    ///
    /// Blocking (about half a second with the full 1.2 data); call it from `spawn_blocking` inside
    /// an async runtime.
    pub fn load(paths: &WorldPaths) -> Result<World> {
        let started = Instant::now();
        let conn = open_readonly(&paths.compact)?;
        let texts = Texts::load(&conn).context("read localized_texts")?;

        let baked = match &paths.baked {
            Some(dir) => Baked::load(dir, &paths.world),
            None => Baked::default(),
        };

        let factions = Factions::load(&conn).context("load factions")?;
        let chars = Chars::load(&conn, &paths.server_data).context("load character templates")?;
        let skills = Skills::load(&conn, &texts).context("load skills")?;
        let mut npcs = npc::load(&conn, &texts).context("load npcs")?;
        let doodads = crate::doodad::load(&conn, &texts).context("load doodads")?;
        let items = load_items(&conn, &texts).context("load items")?;
        let item_uses = ItemUses::load(&conn).context("load item use skills")?;
        let item_facts = ItemTable::load(&conn).context("load item trade facts")?;

        // Spawn files.
        let world_dir: PathBuf = paths.server_data.join("Worlds").join(&paths.world);
        let ground = |x: f32, y: f32| baked.terrain.as_ref().and_then(|t| t.height(x, y));
        let (npc_raw, npc_spawn_stats) =
            spawns::load(&world_dir, Kind::Npc, |t| npcs.contains(t), ground)
                .context("load npc spawns")?;
        let (doodad_raw, doodad_spawn_stats) = spawns::load(
            &world_dir,
            Kind::Doodad,
            |t| doodads.templates.contains(t),
            ground,
        )
        .context("load doodad spawns")?;
        let npc_spawns = SpawnIndex::build(npc_raw);
        let auctioneers = Auctioneers::build(&npcs, &npc_spawns);
        let doodad_spawns = SpawnIndex::build(doodad_raw);

        let quests =
            Quests::load(&conn, &texts, &|n| npc_spawns.has(n)).context("compile quests")?;
        for npc_id in quests.npc_starters() {
            if let Some(n) = npcs.get_mut(npc_id) {
                n.is_quest_giver = true;
            }
        }
        let start_gates = StartGates::load(&conn).context("load quest start requirements")?;
        let gather_doodads = doodads
            .item_sources(&conn, &quests.gather_items(), |d| doodad_spawns.has(d))
            .context("resolve gather item sources")?;
        let economy =
            Economy::load(&conn, &texts, &skills, &doodads.bench_pairs).context("load crafts")?;
        let housing =
            Housing::load(&conn, &texts, &|i| items.get(i).is_some()).context("load housing")?;
        let areas = AreaIndex::build(&baked.housing_areas);
        let zones = Zones::load(&conn, &texts).context("load zones")?;
        let spheres = baked
            .quest_spheres
            .iter()
            .enumerate()
            .map(|(i, s)| ((s.quest, s.component), i as u32))
            .collect();

        let travel = Travel::load(&TravelInputs {
            conn: &conn,
            texts: &texts,
            server_data: &paths.server_data,
            main_world: &paths.world,
            npcs: &npcs,
            doodads: &doodads,
            skills: &skills,
            doodad_spawns: &doodad_spawns,
            items: &items,
        })
        .context("load travel data")?;
        let events = {
            let zone_group = |x: f32, y: f32| {
                baked
                    .zones
                    .as_ref()
                    .and_then(|z| z.zone_key_at(x, y))
                    .and_then(|k| travel.zone_groups.of_key(k))
                    .map_or(0, |g| u16::try_from(g).unwrap_or(0))
            };
            Events::load(&conn, &npcs, &npc_spawns, &zone_group).context("load events")?
        };

        let farm = Farming::load(&conn, &npcs, &|n| npc_spawns.has(n))
            .context("load farming data")?
            .with_areas(&baked.farm_areas);
        let gear =
            GearData::load(&conn, &npcs, &|n| npc_spawns.has(n)).context("load gear data")?;

        let world = World {
            baked,
            factions,
            chars,
            skills,
            npcs,
            doodads,
            npc_spawns,
            doodad_spawns,
            auctioneers,
            npc_spawn_stats,
            doodad_spawn_stats,
            quests,
            start_gates,
            economy,
            housing,
            areas,
            items,
            item_facts,
            item_uses,
            zones,
            gather_doodads,
            spheres,
            travel,
            events,
            farm,
            gear,
            water: OnceLock::new(),
            water_deep: OnceLock::new(),
            quest_atlas: OnceLock::new(),
            group_spots: OnceLock::new(),
            landmarks: OnceLock::new(),
        };
        let q = world.quests.stats();
        tracing::info!(
            world = %paths.world,
            elapsed_ms = started.elapsed().as_millis() as u64,
            npcs = world.npcs.len(),
            doodads = world.doodads.templates.len(),
            npc_spawns = world.npc_spawns.len(),
            npc_spawn_ids = world.npc_spawns.template_count(),
            doodad_spawns = world.doodad_spawns.len(),
            quests = q.quests,
            quests_with_objectives = q.with_objectives,
            skills = world.skills.len(),
            crafts = world.economy.craft_count(),
            designs = world.housing.designs().len(),
            items = world.items.len(),
            item_facts = world.item_facts.len(),
            item_use_skills = world.item_uses.len(),
            gated_quests = world.start_gates.len(),
            recall_points = world.travel.portals.recall_points().len(),
            dungeons = world.travel.dungeons.dungeons().len(),
            instance_worlds = world.travel.dungeons.worlds().all().len(),
            boat_scrolls = world.travel.boats.scrolls().len(),
            events = world.events.all().len(),
            crops = world.farm.crops().len(),
            farm_areas = world.farm.areas().len(),
            factions = world.factions.len(),
            terrain = world.baked.terrain.is_some(),
            memory_kib = world.memory_estimate() / 1024,
            "world loaded"
        );
        Ok(world)
    }

    /// Approximate heap bytes held (memory-mapped terrain is not counted).
    pub fn memory_estimate(&self) -> usize {
        self.memory_breakdown().iter().map(|(_, n)| n).sum()
    }

    /// Heap bytes per component, largest first.
    pub fn memory_breakdown(&self) -> Vec<(&'static str, usize)> {
        let baked = self
            .baked
            .housing_areas
            .iter()
            .map(|p| p.points.capacity() * 8 + std::mem::size_of_val(p))
            .sum::<usize>()
            + self.baked.quest_spheres.capacity() * std::mem::size_of::<QuestSphere>()
            + self
                .baked
                .zones
                .as_ref()
                .map_or(0, |z| z.keys.capacity() * 2);
        let gather = self
            .gather_doodads
            .values()
            .map(|v| v.len() * 4 + 24)
            .sum::<usize>();
        let mut v = vec![
            ("npc spawns + grid", self.npc_spawns.bytes()),
            ("doodad spawns + grid", self.doodad_spawns.bytes()),
            ("npc templates", npc::bytes(&self.npcs)),
            ("doodad templates", self.doodads.bytes()),
            ("quests", self.quests.bytes()),
            ("skills", self.skills.bytes()),
            ("item names", self.items.bytes()),
            ("item trade facts", self.item_facts.bytes()),
            ("auctioneers", self.auctioneers.bytes()),
            ("item use skills", self.item_uses.bytes()),
            ("gear", self.gear.bytes()),
            ("quest start gates", self.start_gates.bytes()),
            ("crafts + specialty", self.economy.bytes()),
            ("housing", self.housing.bytes() + self.areas.bytes()),
            ("factions", self.factions.bytes()),
            ("characters", self.chars.bytes()),
            ("zones", self.zones.bytes()),
            ("baked (areas, spheres, zones)", baked),
            ("indexes", gather + self.spheres.capacity() * 16),
        ];
        v.extend(self.travel.memory_breakdown());
        v.push(("events", self.events.bytes()));
        v.push(("farming", self.farm.bytes()));
        v.push((
            "water map",
            self.water
                .get()
                .and_then(Option::as_ref)
                .map_or(0, WaterMap::bytes)
                + self
                    .water_deep
                    .get()
                    .and_then(Option::as_ref)
                    .map_or(0, WaterMap::bytes),
        ));
        v.push((
            "quest atlas",
            self.quest_atlas.get().map_or(0, QuestAtlas::bytes),
        ));
        v.sort_by_key(|e| std::cmp::Reverse(e.1));
        v
    }

    /// The baked extras (terrain, map calibration, housing areas, quest spheres, zones).
    pub fn baked(&self) -> &Baked {
        &self.baked
    }

    /// Portals, instance dungeons and boats; see [`crate::travel`].
    pub fn travel(&self) -> &Travel {
        &self.travel
    }

    /// Tower defence events ("rifts"); see [`crate::event`].
    pub fn events(&self) -> &Events {
        &self.events
    }

    /// Crops, seed vendors and the public farms; see [`crate::farm`].
    pub fn farm(&self) -> &Farming {
        &self.farm
    }

    /// Equippable items, lunagems, sockets and gear merchants; see [`crate::gear`].
    pub fn gear(&self) -> &GearData {
        &self.gear
    }

    /// The sea map boats are planned on, built from the baked terrain the first time it is asked
    /// for (a tenth of a second on four threads, 0.7 MB; see [`crate::water`]); `None` without
    /// baked terrain.
    pub fn water(&self) -> Option<&WaterMap> {
        self.water
            .get_or_init(|| {
                let started = Instant::now();
                let map = WaterMap::from_terrain(self.baked.terrain.as_ref()?, NAV_FLOOR_M);
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    cells = u64::from(map.cols()) * u64::from(map.rows()),
                    sea_cells = map.sea_count(),
                    kib = map.bytes() / 1024,
                    "water map built"
                );
                Some(map)
            })
            .as_ref()
    }

    /// The sea mask for the large hulls ([`crate::travel::boat::DEEP_FLOOR_M`]: cells with 4.5 m of
    /// water), built at the first call like [`World::water`].
    pub fn water_deep(&self) -> Option<&WaterMap> {
        self.water_deep
            .get_or_init(|| {
                let started = Instant::now();
                let map = WaterMap::from_terrain(
                    self.baked.terrain.as_ref()?,
                    crate::travel::boat::DEEP_FLOOR_M,
                );
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    sea_cells = map.sea_count(),
                    kib = map.bytes() / 1024,
                    "deep water map built"
                );
                Some(map)
            })
            .as_ref()
    }

    /// The sea mask a hull of `class` is planned on: [`World::water`] for the small and medium
    /// hulls, [`World::water_deep`] for the large.
    pub fn water_for(&self, class: &crate::travel::ShipClass) -> Option<&WaterMap> {
        if class.sea_floor_m() < NAV_FLOOR_M {
            self.water_deep()
        } else {
            self.water()
        }
    }

    /// Terrain height at (x, y) from the baked grid; `None` without baked terrain or outside it.
    pub fn ground_z(&self, x: f32, y: f32) -> Option<f32> {
        self.baked.terrain.as_ref()?.height(x, y)
    }

    // ---- NPCs ----------------------------------------------------------------------------

    /// An NPC template.
    pub fn npc(&self, template: u32) -> Option<&NpcTemplate> {
        self.npcs.get(template)
    }

    /// All NPC templates in id order.
    pub fn npc_templates(&self) -> &[NpcTemplate] {
        self.npcs.values()
    }

    /// Static spawn positions of an NPC template.
    pub fn npc_spawns(&self, template: u32) -> &[Vec3] {
        self.npc_spawns.of(template)
    }

    /// The spawn of `template` horizontally closest to `from`.
    pub fn nearest_npc_spawn(&self, template: u32, from: Vec3) -> Option<Vec3> {
        self.npc_spawns.nearest(template, from)
    }

    /// Calls `f(template, position)` for every NPC spawn within `radius` metres (horizontal).
    pub fn npc_spawns_within(&self, center: Vec3, radius: f32, f: impl FnMut(u32, Vec3)) {
        self.npc_spawns.within(center, radius, f)
    }

    /// Whether the NPC template spawns anywhere in this world.
    pub fn npc_spawns_anywhere(&self, template: u32) -> bool {
        self.npc_spawns.has(template)
    }

    /// Whether a spawn of `template` is on the navigation mesh and connected to the mesh node
    /// under `from`. Without a baked mesh every spawn counts as reachable (the bot then walks
    /// there and fails the way the data allows).
    pub fn npc_reachable(&self, template: u32, from: Vec3) -> bool {
        let Some(nav) = self.nav() else {
            return true;
        };
        let Some(start) = nav.locate(from, 24.0) else {
            return true;
        };
        for p in self.npc_spawns(template) {
            let Some(goal) = nav.locate(*p, 24.0) else {
                continue;
            };
            if nav.may_reach(start, goal) != Some(false) {
                return true;
            }
        }
        // Every spawn is off the mesh or in another, unreachable component.
        false
    }

    /// Counters from reading the NPC spawn files.
    pub fn npc_spawn_stats(&self) -> SpawnStats {
        self.npc_spawn_stats
    }

    // ---- Doodads -------------------------------------------------------------------------

    /// A doodad template.
    pub fn doodad(&self, template: u32) -> Option<&DoodadTemplate> {
        self.doodads.templates.get(template)
    }

    /// Static spawn positions of a doodad template.
    pub fn doodad_spawns(&self, template: u32) -> &[Vec3] {
        self.doodad_spawns.of(template)
    }

    /// The spawn of `template` horizontally closest to `from`.
    pub fn nearest_doodad_spawn(&self, template: u32, from: Vec3) -> Option<Vec3> {
        self.doodad_spawns.nearest(template, from)
    }

    /// Calls `f(template, position)` for every doodad spawn within `radius` metres (horizontal).
    pub fn doodad_spawns_within(&self, center: Vec3, radius: f32, f: impl FnMut(u32, Vec3)) {
        self.doodad_spawns.within(center, radius, f)
    }

    /// Whether the doodad template spawns anywhere in this world.
    pub fn doodad_spawns_anywhere(&self, template: u32) -> bool {
        self.doodad_spawns.has(template)
    }

    /// Counters from reading the doodad spawn files.
    pub fn doodad_spawn_stats(&self) -> SpawnStats {
        self.doodad_spawn_stats
    }

    /// Doodad templates (that spawn here) which yield `item` when used.
    pub fn gather_doodads(&self, item: u32) -> &[u32] {
        self.gather_doodads.get(&item).map_or(&[], |v| &v[..])
    }

    // ---- Factions and characters ---------------------------------------------------------

    /// How faction `a` regards faction `b`.
    pub fn relation(&self, faction_a: u32, faction_b: u32) -> Relation {
        self.factions.relation(faction_a, faction_b)
    }

    /// Where a new character of `race` starts.
    pub fn race_start(&self, race: u8) -> Option<Vec3> {
        self.chars.race_start(race)
    }

    /// Whether `race` / `gender` can be created.
    pub fn creatable(&self, race: u8, gender: u8) -> bool {
        self.chars.creatable(race, gender)
    }

    /// The faction a player of `race` has.
    pub fn race_faction(&self, race: u8) -> Option<u32> {
        self.chars.race_faction(race)
    }

    // ---- Quests --------------------------------------------------------------------------

    /// A compiled quest.
    pub fn quest(&self, id: u32) -> Option<&Quest> {
        self.quests.get(id)
    }

    /// All quests in id order.
    pub fn quests(&self) -> impl Iterator<Item = &Quest> {
        self.quests.iter()
    }

    /// Quests started by talking to this NPC template.
    pub fn quests_from_npc(&self, npc_template: u32) -> &[u32] {
        self.quests.from_npc(npc_template)
    }

    /// Quests started by using this doodad template.
    pub fn quests_from_doodad(&self, doodad_template: u32) -> &[u32] {
        self.quests.from_doodad(doodad_template)
    }

    /// Aggregate numbers about the compiled quests.
    pub fn quest_stats(&self) -> QuestStats {
        self.quests.stats()
    }

    /// The unit requirements of the start components of the quests (level, race, faction, ...).
    pub fn start_gates(&self) -> &StartGates {
        &self.start_gates
    }

    /// `mother_id` of `faction` (the alliance it belongs to), 0 when it has none; what the
    /// "mother faction" quest requirements compare with.
    pub fn mother_faction(&self, faction: u32) -> u32 {
        self.factions.mother_of(faction)
    }

    /// Where the quests are (see [`crate::quest_atlas`]), built the first time it is asked for.
    /// `supported(quest)` says whether a bot can do the quest at all; the world does not know what
    /// bots can do, so the first caller's answer is the table every caller gets (the bots ask with
    /// the same predicate).
    pub fn quest_atlas(&self, supported: &dyn Fn(&Quest) -> bool) -> &QuestAtlas {
        self.quest_atlas.get_or_init(|| {
            let started = Instant::now();
            let atlas = QuestAtlas::build(self, supported);
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                points = atlas.len(),
                zones = atlas.zones().len(),
                chains = atlas.chains().len(),
                kib = atlas.bytes() / 1024,
                "quest atlas built"
            );
            atlas
        })
    }

    /// The baked sign sphere of a quest component (start / objective position).
    pub fn quest_sphere(&self, quest: u32, component: u32) -> Option<&QuestSphere> {
        let i = *self.spheres.get(&(quest, component))?;
        self.baked.quest_spheres.get(i as usize)
    }

    // ---- Skills --------------------------------------------------------------------------

    /// A skill.
    pub fn skill(&self, id: u32) -> Option<&SkillInfo> {
        self.skills.get(id)
    }

    /// Whether doodad template `template` is what a put-down or dropped backpack turns into (a
    /// trade pack lying on the ground that anybody can pick up again).
    pub fn is_pack_doodad(&self, template: u32) -> bool {
        self.doodads.is_pack_doodad(template)
    }

    /// The skill a client casts to use doodad template `doodad` (a quest prop, a lever, a bench);
    /// 0 when it has none and the client's default interaction applies.
    pub fn doodad_use_skill(&self, doodad: u32) -> u32 {
        self.doodads
            .templates
            .get(doodad)
            .map_or(0, |d| d.use_skill)
    }

    /// The skill using item `item` casts (`items.use_skill_id`), 0 when it has none.
    pub fn item_use_skill(&self, item: u32) -> u32 {
        self.item_uses.skill(item)
    }

    /// Where the item use skill `skill` must be cast (an NPC, a doodad within a range, a quest
    /// sphere); empty when it works anywhere.
    pub fn skill_needs(&self, skill: u32) -> &[SkillNeed] {
        self.item_uses.needs(skill)
    }

    /// The NPC templates of quest monster group `group` (`quest_monster_npcs`); empty when the
    /// data has none.
    pub fn quest_monster_group(&self, group: u32) -> &[u32] {
        self.item_uses.group_members(group)
    }

    /// Skill ids of class ability `ability` (1..=10) ordered by required ability level.
    pub fn ability_skills(&self, ability: u8) -> &[u32] {
        self.skills.ability_skills(ability)
    }

    // ---- Economy -------------------------------------------------------------------------

    /// A crafting recipe.
    pub fn craft(&self, id: u32) -> Option<&Craft> {
        self.economy.craft(id)
    }

    /// Trade-pack recipes offered by a bench doodad template.
    pub fn pack_crafts_at(&self, doodad_template: u32) -> &[u32] {
        self.economy.pack_crafts_at(doodad_template)
    }

    /// NPC templates that buy `pack_item`.
    pub fn specialty_buyers(&self, pack_item: u32) -> &[u32] {
        self.economy.specialty_buyers(pack_item)
    }

    /// The lowest character level any trade-pack recipe needs (the craft skill's level requirement).
    pub fn min_pack_craft_level(&self) -> u8 {
        self.economy.min_pack_level()
    }

    /// Every trade-pack recipe.
    pub fn pack_crafts(&self) -> impl Iterator<Item = &Craft> {
        self.economy.pack_crafts()
    }

    /// What a specialty NPC pays for a trade pack (copper-equivalent, at the server's best price
    /// ratio); `None` when it does not buy it. `from_zone` is the zone group the pack comes from;
    /// see [`Economy::pack_price_estimate`](crate::economy::Economy::pack_price_estimate).
    pub fn pack_price_estimate(
        &self,
        pack_item: u32,
        buyer_npc: u32,
        from_zone: u32,
    ) -> Option<u32> {
        self.economy
            .pack_price_estimate(pack_item, buyer_npc, from_zone)
    }

    /// The `specialties` row between two zone groups: `(ratio, profit)`.
    pub fn specialty_zone_pair(&self, from_group: u32, to_group: u32) -> Option<(u32, u32)> {
        self.economy.zone_pair(from_group, to_group)
    }

    /// An item's English name.
    pub fn item_name(&self, id: u32) -> Option<&str> {
        self.items.get(id)
    }

    /// The trade facts of item `template` (vendor prices, binding, stacking, auction categories,
    /// whether it is a quest item); `None` for an unknown template. See [`ItemFacts::tradable`]:
    /// a bot never asks for, sells, buys or trades away an item that is not.
    pub fn item_facts(&self, template: u32) -> Option<&ItemFacts> {
        self.item_facts.get(template)
    }

    /// The trade facts of every item template.
    pub fn item_table(&self) -> &ItemTable {
        &self.item_facts
    }

    // ---- Housing -------------------------------------------------------------------------

    /// Buildable house designs, cheapest first.
    pub fn housing_designs(&self) -> &[HousingDesign] {
        self.housing.designs()
    }

    /// A buildable house design.
    pub fn housing_design(&self, design: u32) -> Option<&HousingDesign> {
        self.housing.design(design)
    }

    /// Whether housing area `area_id` accepts houses of `category`.
    pub fn housing_area_allows(&self, area_id: u32, category: u32) -> bool {
        self.housing.area_allows(area_id, category)
    }

    /// The housing area (`housing_areas.id`) containing (x, y), from the baked outlines.
    pub fn housing_area_at(&self, x: f32, y: f32) -> Option<u32> {
        self.areas.at(&self.baked.housing_areas, x, y)
    }

    // ---- Zones ---------------------------------------------------------------------------

    /// The zone key at (x, y) from the baked zone grid.
    pub fn zone_key_at(&self, x: f32, y: f32) -> Option<u16> {
        self.baked.zones.as_ref()?.zone_key_at(x, y)
    }

    /// English zone name for a zone key.
    pub fn zone_name(&self, zone_key: u16) -> Option<&str> {
        self.zones.name(zone_key)
    }

    /// Zone key for a `zones.id` (quests use ids, the client and baked grid use keys).
    pub fn zone_key_for_id(&self, zone_id: u32) -> Option<u16> {
        self.zones.key_of_id.get(zone_id).copied()
    }

    /// The zone group and the owning faction of a zone key.
    pub fn zone_info(&self, zone_key: u16) -> Option<ZoneInfo> {
        self.zones.info(zone_key)
    }

    /// The zone info at (x, y) from the baked zone grid; `None` outside every zone (the open sea
    /// is partly one) and without the baked grid.
    pub fn zone_info_at(&self, x: f32, y: f32) -> Option<ZoneInfo> {
        self.zones.info(self.zone_key_at(x, y)?)
    }

    /// Whether `zones.closed` is set for a zone key: no ordinary player may enter it (Aubre
    /// Cradle, Airain Rock, Rainbow Field 1, ...), so a bot does not plan quests there.
    pub fn zone_closed(&self, zone_key: u16) -> bool {
        self.zones.closed(zone_key)
    }

    /// [`World::zone_closed`] at (x, y); `false` when the position is in no known zone.
    pub fn zone_closed_at(&self, x: f32, y: f32) -> bool {
        self.zone_key_at(x, y).is_some_and(|k| self.zone_closed(k))
    }

    /// Spots (x, y) inside the conflict zone group `group`: the centres of every fourth sector
    /// (256 m apart) of the baked zone grid whose zone belongs to the group, in a stable order.
    /// Empty for other groups and without the baked grid. Built for all conflict groups at the
    /// first call (a scan of the grid, a few ms) and shared by all bots.
    pub fn zone_group_spots(&self, group: u16) -> &[[f32; 2]] {
        let all = self.group_spots.get_or_init(|| {
            let mut out: FxHashMap<u16, Vec<[f32; 2]>> = FxHashMap::default();
            let Some(grid) = self.baked.zones.as_ref() else {
                return FxHashMap::default();
            };
            for row in (0..grid.height).step_by(4) {
                for col in (0..grid.width).step_by(4) {
                    let key = grid.keys[row as usize * grid.width as usize + col as usize];
                    if key == 0 {
                        continue;
                    }
                    let Some(info) = self.zones.info(key) else {
                        continue;
                    };
                    if self.is_conflict_group(info.group) {
                        out.entry(info.group).or_default().push([
                            (col as f32 + 0.5) * grid.sector,
                            (row as f32 + 0.5) * grid.sector,
                        ]);
                    }
                }
            }
            out.into_iter()
                .map(|(g, v)| (g, v.into_boxed_slice()))
                .collect()
        });
        all.get(&group).map_or(&[], |v| &v[..])
    }

    /// Whether zone group `group` takes part in the zone conflicts (land conflict zones and the
    /// sea groups; not the closed ones).
    pub fn is_conflict_group(&self, group: u16) -> bool {
        self.zones.conflict_groups.binary_search(&group).is_ok()
    }

    /// The zone groups of the zone conflicts, ascending.
    pub fn conflict_groups(&self) -> &[u16] {
        &self.zones.conflict_groups
    }
}

/// `items` ids with English names.
fn load_items(conn: &Connection, texts: &Texts) -> Result<StrTable> {
    let mut entries: Vec<(u32, Box<str>)> = Vec::new();
    each_row(conn, "SELECT id, name FROM items", |r| {
        let id = r.u32(0);
        entries.push((id, texts.name_or(TextTable::Item, id, r.text(1))));
    })?;
    Ok(StrTable::build(entries))
}
