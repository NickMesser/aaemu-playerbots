//! Extracts the data player bots need from the ArcheAge 1.2 client `game_pak`.
//!
//! [`bake`] reads, for one world, the per-cell heightmaps, `world.xml`, the housing area polygons,
//! the public farm polygons, the quest sign spheres and the painted world map from the pak and writes the files described in
//! `docs/DATA-FORMATS.md` into the output directory:
//!
//! ```text
//! heights_<world>.bin  map_<world>.png  map_<world>_relief.png  map_<world>.json
//! housing_areas.json   farm_areas.json   quest_spheres.json   zones_<world>.bin
//! debug_map_markers.png
//! ```

pub mod areas;
pub mod compact;
pub mod dds;
pub mod heights;
pub mod hmap;
pub mod map;
pub mod nav;
pub mod pak;
pub mod worldxml;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use aa_world::baked::{
    write_json, HousingAreaPoly, MapCalibration, QuestSphere, WorldRect, ZoneGrid,
};
use aa_world::terrain::{write_height_grid, Terrain};
use anyhow::{bail, Context};

use crate::compact::{Compact, RectF};
use crate::heights::{bake_heights, validate_step, HeightGrid, HeightsReport};
use crate::pak::Pak;
use crate::worldxml::{parse_world_xml, WorldDef};

/// Scratch directory for tests: below `$AA_TEST_TMP` when set (a roomier volume), else the OS
/// temp dir.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> PathBuf {
    let base = match std::env::var_os("AA_TEST_TMP") {
        Some(root) => PathBuf::from(root).join("tmp-test"),
        None => std::env::temp_dir().join("aa-bake-test"),
    };
    let dir = base.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Inputs of a bake run.
#[derive(Debug, Clone)]
pub struct BakeOptions {
    /// The client's `game_pak`.
    pub pak: PathBuf,
    /// Output directory (created if missing).
    pub out: PathBuf,
    /// World name, e.g. `main_world`.
    pub world: String,
    /// Metres between height samples; a multiple of 2 that divides 1024 (default 4.0).
    pub step: f32,
    /// The server's `compact.sqlite3`: validates housing area ids and supplies the map rectangle.
    pub compact: Option<PathBuf>,
}

impl BakeOptions {
    pub fn new(pak: impl Into<PathBuf>, out: impl Into<PathBuf>, world: impl Into<String>) -> Self {
        BakeOptions {
            pak: pak.into(),
            out: out.into(),
            world: world.into(),
            step: 4.0,
            compact: None,
        }
    }
}

/// Which parts of the bake to run (all by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakeSteps {
    pub heights: bool,
    pub zones: bool,
    pub housing: bool,
    /// The public farm polygons (`farm_areas.json`).
    pub farms: bool,
    pub spheres: bool,
    pub map: bool,
    /// The navigation mesh of the world being baked (`nav_<world>.bin`).
    pub nav: bool,
    /// The navigation meshes of every other world with `.bai` files in the pak (the instance
    /// worlds).
    pub nav_instances: bool,
}

impl BakeSteps {
    pub fn all() -> Self {
        BakeSteps {
            heights: true,
            zones: true,
            housing: true,
            farms: true,
            spheres: true,
            map: true,
            nav: true,
            nav_instances: true,
        }
    }

    pub fn none() -> Self {
        BakeSteps {
            heights: false,
            zones: false,
            housing: false,
            farms: false,
            spheres: false,
            map: false,
            nav: false,
            nav_instances: false,
        }
    }
}

/// A verification result of the bake.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// What a bake run produced.
#[derive(Debug, Clone, Default)]
pub struct BakeReport {
    /// Files written: name and size in bytes.
    pub files: Vec<(String, u64)>,
    pub pak_entries_kept: usize,
    pub heights: Option<HeightsReport>,
    pub zones: usize,
    pub zone_sectors_assigned: usize,
    pub housing_areas: usize,
    pub housing_areas_dropped_unknown_id: usize,
    pub farm_areas: usize,
    pub quest_spheres: usize,
    /// `"painted"` or `"relief"`: what `map_<world>.json` points at.
    pub map_source: Option<String>,
    pub map_rect: Option<WorldRect>,
    /// One entry per navigation mesh baked (the world, then the instance worlds).
    pub nav: Vec<nav::NavReport>,
    pub checks: Vec<Check>,
    pub warnings: Vec<String>,
    /// Step name and seconds.
    pub timings: Vec<(String, f32)>,
}

impl BakeReport {
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        self.checks.push(Check {
            name: name.into(),
            ok,
            detail,
        });
    }

    fn warn(&mut self, msg: String) {
        tracing::warn!("{msg}");
        self.warnings.push(msg);
    }
}

impl fmt::Display for BakeReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "pak entries kept: {}", self.pak_entries_kept)?;
        if let Some(h) = &self.heights {
            writeln!(
                f,
                "heights: {}x{} @ {} m, {}/{} cells with terrain, {} failed, {:.1}..{:.1} m ({} samples clamped at 2047.97 m), {:.0} MB read",
                h.width,
                h.height,
                h.step,
                h.cells_with_terrain,
                h.cells_in_world,
                h.failed_cells.len(),
                h.min_height,
                h.max_height,
                h.samples_clamped,
                h.bytes_read as f64 / 1e6
            )?;
        }
        writeln!(
            f,
            "zones: {} in world.xml, {} sectors assigned; housing areas: {} ({} dropped: id not in compact); farm areas: {}; quest spheres: {}",
            self.zones,
            self.zone_sectors_assigned,
            self.housing_areas,
            self.housing_areas_dropped_unknown_id,
            self.farm_areas,
            self.quest_spheres
        )?;
        if let (Some(src), Some(r)) = (&self.map_source, &self.map_rect) {
            writeln!(
                f,
                "map: source {src}, rect x {}..{} y {}..{}",
                r.min_x, r.max_x, r.min_y, r.max_y
            )?;
        }
        for n in &self.nav {
            write!(f, "{n}")?;
        }
        for (name, bytes) in &self.files {
            writeln!(f, "file {name}: {bytes} bytes")?;
        }
        for (name, secs) in &self.timings {
            writeln!(f, "time {name}: {secs:.1} s")?;
        }
        for c in &self.checks {
            writeln!(
                f,
                "check {}: {} ({})",
                c.name,
                if c.ok { "OK" } else { "FAILED" },
                c.detail
            )?;
        }
        for w in &self.warnings {
            writeln!(f, "warning: {w}")?;
        }
        Ok(())
    }
}

/// The four race starts, used to verify the map calibration: name, world x/y, marker colour.
pub const RACE_STARTS: [(&str, f32, f32, [u8; 3]); 4] = [
    ("Nuian (Solzreed)", 15578.04, 15382.12, [230, 30, 30]),
    ("Elf (Gweonid Forest)", 10388.0, 15982.0, [30, 200, 60]),
    ("Hariharan (Rainbow Field)", 19304.0, 7646.0, [40, 90, 255]),
    ("Ferre (Falcony Plateau)", 23881.0, 10495.0, [220, 40, 220]),
];

/// The whole-world map rectangle of `main_world` (compact `world_groups`), used when no compact
/// database is given. Verified against the height grid, see [`map::debug_overlay`].
pub const MAIN_WORLD_PAINTED_RECT: WorldRect = WorldRect {
    min_x: -14731.0,
    min_y: 46.0,
    max_x: -14731.0 + 64651.0,
    max_y: 46.0 + 38865.0,
};

fn rect_of(r: &RectF) -> WorldRect {
    WorldRect {
        min_x: r.x as f32,
        min_y: r.y as f32,
        max_x: (r.x + r.w) as f32,
        max_y: (r.y + r.h) as f32,
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

impl HeightGrid {
    /// Loads a baked `heights_<world>.bin` back into memory (for map-only runs).
    pub fn from_file(path: &Path) -> anyhow::Result<HeightGrid> {
        let t = Terrain::open(path)?;
        let h = t.header;
        let mut samples = Vec::with_capacity(h.width as usize * h.height as usize);
        for row in 0..h.height {
            for col in 0..h.width {
                samples.push(t.raw(col, row).unwrap_or(0));
            }
        }
        Ok(HeightGrid { header: h, samples })
    }
}

/// Runs the whole bake.
pub fn bake(opts: &BakeOptions) -> anyhow::Result<BakeReport> {
    bake_steps(opts, &BakeSteps::all())
}

/// Runs the selected parts of the bake. `map` without `heights` reads the previously baked height
/// grid from the output directory.
pub fn bake_steps(opts: &BakeOptions, steps: &BakeSteps) -> anyhow::Result<BakeReport> {
    validate_step(opts.step)?;
    std::fs::create_dir_all(&opts.out).with_context(|| format!("create {}", opts.out.display()))?;
    let mut report = BakeReport::default();
    let world_name = opts.world.as_str();

    let t = Instant::now();
    let world_prefix = format!("game/worlds/{world_name}/");
    let nav_instances = steps.nav_instances;
    let mut pak = Pak::open_filtered(&opts.pak, |n| {
        n.starts_with(&world_prefix)
            || n.starts_with("game/ui/map/world/")
            // other worlds: only what the navigation meshes need
            || (nav_instances
                && n.starts_with("game/worlds/")
                && (n.ends_with("/world.xml")
                    || (n.ends_with(".bai")
                        && (n.contains("/netmission") || n.contains("/vertsmission")))))
    })?;
    report.pak_entries_kept = pak.len();
    let xml = pak
        .read_text(&format!("{world_prefix}world.xml"))?
        .with_context(|| {
            format!(
                "{world_prefix}world.xml not found in {}",
                opts.pak.display()
            )
        })?;
    let world = parse_world_xml(&xml)?;
    if world.name != world_name {
        bail!(
            "world.xml names the world {:?}, expected {world_name:?}",
            world.name
        );
    }
    report
        .timings
        .push(("open pak + world.xml".into(), t.elapsed().as_secs_f32()));

    let compact = match &opts.compact {
        Some(p) => Some(Compact::open(p)?),
        None => None,
    };

    let mut grid: Option<HeightGrid> = None;

    if steps.heights {
        let t = Instant::now();
        let (g, hr) = bake_heights(&mut pak, &world, opts.step)?;
        let path = opts.out.join(format!("heights_{world_name}.bin"));
        write_height_grid(&path, &g.header, &g.samples)?;
        report
            .files
            .push((format!("heights_{world_name}.bin"), file_size(&path)));
        for f in &hr.failed_cells {
            report.warn(format!("heightmap not decoded: {f}"));
        }
        report.heights = Some(hr);
        report
            .timings
            .push(("heights".into(), t.elapsed().as_secs_f32()));
        if world_name == "main_world" {
            // Nuian start: the server's terrain height there is 126.2 - 126.5 m.
            let z = g.height(15578.04, 15382.12);
            report.check(
                "Nuian start height",
                z.is_some_and(|z| (126.0..=126.7).contains(&z)),
                format!("{z:?} m at (15578.04, 15382.12), expected 126.2-126.5"),
            );
        }
        grid = Some(g);
    }

    if steps.zones {
        let t = Instant::now();
        let zg = ZoneGrid {
            width: world.sectors_x,
            height: world.sectors_y,
            sector: worldxml::SECTOR_SIZE,
            keys: world.sector_zone.clone(),
        };
        let path = opts.out.join(format!("zones_{world_name}.bin"));
        zg.write(&path)?;
        report
            .files
            .push((format!("zones_{world_name}.bin"), file_size(&path)));
        report.zones = world.zones.len();
        report.zone_sectors_assigned = world.sector_zone.iter().filter(|&&k| k != 0).count();
        if world_name == "main_world" {
            let z = world.zone_at(15578.04, 15382.12);
            report.check(
                "zone at the Nuian start",
                z != 0,
                format!("zone key {z} (w_solzreed_*)"),
            );
        }
        report
            .timings
            .push(("zones".into(), t.elapsed().as_secs_f32()));
    }

    if steps.housing {
        let t = Instant::now();
        let mut polys: Vec<HousingAreaPoly> = Vec::new();
        for zone in &world.zones {
            let name = format!(
                "{world_prefix}level_design/zone/{}/client/housing_area.xml",
                zone.key
            );
            let Some(text) = pak.read_text(&name)? else {
                continue;
            };
            match areas::parse_housing_areas(&text, zone.key, zone.origin) {
                Ok(mut v) => polys.append(&mut v),
                Err(e) => report.warn(format!("{name}: {e:#}")),
            }
        }
        polys.retain(|p| p.id != 0 && p.points.len() >= 3);
        if let Some(c) = &compact {
            let known = c.housing_area_ids()?;
            let before = polys.len();
            polys.retain(|p| known.contains(&p.id));
            report.housing_areas_dropped_unknown_id = before - polys.len();
        }
        let path = opts.out.join("housing_areas.json");
        write_json(&path, &polys)?;
        report
            .files
            .push(("housing_areas.json".into(), file_size(&path)));
        report.housing_areas = polys.len();
        if world_name == "main_world" {
            let ids: Vec<u32> = polys
                .iter()
                .filter(|p| areas::polygon_contains(&p.points, [13944.868, 14216.393]))
                .map(|p| p.id)
                .collect();
            report.check(
                "housing area at (13944.868, 14216.393)",
                ids.contains(&52),
                format!("areas containing the point: {ids:?}, expected 52"),
            );
        }
        report
            .timings
            .push(("housing areas".into(), t.elapsed().as_secs_f32()));
    }

    if steps.farms {
        let t = Instant::now();
        let mut polys: Vec<HousingAreaPoly> = Vec::new();
        for zone in &world.zones {
            let name = format!(
                "{world_prefix}level_design/zone/{}/client/subzone_area.xml",
                zone.key
            );
            let Some(text) = pak.read_text(&name)? else {
                continue;
            };
            match areas::parse_sub_zone_areas(&text, zone.key, zone.origin) {
                Ok(v) => polys.extend(v.into_iter().filter(|p| {
                    aa_world::farm::FarmKind::of_sub_zone(p.id).is_some() && p.points.len() >= 3
                })),
                Err(e) => report.warn(format!("{name}: {e:#}")),
            }
        }
        let path = opts.out.join("farm_areas.json");
        write_json(&path, &polys)?;
        report
            .files
            .push(("farm_areas.json".into(), file_size(&path)));
        report.farm_areas = polys.len();
        for p in &polys {
            tracing::info!(
                "farm area: sub zone {} zone {} corners {}",
                p.id,
                p.zone_key,
                p.points.len()
            );
        }
        report
            .timings
            .push(("farm areas".into(), t.elapsed().as_secs_f32()));
    }

    if steps.spheres {
        let t = Instant::now();
        let mut spheres: Vec<QuestSphere> = Vec::new();
        for zone in &world.zones {
            let name = format!(
                "{world_prefix}level_design/zone/{}/client/quest_sign_sphere.g",
                zone.key
            );
            let Some(text) = pak.read_text(&name)? else {
                continue;
            };
            match areas::parse_quest_spheres(&text, zone.origin) {
                Ok(mut v) => spheres.append(&mut v),
                Err(e) => report.warn(format!("{name}: {e:#}")),
            }
        }
        let path = opts.out.join("quest_spheres.json");
        write_json(&path, &spheres)?;
        report
            .files
            .push(("quest_spheres.json".into(), file_size(&path)));
        report.quest_spheres = spheres.len();
        report
            .timings
            .push(("quest spheres".into(), t.elapsed().as_secs_f32()));
    }

    if steps.nav {
        let t = Instant::now();
        let origins = world.zones.iter().map(|z| (z.key, z.origin)).collect();
        let min_nodes = if world_name == "main_world" {
            nav::MIN_COMPONENT_MAIN
        } else {
            nav::MIN_COMPONENT_INSTANCE
        };
        match nav::bake_nav_world(&mut pak, world_name, &origins, &opts.out, min_nodes) {
            Ok(r) => finish_nav(&mut report, &opts.out, r),
            Err(e) => report.warn(format!("navigation mesh of {world_name} not baked: {e:#}")),
        }
        report
            .timings
            .push(("navigation mesh".into(), t.elapsed().as_secs_f32()));
    }

    if steps.nav_instances {
        let t = Instant::now();
        let others: Vec<String> = nav::nav_worlds_in_pak(&pak)
            .into_keys()
            .filter(|w| w != world_name)
            .collect();
        for w in others {
            let origins = match pak.read_text(&format!("game/worlds/{w}/world.xml")) {
                Ok(Some(xml)) => nav::parse_zone_origins(&xml),
                _ => Default::default(),
            };
            match nav::bake_nav_world(
                &mut pak,
                &w,
                &origins,
                &opts.out,
                nav::MIN_COMPONENT_INSTANCE,
            ) {
                Ok(r) => finish_nav(&mut report, &opts.out, r),
                Err(e) => report.warn(format!("navigation mesh of {w} not baked: {e:#}")),
            }
        }
        report.timings.push((
            "instance navigation meshes".into(),
            t.elapsed().as_secs_f32(),
        ));
    }

    if steps.map {
        let t = Instant::now();
        let grid = match grid.take() {
            Some(g) => g,
            None => HeightGrid::from_file(&opts.out.join(format!("heights_{world_name}.bin")))
                .context("the map needs the height grid: run the heights step first")?,
        };
        bake_map(&mut pak, &world, &grid, compact.as_ref(), opts, &mut report)?;
        report
            .timings
            .push(("map".into(), t.elapsed().as_secs_f32()));
    }
    Ok(report)
}

/// Records a baked navigation mesh and verifies the file by reading it back (a full scan of its
/// indices); for `main_world` also checks that the Nuian start is on the mesh and walkable to the
/// neighbouring towns.
fn finish_nav(report: &mut BakeReport, out: &Path, r: nav::NavReport) {
    use aa_world::nav::{NavMesh, PathQuery, PathResult};
    use aa_world::Vec3;

    report.files.push((r.file.clone(), r.bytes));
    let world = r.world.clone();
    let path = out.join(&r.file);
    for w in &r.warnings {
        tracing::warn!("nav {world}: {w}");
    }
    report.nav.push(r);
    let mesh = match NavMesh::open(&path).and_then(|m| m.verify().map(|()| m)) {
        Ok(m) => {
            report.check(
                &format!("nav {world} reads back"),
                true,
                format!(
                    "{} nodes, {} links verified",
                    m.node_count(),
                    m.link_count()
                ),
            );
            m
        }
        Err(e) => {
            report.check(&format!("nav {world} reads back"), false, format!("{e:#}"));
            return;
        }
    };
    if world != "main_world" {
        return;
    }
    // Nuian start, Desireen and Wardton (positions of well-known NPCs)
    let start = Vec3::new(15578.04, 15382.12, 126.3);
    let on_mesh = mesh.locate(start, 8.0).is_some();
    report.check(
        "nav: Nuian start is on the mesh",
        on_mesh,
        "within 8 m of (15578.04, 15382.12, 126.3)".into(),
    );
    for (name, to) in [
        ("Desireen", Vec3::new(15667.9, 15166.7, 120.7)),
        ("Wardton", Vec3::new(14809.2, 14295.8, 119.9)),
    ] {
        let res = mesh.find_path(&PathQuery::new(start, to));
        let (ok, detail) = match &res {
            PathResult::Found(p) => (
                true,
                format!(
                    "{} corners, {:.0} m, {} expansions",
                    p.points.len(),
                    p.length,
                    p.expansions
                ),
            ),
            other => (false, format!("{other:?}").chars().take(80).collect()),
        };
        report.check(&format!("nav: path Nuian start -> {name}"), ok, detail);
    }
}

/// Width of the relief fallback image in pixels.
const RELIEF_WIDTH: u32 = 2240;

fn bake_map(
    pak: &mut Pak,
    world: &WorldDef,
    grid: &HeightGrid,
    compact: Option<&Compact>,
    opts: &BakeOptions,
    report: &mut BakeReport,
) -> anyhow::Result<()> {
    let name = world.name.as_str();
    let json_path = opts.out.join(format!("map_{name}.json"));

    // Relief fallback: always written.
    let (relief, relief_rect) = map::render_relief(grid, world.ocean_level, RELIEF_WIDTH);
    let relief_file = format!("map_{name}_relief.png");
    relief.write_png(&opts.out.join(&relief_file))?;
    report
        .files
        .push((relief_file.clone(), file_size(&opts.out.join(&relief_file))));

    // Painted map from the client's UI.
    let dds_names = [
        format!("game/ui/map/world/en_us/{name}.dds"),
        format!("game/ui/map/world/{name}.dds"),
    ];
    let mut painted = None;
    for n in &dds_names {
        if let Some(bytes) = pak.read(n)? {
            painted = Some(dds::decode_dds(&bytes).with_context(|| n.clone())?);
            break;
        }
    }

    let painted_rect = match compact {
        Some(c) => c.world_group_rect(name)?.map(|r| rect_of(&r)),
        None => None,
    }
    .or_else(|| (name == "main_world").then_some(MAIN_WORLD_PAINTED_RECT));

    let calibration = match (&painted, painted_rect) {
        (Some(img), Some(rect)) => {
            let painted_file = format!("map_{name}.png");
            map::write_rgba_png(img, &opts.out.join(&painted_file))?;
            report.files.push((
                painted_file.clone(),
                file_size(&opts.out.join(&painted_file)),
            ));
            // Debug overlay: coastline of the height grid, zone group rectangles, race starts.
            let zone_rects: Vec<WorldRect> = match compact {
                Some(c) => c
                    .zone_group_rects()?
                    .iter()
                    .map(|(_, r)| rect_of(r))
                    .collect(),
                None => Vec::new(),
            };
            let markers: Vec<map::DebugMarker> = if name == "main_world" {
                RACE_STARTS
                    .iter()
                    .map(|&(_, x, y, color)| map::DebugMarker { x, y, color })
                    .collect()
            } else {
                Vec::new()
            };
            let overlay =
                map::debug_overlay(img, &rect, grid, world.ocean_level, &zone_rects, &markers);
            let dbg = opts.out.join("debug_map_markers.png");
            overlay.write_png(&dbg)?;
            report
                .files
                .push(("debug_map_markers.png".into(), file_size(&dbg)));
            MapCalibration {
                image: painted_file,
                width: img.width,
                height: img.height,
                source: "painted".into(),
                world: rect,
            }
        }
        _ => {
            if painted.is_none() {
                report.warn(format!(
                    "no painted map for {name} in the pak; using the relief"
                ));
            } else {
                report.warn(format!(
                    "no calibrated rectangle for the painted {name} map; using the relief"
                ));
            }
            MapCalibration {
                image: relief_file,
                width: relief.width,
                height: relief.height,
                source: "relief".into(),
                world: relief_rect,
            }
        }
    };
    write_json(&json_path, &calibration)?;
    report
        .files
        .push((format!("map_{name}.json"), file_size(&json_path)));
    report.map_source = Some(calibration.source.clone());
    report.map_rect = Some(calibration.world);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aa_world::terrain::{write_height_grid, HeightGridHeader};

    #[test]
    fn map_only_bake_rejects_an_old_height_grid_with_a_rebake_hint() {
        let dir = test_dir("bake-old-heights");
        let path = dir.join("heights_main_world.bin");
        let header = HeightGridHeader {
            width: 2,
            height: 2,
            origin_x: 0.0,
            origin_y: 0.0,
            step: 4.0,
            scale: 1.0 / 32.0,
            offset: 0.0,
            flags: 0,
        };
        write_height_grid(&path, &header, &[0, 32, 64, 96]).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        std::io::Write::write_all(&mut f, b"PBHGT001").unwrap();
        drop(f);

        let Err(e) = HeightGrid::from_file(&path) else {
            panic!("an old bake was accepted");
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("older playerbots"), "{msg}");
        assert!(msg.contains("PBHGT001"), "{msg}");
        assert!(msg.contains("`playerbots bake`"), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
