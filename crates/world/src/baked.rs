//! Baked extras written by `aa-bake` (see `docs/DATA-FORMATS.md`): map calibration, housing area
//! polygons, public farm polygons, quest sign spheres and the zone sector grid.
//!
//! CONTRACT (shared by the `aa-bake` writer and the `aa-world` reader). The types and function
//! signatures in this file are fixed; implementations may change.

use std::path::Path;

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

use crate::nav::NavSet;
use crate::terrain::Terrain;

/// World rectangle covered by the map image (metres).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldRect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

/// `map_<world>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapCalibration {
    /// File name of the image, relative to the data dir.
    pub image: String,
    pub width: u32,
    pub height: u32,
    /// `"painted"` or `"relief"`.
    pub source: String,
    pub world: WorldRect,
}

/// One entry of `housing_areas.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HousingAreaPoly {
    /// `housing_areas.id` in compact.sqlite3.
    pub id: u32,
    pub zone_key: u32,
    /// World x/y, polygon in order (not closed: last != first).
    pub points: Vec<[f32; 2]>,
}

/// One entry of `quest_spheres.json`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QuestSphere {
    pub quest: u32,
    pub component: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub radius: f32,
}

pub const ZONES_MAGIC: &[u8; 8] = b"PBZON001";
const ZONES_HEADER_LEN: usize = 32;

/// `zones_<world>.bin`: zone key per 64 m sector.
pub struct ZoneGrid {
    pub width: u32,
    pub height: u32,
    pub sector: f32,
    /// Row-major zone keys (`row * width + col`), 0 = none.
    pub keys: Vec<u16>,
}

impl ZoneGrid {
    pub fn open(path: &Path) -> anyhow::Result<ZoneGrid> {
        let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        ensure!(
            data.len() >= ZONES_HEADER_LEN,
            "{}: shorter than the zone grid header",
            path.display()
        );
        ensure!(
            &data[0..8] == ZONES_MAGIC,
            "{}: bad zone grid magic",
            path.display()
        );
        let u32_at =
            |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        let width = u32_at(8);
        let height = u32_at(12);
        let sector = f32::from_bits(u32_at(16));
        ensure!(
            width > 0 && height > 0,
            "{}: empty zone grid ({width}x{height})",
            path.display()
        );
        ensure!(
            sector.is_finite() && sector > 0.0,
            "{}: bad sector size {sector}",
            path.display()
        );
        let count = width as usize * height as usize;
        ensure!(
            data.len() >= ZONES_HEADER_LEN + count * 2,
            "{}: truncated zone grid ({} bytes, expected {})",
            path.display(),
            data.len(),
            ZONES_HEADER_LEN + count * 2
        );
        let keys = (0..count)
            .map(|i| {
                let o = ZONES_HEADER_LEN + i * 2;
                u16::from_le_bytes([data[o], data[o + 1]])
            })
            .collect();
        Ok(ZoneGrid {
            width,
            height,
            sector,
            keys,
        })
    }

    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        ensure!(
            self.keys.len() == self.width as usize * self.height as usize,
            "zone grid has {} keys, header says {}x{}",
            self.keys.len(),
            self.width,
            self.height
        );
        let mut out = Vec::with_capacity(ZONES_HEADER_LEN + self.keys.len() * 2);
        out.extend_from_slice(ZONES_MAGIC);
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.sector.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        for k in &self.keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        std::fs::write(path, out).with_context(|| format!("write {}", path.display()))
    }

    /// Zone key at world (x, y), `None` outside the grid or for key 0.
    pub fn zone_key_at(&self, x: f32, y: f32) -> Option<u16> {
        let col = (x / self.sector).floor();
        let row = (y / self.sector).floor();
        // Written so that NaN fails the test.
        if !(col >= 0.0 && row >= 0.0 && col < self.width as f32 && row < self.height as f32) {
            return None;
        }
        let key = *self
            .keys
            .get(row as usize * self.width as usize + col as usize)?;
        (key != 0).then_some(key)
    }
}

/// Everything baked for one world. Missing files load as `None`/empty (with a warning), so bots
/// run without baked data (flat terrain fallback, no map image).
#[derive(Default)]
pub struct Baked {
    pub terrain: Option<Terrain>,
    pub map: Option<MapCalibration>,
    pub housing_areas: Vec<HousingAreaPoly>,
    pub quest_spheres: Vec<QuestSphere>,
    /// The public farm sub zone polygons (`farm_areas.json`; `id` is the sub zone id).
    pub farm_areas: Vec<HousingAreaPoly>,
    pub zones: Option<ZoneGrid>,
    /// The navigation meshes (`nav_<world>.bin`): the main world's and the instance worlds'.
    pub navs: NavSet,
}

/// Loads the height grid. A missing file warns (the bots use the flat fallback); an unusable one
/// is an error with the file path, so an old bake that must be re-run does not go unnoticed.
fn load_terrain(path: &Path) -> Option<Terrain> {
    if !path.is_file() {
        tracing::warn!("baked height grid not found: {}", path.display());
        return None;
    }
    match Terrain::open(path) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::error!("baked height grid unusable: {e:#}");
            None
        }
    }
}

/// Runs `load` on `path` if the file exists; a missing file warns, a malformed one warns with the
/// error and yields `None`.
fn load_optional<T>(
    path: &Path,
    what: &str,
    load: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> Option<T> {
    if !path.is_file() {
        tracing::warn!("baked {what} not found: {}", path.display());
        return None;
    }
    match load(path) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("skipping malformed baked {what} {}: {e:#}", path.display());
            None
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

impl Baked {
    /// Loads `<dir>/heights_<world>.bin`, `map_<world>.json`, `housing_areas.json`,
    /// `quest_spheres.json` and `zones_<world>.bin`, each optional.
    pub fn load(dir: &Path, world: &str) -> Baked {
        Baked {
            terrain: load_terrain(&dir.join(format!("heights_{world}.bin"))),
            map: load_optional(
                &dir.join(format!("map_{world}.json")),
                "map calibration",
                read_json::<MapCalibration>,
            ),
            housing_areas: load_optional(
                &dir.join("housing_areas.json"),
                "housing areas",
                read_json::<Vec<HousingAreaPoly>>,
            )
            .unwrap_or_default(),
            quest_spheres: load_optional(
                &dir.join("quest_spheres.json"),
                "quest spheres",
                read_json::<Vec<QuestSphere>>,
            )
            .unwrap_or_default(),
            farm_areas: load_farm_areas(&dir.join("farm_areas.json")),
            zones: load_optional(
                &dir.join(format!("zones_{world}.bin")),
                "zone grid",
                ZoneGrid::open,
            ),
            navs: NavSet::load(dir, world),
        }
    }
}

/// `farm_areas.json`: optional (an older bake does not have it, the bots then plant on their own
/// land only), so a missing file is only noted.
fn load_farm_areas(path: &Path) -> Vec<HousingAreaPoly> {
    if !path.is_file() {
        tracing::info!(
            "baked farm areas not found: {} (re-run `playerbots bake` for public farms)",
            path.display()
        );
        return Vec::new();
    }
    load_optional(path, "farm areas", read_json::<Vec<HousingAreaPoly>>).unwrap_or_default()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    let text = serde_json::to_string(value)?;
    std::fs::write(path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::{test_dir, write_height_grid, HeightGridHeader};

    fn grid() -> ZoneGrid {
        // 3 columns x 2 rows, 64 m sectors: row 0 = [0, 5, 7], row 1 = [9, 0, 300]
        ZoneGrid {
            width: 3,
            height: 2,
            sector: 64.0,
            keys: vec![0, 5, 7, 9, 0, 300],
        }
    }

    #[test]
    fn zone_grid_roundtrip_and_lookup() {
        let dir = test_dir("baked-zones");
        let path = dir.join("zones_test.bin");
        grid().write(&path).unwrap();
        let g = ZoneGrid::open(&path).unwrap();
        assert_eq!((g.width, g.height, g.sector), (3, 2, 64.0));
        assert_eq!(g.keys, grid().keys);
        assert_eq!(g.zone_key_at(0.0, 0.0), None, "key 0 is none");
        assert_eq!(g.zone_key_at(64.0, 0.0), Some(5));
        assert_eq!(g.zone_key_at(191.9, 63.9), Some(7));
        assert_eq!(g.zone_key_at(10.0, 64.0), Some(9));
        assert_eq!(g.zone_key_at(150.0, 100.0), Some(300));
        assert_eq!(g.zone_key_at(192.0, 0.0), None, "outside in x");
        assert_eq!(g.zone_key_at(0.0, 128.0), None, "outside in y");
        assert_eq!(g.zone_key_at(-0.1, 0.0), None);
        assert_eq!(g.zone_key_at(f32::NAN, 0.0), None);
        // bad files
        std::fs::write(dir.join("bad.bin"), b"PBZON001\x03\0\0\0").unwrap();
        assert!(ZoneGrid::open(&dir.join("bad.bin")).is_err());
        let mut trunc = std::fs::read(&path).unwrap();
        trunc.pop();
        std::fs::write(dir.join("trunc.bin"), trunc).unwrap();
        assert!(ZoneGrid::open(&dir.join("trunc.bin")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_missing_dir_is_empty() {
        let b = Baked::load(Path::new("definitely/not/here"), "main_world");
        assert!(b.terrain.is_none());
        assert!(b.map.is_none());
        assert!(b.housing_areas.is_empty());
        assert!(b.quest_spheres.is_empty());
        assert!(b.zones.is_none());
    }

    #[test]
    fn load_reads_everything_and_skips_malformed() {
        let dir = test_dir("baked-load");
        let hdr = HeightGridHeader {
            width: 2,
            height: 2,
            origin_x: 0.0,
            origin_y: 0.0,
            step: 4.0,
            scale: 1.0 / 64.0,
            offset: 0.0,
            flags: 0,
        };
        write_height_grid(&dir.join("heights_w.bin"), &hdr, &[0, 64, 128, 192]).unwrap();
        grid().write(&dir.join("zones_w.bin")).unwrap();
        let map = MapCalibration {
            image: "map_w.png".into(),
            width: 10,
            height: 5,
            source: "relief".into(),
            world: WorldRect {
                min_x: 0.0,
                min_y: 0.0,
                max_x: 100.0,
                max_y: 50.0,
            },
        };
        write_json(&dir.join("map_w.json"), &map).unwrap();
        let areas = vec![HousingAreaPoly {
            id: 52,
            zone_key: 179,
            points: vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]],
        }];
        write_json(&dir.join("housing_areas.json"), &areas).unwrap();
        // malformed quest spheres file: skipped, not fatal
        std::fs::write(dir.join("quest_spheres.json"), "{ not json").unwrap();

        let b = Baked::load(&dir, "w");
        assert_eq!(b.terrain.as_ref().unwrap().header, hdr);
        assert_eq!(b.map.as_ref(), Some(&map));
        assert_eq!(b.housing_areas, areas);
        assert!(b.quest_spheres.is_empty());
        assert_eq!(b.zones.as_ref().unwrap().zone_key_at(70.0, 0.0), Some(5));
        drop(b);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Reads the real baked data dir; run with `AA_BAKED=<data dir>`.
    #[test]
    #[ignore = "needs a baked data dir: set AA_BAKED"]
    fn loads_real_baked_data() {
        let dir = std::env::var("AA_BAKED").expect("AA_BAKED not set");
        let b = Baked::load(Path::new(&dir), "main_world");
        let terrain = b.terrain.as_ref().expect("heights_main_world.bin");
        // Nuian start
        let z = terrain.height(15578.04, 15382.12).expect("inside the grid");
        assert!((125.0..128.0).contains(&z), "Nuian start height {z}");
        let map = b.map.as_ref().expect("map_main_world.json");
        assert!(Path::new(&dir).join(&map.image).is_file(), "map image");
        assert!(!b.housing_areas.is_empty(), "housing areas");
        assert!(!b.quest_spheres.is_empty(), "quest spheres");
        let zones = b.zones.as_ref().expect("zones_main_world.bin");
        assert!(
            zones.zone_key_at(15578.0, 15382.0).is_some(),
            "zone at the Nuian start"
        );
    }
}
