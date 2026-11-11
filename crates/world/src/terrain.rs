//! Terrain height grid (`heights_<world>.bin`, see `docs/DATA-FORMATS.md`), memory-mapped so only
//! the pages around bots become resident.
//!
//! CONTRACT (shared by the `aa-bake` writer and the `aa-world` reader). The types and function
//! signatures in this file are fixed; implementations may change.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{bail, ensure, Context};
use memmap2::Mmap;

pub const HEIGHTS_MAGIC: &[u8; 8] = b"PBHGT002";
pub const HEIGHTS_HEADER_LEN: usize = 64;
const HEIGHTS_MAGIC_PREFIX: &[u8; 5] = b"PBHGT";

/// Header of a height grid file.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeightGridHeader {
    pub width: u32,
    pub height: u32,
    pub origin_x: f32,
    pub origin_y: f32,
    pub step: f32,
    pub scale: f32,
    pub offset: f32,
    pub flags: u32,
}

impl HeightGridHeader {
    fn to_bytes(self) -> [u8; HEIGHTS_HEADER_LEN] {
        let mut b = [0u8; HEIGHTS_HEADER_LEN];
        b[0..8].copy_from_slice(HEIGHTS_MAGIC);
        b[8..12].copy_from_slice(&self.width.to_le_bytes());
        b[12..16].copy_from_slice(&self.height.to_le_bytes());
        b[16..20].copy_from_slice(&self.origin_x.to_le_bytes());
        b[20..24].copy_from_slice(&self.origin_y.to_le_bytes());
        b[24..28].copy_from_slice(&self.step.to_le_bytes());
        b[28..32].copy_from_slice(&self.scale.to_le_bytes());
        b[32..36].copy_from_slice(&self.offset.to_le_bytes());
        b[36..40].copy_from_slice(&self.flags.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> anyhow::Result<Self> {
        ensure!(
            b.len() >= HEIGHTS_HEADER_LEN,
            "height grid shorter than its {HEIGHTS_HEADER_LEN}-byte header"
        );
        if &b[0..8] != HEIGHTS_MAGIC {
            if &b[0..5] == HEIGHTS_MAGIC_PREFIX {
                bail!(
                    "this height grid was baked by an older playerbots ({}); re-run `playerbots bake`",
                    String::from_utf8_lossy(&b[0..8])
                );
            }
            bail!("bad height grid magic");
        }
        let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let f32_at = |o: usize| f32::from_bits(u32_at(o));
        Ok(Self {
            width: u32_at(8),
            height: u32_at(12),
            origin_x: f32_at(16),
            origin_y: f32_at(20),
            step: f32_at(24),
            scale: f32_at(28),
            offset: f32_at(32),
            flags: u32_at(36),
        })
    }

    /// Number of samples (`width * height`).
    pub fn sample_count(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// Writes a height grid file (header + row-major u16 samples).
pub fn write_height_grid(
    path: &Path,
    header: &HeightGridHeader,
    samples: &[u16],
) -> anyhow::Result<()> {
    ensure!(
        samples.len() == header.sample_count(),
        "height grid has {} samples, header says {}x{}",
        samples.len(),
        header.width,
        header.height
    );
    let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut out = BufWriter::with_capacity(1 << 20, file);
    out.write_all(&header.to_bytes())?;
    // Convert in chunks so a big grid does not need a second copy in memory.
    let mut buf = Vec::with_capacity(1 << 16);
    for chunk in samples.chunks(1 << 15) {
        buf.clear();
        for s in chunk {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        out.write_all(&buf)?;
    }
    out.flush()?;
    Ok(())
}

enum Samples {
    Mapped(Mmap),
    Owned(Vec<u16>),
}

/// Read-only, memory-mapped height grid.
pub struct Terrain {
    pub header: HeightGridHeader,
    samples: Samples,
}

impl Terrain {
    /// Maps `path` read-only and validates magic and size.
    pub fn open(path: &Path) -> anyhow::Result<Terrain> {
        let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: the map is read-only and the file is a private data file; if another process
        // truncated it while mapped the OS would fault, which is accepted for baked data.
        let map =
            unsafe { Mmap::map(&file) }.with_context(|| format!("mmap {}", path.display()))?;
        let header =
            HeightGridHeader::from_bytes(&map).with_context(|| format!("{}", path.display()))?;
        ensure!(
            header.width > 0 && header.height > 0,
            "{}: empty height grid ({}x{})",
            path.display(),
            header.width,
            header.height
        );
        ensure!(
            header.step.is_finite() && header.step > 0.0,
            "{}: bad step {}",
            path.display(),
            header.step
        );
        let expect = HEIGHTS_HEADER_LEN as u64 + header.sample_count() as u64 * 2;
        if (map.len() as u64) < expect {
            bail!(
                "{}: truncated height grid ({} bytes, expected {expect})",
                path.display(),
                map.len()
            );
        }
        Ok(Terrain {
            header,
            samples: Samples::Mapped(map),
        })
    }

    /// An in-memory grid (synthetic terrain for tests and flat fallbacks).
    pub fn from_samples(header: HeightGridHeader, samples: Vec<u16>) -> anyhow::Result<Terrain> {
        ensure!(
            samples.len() == header.sample_count(),
            "height grid has {} samples, header says {}x{}",
            samples.len(),
            header.width,
            header.height
        );
        ensure!(
            header.width > 0 && header.height > 0,
            "empty height grid ({}x{})",
            header.width,
            header.height
        );
        Ok(Terrain {
            header,
            samples: Samples::Owned(samples),
        })
    }

    /// Raw sample at column `col` (x) and row `row` (y); `None` outside the grid.
    pub fn raw(&self, col: u32, row: u32) -> Option<u16> {
        if col >= self.header.width || row >= self.header.height {
            return None;
        }
        let i = row as usize * self.header.width as usize + col as usize;
        Some(match &self.samples {
            Samples::Mapped(m) => {
                let o = HEIGHTS_HEADER_LEN + i * 2;
                u16::from_le_bytes([m[o], m[o + 1]])
            }
            Samples::Owned(v) => v[i],
        })
    }

    fn metres(&self, raw: u16) -> f32 {
        raw as f32 * self.header.scale + self.header.offset
    }

    /// Bilinear terrain height at world (x, y); `None` outside the grid.
    pub fn height(&self, x: f32, y: f32) -> Option<f32> {
        let h = &self.header;
        let fx = (x - h.origin_x) / h.step;
        let fy = (y - h.origin_y) / h.step;
        let max_x = (h.width - 1) as f32;
        let max_y = (h.height - 1) as f32;
        // Written so that NaN fails the test.
        if !(fx >= 0.0 && fx <= max_x && fy >= 0.0 && fy <= max_y) {
            return None;
        }
        let c0 = (fx.floor() as u32).min(h.width - 1);
        let r0 = (fy.floor() as u32).min(h.height - 1);
        let c1 = (c0 + 1).min(h.width - 1);
        let r1 = (r0 + 1).min(h.height - 1);
        let tx = fx - c0 as f32;
        let ty = fy - r0 as f32;
        let h00 = self.metres(self.raw(c0, r0)?);
        let h10 = self.metres(self.raw(c1, r0)?);
        let h01 = self.metres(self.raw(c0, r1)?);
        let h11 = self.metres(self.raw(c1, r1)?);
        let top = h00 + (h10 - h00) * tx;
        let bottom = h01 + (h11 - h01) * tx;
        Some(top + (bottom - top) * ty)
    }

    /// (min_x, min_y, max_x, max_y) covered by the grid.
    pub fn bounds(&self) -> (f32, f32, f32, f32) {
        let h = &self.header;
        (
            h.origin_x,
            h.origin_y,
            h.origin_x + h.step * (h.width.saturating_sub(1)) as f32,
            h.origin_y + h.step * (h.height.saturating_sub(1)) as f32,
        )
    }
}

/// Scratch directory for tests: below `$AA_TEST_TMP` when set (a roomier volume), else the OS
/// temp dir.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    let base = match std::env::var_os("AA_TEST_TMP") {
        Some(root) => std::path::PathBuf::from(root).join("tmp-test"),
        None => std::env::temp_dir().join("aa-world-test"),
    };
    let dir = base.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u32, h: u32) -> HeightGridHeader {
        HeightGridHeader {
            width: w,
            height: h,
            origin_x: 100.0,
            origin_y: 200.0,
            step: 4.0,
            scale: 1.0 / 64.0,
            offset: 0.0,
            flags: 1,
        }
    }

    /// 3x2 grid: row 0 = [0, 64, 128] raw (0, 1, 2 m); row 1 = [64, 128, 640] raw (1, 2, 10 m).
    fn small_samples() -> Vec<u16> {
        vec![0, 64, 128, 64, 128, 640]
    }

    #[test]
    fn write_open_roundtrip() {
        let dir = test_dir("terrain-roundtrip");
        let path = dir.join("heights_test.bin");
        let hdr = header(3, 2);
        write_height_grid(&path, &hdr, &small_samples()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            (HEIGHTS_HEADER_LEN + 6 * 2) as u64
        );
        let t = Terrain::open(&path).unwrap();
        assert_eq!(t.header, hdr);
        assert_eq!(t.raw(2, 1), Some(640));
        assert_eq!(t.raw(3, 0), None);
        assert_eq!(t.raw(0, 2), None);
        assert_eq!(t.bounds(), (100.0, 200.0, 108.0, 204.0));
        drop(t);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bilinear_values() {
        let t = Terrain::from_samples(header(3, 2), small_samples()).unwrap();
        let near = |a: Option<f32>, b: f32| {
            let a = a.expect("inside the grid");
            assert!((a - b).abs() < 1e-4, "{a} != {b}");
        };
        // exactly on samples
        near(t.height(100.0, 200.0), 0.0);
        near(t.height(104.0, 200.0), 1.0);
        near(t.height(108.0, 204.0), 10.0);
        // between samples
        near(t.height(102.0, 200.0), 0.5);
        near(t.height(100.0, 202.0), 0.5);
        near(t.height(102.0, 202.0), 1.0);
        near(t.height(106.0, 202.0), (2.0 + 1.0 + 2.0 + 10.0) / 4.0);
        // upper edges are inside
        near(t.height(108.0, 200.0), 2.0);
        near(t.height(104.0, 204.0), 2.0);
    }

    #[test]
    fn out_of_bounds_is_none() {
        let t = Terrain::from_samples(header(3, 2), small_samples()).unwrap();
        assert_eq!(t.height(99.9, 200.0), None);
        assert_eq!(t.height(100.0, 199.9), None);
        assert_eq!(t.height(108.1, 200.0), None);
        assert_eq!(t.height(100.0, 204.1), None);
        assert_eq!(t.height(f32::NAN, 200.0), None);
        assert_eq!(t.height(100.0, f32::INFINITY), None);
    }

    #[test]
    fn scale_and_offset_apply() {
        let mut hdr = header(2, 1);
        hdr.offset = -10.0;
        let t = Terrain::from_samples(hdr, vec![640, 1280]).unwrap();
        assert_eq!(t.height(100.0, 200.0), Some(0.0));
        assert_eq!(t.height(104.0, 200.0), Some(10.0));
    }

    #[test]
    fn rejects_bad_files() {
        let dir = test_dir("terrain-bad");
        // wrong magic
        let p = dir.join("magic.bin");
        std::fs::write(&p, vec![0u8; 100]).unwrap();
        assert!(Terrain::open(&p).is_err());
        // too short for the header
        let p = dir.join("short.bin");
        std::fs::write(&p, HEIGHTS_MAGIC).unwrap();
        assert!(Terrain::open(&p).is_err());
        // truncated samples
        let p = dir.join("trunc.bin");
        write_height_grid(&p, &header(3, 2), &small_samples()).unwrap();
        let len = std::fs::metadata(&p).unwrap().len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(len - 2)
            .unwrap();
        assert!(Terrain::open(&p).is_err());
        // missing file
        assert!(Terrain::open(&dir.join("missing.bin")).is_err());
        // sample count mismatch on write
        assert!(write_height_grid(&dir.join("w.bin"), &header(3, 2), &[1, 2, 3]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_old_bake_is_rejected_with_a_rebake_hint() {
        let dir = test_dir("terrain-old-bake");
        let path = dir.join("heights_main_world.bin");
        write_height_grid(&path, &header(3, 2), &small_samples()).unwrap();
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all(b"PBHGT001").unwrap();
        drop(f);

        let Err(e) = Terrain::open(&path) else {
            panic!("an old bake was accepted");
        };
        let msg = format!("{e:#}");
        assert!(msg.contains("older playerbots"), "{msg}");
        assert!(msg.contains("PBHGT001"), "{msg}");
        assert!(msg.contains("`playerbots bake`"), "{msg}");
        assert!(msg.contains(&path.display().to_string()), "{msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
