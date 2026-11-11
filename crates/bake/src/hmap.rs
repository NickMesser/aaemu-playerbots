//! Reader for the client's per-cell `client/terrain/heightmap.dat`.
//!
//! Ported from the AAEmu Rust server (`aaemu-game/src/systems/npcs/heightmap.rs`, `hmap` module and
//! `load_cell_height_map`; GPL-3.0-or-later), which is a port of the C# `Hmap` / `NodeCell` /
//! `AABB`. Low-resolution nodes are resampled with the original 1.2 client's rule (see
//! [`NodeCell::get_height`]), matching the AAEmu server since AAEmu/AAEmu-1.2-Rust PR #56. The
//! check against the original 1.2 client's terrain height at 12 points showed the C# `UpScale`
//! was wrong (off by up to ~25 m; the client rule is within 0.1 m), so the sample decode is kept
//! bit-exact but the upscale quirk is not.
//!
//! A cell is 1024 m wide and made of 16 x 16 sectors of 64 m. Each sector is a quadtree node with
//! `n_size` x `n_size` u16 samples (33 x 33 for full detail); the server reads 32 x 32 samples of
//! each sector, i.e. a 512 x 512 grid at 2 m per cell.

use std::cmp::Ordering;

use anyhow::bail;

/// Samples per cell side (2 m grid).
pub const CELL_SAMPLES: usize = 512;
/// Sectors per cell side.
pub const SECTORS_PER_CELL: usize = 16;
/// Samples per sector side used by the server.
pub const SECTOR_SAMPLES: usize = 32;

const INV_5CM: i32 = 20;
const MASK_12BIT: u32 = (1 << 12) - 1;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> anyhow::Result<[u8; N]> {
        let Some(s) = self.data.get(self.pos..self.pos + N) else {
            bail!("unable to read beyond the end of the stream");
        };
        self.pos += N;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> anyhow::Result<u16> {
        Ok(u16::from_le_bytes(self.take()?))
    }
    fn i32(&mut self) -> anyhow::Result<i32> {
        Ok(i32::from_le_bytes(self.take()?))
    }
    fn f32(&mut self) -> anyhow::Result<f32> {
        Ok(f32::from_le_bytes(self.take()?))
    }
    /// `BinaryReader.ReadBytes(count)`: fewer bytes at the end of the stream is not an error.
    fn skip(&mut self, count: i32) -> anyhow::Result<()> {
        if count < 0 {
            bail!("negative skip {count}");
        }
        self.pos = (self.pos + count as usize).min(self.data.len());
        Ok(())
    }
}

/// One quadtree node (one 64 m sector).
struct NodeCell {
    version: u8,
    min: [f32; 3],
    f_offset: f32,
    f_range: f32,
    n_size: i32,
    data: Vec<u16>,
    i_offset: i32,
    i_step: i32,
    f_min: f32,
}

impl NodeCell {
    fn read(br: &mut Reader<'_>) -> anyhow::Result<Self> {
        let version = br.u8()?;
        br.u8()?; // dummy
        br.u8()?; // flags
        br.u8()?; // flags2
        let min = [br.f32()?, br.f32()?, br.f32()?];
        for _ in 0..3 {
            br.f32()?; // box max
        }
        br.u8()?; // has holes
        let f_offset = br.f32()?;
        let f_range = br.f32()?;
        let n_size = br.i32()?;
        let len = n_size.wrapping_mul(n_size);
        if len < 0 {
            bail!("negative node size {n_size}");
        }
        let unk_count = br.i32()?;
        if (len as usize).saturating_mul(2) > br.data.len() - br.pos {
            bail!("node data past the end of the stream");
        }
        let data = (0..len).map(|_| br.u16()).collect::<Result<Vec<_>, _>>()?;
        br.i32()?;
        for _ in 0..4 {
            br.f32()?;
        }
        br.skip(36i32.wrapping_add(unk_count))?;

        let mut n = NodeCell {
            version,
            min,
            f_offset,
            f_range,
            n_size,
            data,
            i_offset: 0,
            i_step: 1,
            f_min: 0.0,
        };
        n.init();
        if n.version < 7 {
            n.rescale_to_int();
        }
        Ok(n)
    }

    fn init(&mut self) {
        self.f_min = self.f_offset;
        let f_max = self.f_min + 0xFFF0 as f32 * self.f_range;
        self.i_offset = (self.f_min * INV_5CM as f32) as i32;
        let i_range = ((f_max - self.f_min) * INV_5CM as f32) as i32;
        self.i_step = if i_range > 0 {
            (i64::from(i_range) + i64::from(MASK_12BIT) - 1) / i64::from(MASK_12BIT)
        } else {
            1
        } as i32;
    }

    fn rescale_to_int(&mut self) {
        for i in 0..self.data.len() {
            let hraw = self.data[i];
            let height = self.f_min + (0xFFF0 & i32::from(hraw)) as f32 * self.f_range;
            let scaled = ((height - self.f_min) * INV_5CM as f32) as i32;
            let hdec = scaled.wrapping_div(self.i_step.max(1)) as u16;
            let res = (i32::from(hraw) & 0xF) | (i32::from(hdec) << 4);
            self.data[i] = res as u16;
        }
    }

    fn lerp(s: f32, e: f32, t: f32) -> f32 {
        s + (e - s) * t
    }

    /// `RawDataToHeight`.
    fn raw_data_to_height(&self, data: u32) -> f32 {
        0.05f32 * self.i_offset as f32
            + (i64::from(data >> 4) * i64::from(self.i_step)) as f32 * 0.05f32
    }

    /// `GetHeight(nX, nY)` in metres. A full-resolution node holds 33 x 33 samples (x-major). A
    /// low-resolution node keeps its `size x size` samples and is resampled on read like the
    /// original 1.2 client, as in the AAEmu server's `NodeCell::get_height`
    /// (`rust/crates/aaemu-game/src/systems/npcs/heightmap.rs`): target `i` maps to source
    /// `i * (size - 1) / 32`, bilinear over the node's own samples in f32, with the upper index
    /// clamped to `size - 2` (the far edge is the last sample at fraction 1). The C# upscaled
    /// the node up front and then read the 33-wide result with the wrong stride (it reused
    /// `n_size`), so low-resolution sectors came out mirrored. A 1 x 1 node (not present in the
    /// 1.2 data; the client treats it as invalid) is our choice to cover the sector with its one
    /// height.
    fn get_height(&self, n_x: u16, n_y: u16) -> Option<f32> {
        if self.data.len() == 33 * 33 {
            let index = i32::from(n_x).wrapping_mul(33).wrapping_add(i32::from(n_y));
            let raw = *self.data.get(index as u32 as usize)?;
            return Some(self.raw_data_to_height(u32::from(raw)));
        }
        if self.n_size <= 0 {
            return Some(0.0);
        }
        if n_x > 32 || n_y > 32 {
            return None;
        }
        if self.n_size == 1 {
            return Some(self.raw_data_to_height(u32::from(self.data[0])));
        }
        let n_size = self.n_size as usize;
        if self.data.len() != n_size * n_size {
            return None;
        }
        let source_scale = (n_size - 1) as f32 / 32.0;
        let (fx, fy) = (f32::from(n_x) * source_scale, f32::from(n_y) * source_scale);
        let (ix, iy) = (
            (fx.floor() as usize).min(n_size - 2),
            (fy.floor() as usize).min(n_size - 2),
        );
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let h = |x: usize, y: usize| self.raw_data_to_height(u32::from(self.data[x * n_size + y]));
        let top = Self::lerp(h(ix, iy), h(ix + 1, iy), tx);
        let bottom = Self::lerp(h(ix, iy + 1), h(ix + 1, iy + 1), tx);
        Some(Self::lerp(top, bottom, ty))
    }
}

/// `Comparer<float>.Default`: NaN sorts before everything else.
fn dotnet_cmp_f32(a: f32, b: f32) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Decodes one `heightmap.dat` into the cell's 512 x 512 height grid in metres, stored
/// `[x * 512 + y]` (x east, y north, 2 m apart, cell-local), as the server's `WorldCell` does.
///
/// Errors when the file is malformed or has fewer than 256 sector nodes (the server fails the
/// cell load in both cases).
pub fn decode_cell(data: &[u8]) -> anyhow::Result<Vec<f32>> {
    let mut br = Reader { data, pos: 0 };
    let version = br.u8()?;
    br.u8()?; // dummy
    br.u8()?; // flags
    br.u8()?; // flags2
    let chunk_size = br.i32()?;
    br.i32()?; // height map size in units
    br.i32()?; // unit size in metres
    br.i32()?; // sector size in metres
    br.i32()?; // sectors table size in sectors
    br.f32()?; // height map z ratio
    br.f32()?; // ocean water level
    if version >= 24 {
        br.skip(128)?;
    }
    let mut nodes = Vec::new();
    while i64::try_from(br.pos).unwrap_or(i64::MAX) != i64::from(chunk_size) {
        nodes.push(NodeCell::read(&mut br)?);
    }
    // OrderBy(Min.X).ThenBy(Min.Y).Where(data.Length > 0)
    let mut sorted: Vec<&NodeCell> = nodes.iter().filter(|n| !n.data.is_empty()).collect();
    sorted.sort_by(|a, b| {
        dotnet_cmp_f32(a.min[0], b.min[0]).then_with(|| dotnet_cmp_f32(a.min[1], b.min[1]))
    });
    if sorted.len() < SECTORS_PER_CELL * SECTORS_PER_CELL {
        bail!("only {} heightmap nodes", sorted.len());
    }
    let mut heights = vec![0f32; CELL_SAMPLES * CELL_SAMPLES];
    for sector_x in 0..SECTORS_PER_CELL {
        for sector_y in 0..SECTORS_PER_CELL {
            let node = sorted[sector_x * SECTORS_PER_CELL + sector_y];
            for unit_x in 0..SECTOR_SAMPLES {
                for unit_y in 0..SECTOR_SAMPLES {
                    let Some(h) = node.get_height(unit_x as u16, unit_y as u16) else {
                        bail!("height index out of range in sector ({sector_x}, {sector_y})");
                    };
                    let o_x = sector_x * SECTOR_SAMPLES + unit_x;
                    let o_y = sector_y * SECTOR_SAMPLES + unit_y;
                    heights[o_x * CELL_SAMPLES + o_y] = h;
                }
            }
        }
    }
    Ok(heights)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes before the first node: 4 flags, chunk size, 4 i32, 2 f32 (+ 128 from version 24).
    fn header_len(version: u8) -> usize {
        4 + 4 + 16 + 8 + if version >= 24 { 128 } else { 0 }
    }

    /// Serialises a version-7+ node with `n_size` x `n_size` samples.
    fn node_bytes(
        min_x: f32,
        min_y: f32,
        f_offset: f32,
        f_range: f32,
        n_size: i32,
        raw: &dyn Fn(i32, i32) -> u16,
    ) -> Vec<u8> {
        let mut b = vec![8u8, 0, 0, 0]; // version 8
        for v in [min_x, min_y, 0.0, min_x + 64.0, min_y + 64.0, 100.0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(0); // has holes
        b.extend_from_slice(&f_offset.to_le_bytes());
        b.extend_from_slice(&f_range.to_le_bytes());
        b.extend_from_slice(&n_size.to_le_bytes());
        b.extend_from_slice(&0i32.to_le_bytes()); // unk count
        for x in 0..n_size {
            for y in 0..n_size {
                b.extend_from_slice(&raw(x, y).to_le_bytes());
            }
        }
        b.extend_from_slice(&0i32.to_le_bytes());
        for _ in 0..4 {
            b.extend_from_slice(&0f32.to_le_bytes());
        }
        b.extend_from_slice(&[0u8; 36]);
        b
    }

    /// 256 sector nodes (shuffled order): sample (x, y) of sector (sx, sy) stores a height that
    /// encodes the global sample coordinates, so the 512 x 512 assembly can be checked.
    fn cell_bytes(version: u8) -> Vec<u8> {
        let mut body = Vec::new();
        // f_offset 0, f_range such that raw>>4 steps are 1 unit of 5 cm: i_range = 0xFFF0 * f_range * 20
        // choose f_range = 1/20/16 -> i_range = 0xFFF0/16 = 4095 -> i_step = 1
        let f_range = 1.0 / 20.0 / 16.0;
        let mut order: Vec<(i32, i32)> =
            (0..16).flat_map(|x| (0..16).map(move |y| (x, y))).collect();
        order.reverse();
        for (sx, sy) in order {
            body.extend(node_bytes(
                sx as f32 * 64.0,
                sy as f32 * 64.0,
                0.0,
                f_range,
                33,
                &|x, y| {
                    // height (in 5 cm units) = global sample x + 2 * global sample y mod 4000, raw = h << 4
                    let gx = sx * 32 + x;
                    let gy = sy * 32 + y;
                    (((gx + gy * 2) % 4000) as u16) << 4
                },
            ));
        }
        let mut file = vec![version, 0, 0, 0];
        let hdr_len = header_len(version);
        let chunk_size = (hdr_len + body.len()) as i32;
        file.extend_from_slice(&chunk_size.to_le_bytes());
        file.extend_from_slice(&512i32.to_le_bytes());
        file.extend_from_slice(&2i32.to_le_bytes());
        file.extend_from_slice(&64i32.to_le_bytes());
        file.extend_from_slice(&16i32.to_le_bytes());
        file.extend_from_slice(&1.0f32.to_le_bytes());
        file.extend_from_slice(&100.0f32.to_le_bytes());
        if version >= 24 {
            file.extend_from_slice(&[0u8; 128]);
        }
        file.extend_from_slice(&body);
        file
    }

    #[test]
    fn assembles_sectors_into_the_cell_grid() {
        for version in [20u8, 24] {
            let heights = decode_cell(&cell_bytes(version)).unwrap();
            assert_eq!(heights.len(), 512 * 512);
            for &(gx, gy) in &[
                (0usize, 0usize),
                (1, 0),
                (0, 1),
                (31, 31),
                (32, 0),
                (100, 300),
                (511, 511),
                (200, 511),
            ] {
                let expect = ((gx + gy * 2) % 4000) as f32 * 0.05;
                let got = heights[gx * 512 + gy];
                assert!(
                    (got - expect).abs() < 1e-3,
                    "v{version} ({gx},{gy}): {got} != {expect}"
                );
            }
        }
    }

    #[test]
    fn rejects_short_and_truncated_files() {
        let good = cell_bytes(24);
        assert!(decode_cell(&good[..good.len() - 10]).is_err());
        assert!(decode_cell(&good[..5]).is_err());
        // a cell with too few nodes
        let mut few = cell_bytes(24);
        let first_node_len = node_bytes(0.0, 0.0, 0.0, 0.01, 33, &|_, _| 0).len();
        let new_chunk = (few.len() - first_node_len) as i32;
        few.drain(header_len(24)..header_len(24) + first_node_len);
        few[4..8].copy_from_slice(&new_chunk.to_le_bytes());
        assert!(decode_cell(&few).is_err());
    }

    /// The original client's resampling rule, as in the AAEmu server's `NodeCell::get_height`
    /// (`rust/crates/aaemu-game/src/systems/npcs/heightmap.rs`), in raw `data >> 4` units:
    /// target `i` -> source `i * (size - 1) / 32`, bilinear over the node's own samples, upper
    /// index clamped to `size - 2` (the far edge is the last sample at fraction 1). Independent
    /// of the implementation (h-dec units, no height decode).
    fn client_upscaled_hdec(data: &[u16], size: usize, x: usize, y: usize) -> f32 {
        let s = (size - 1) as f32 / 32.0;
        let (fx, fy) = (x as f32 * s, y as f32 * s);
        let (ix, iy) = (
            (fx.floor() as usize).min(size - 2),
            (fy.floor() as usize).min(size - 2),
        );
        let (tx, ty) = (fx - ix as f32, fy - iy as f32);
        let h = |a: usize, b: usize| f32::from(data[a * size + b] >> 4);
        let top = h(ix, iy) * (1.0 - tx) + h(ix + 1, iy) * tx;
        let bottom = h(ix, iy + 1) * (1.0 - tx) + h(ix + 1, iy + 1) * tx;
        top * (1.0 - ty) + bottom * ty
    }

    #[test]
    fn low_resolution_nodes_resample_like_the_client() {
        // Sizes 2, 5, 9 and 17 (the low-resolution nodes of the 1.2 data). The node keeps its
        // own samples and `get_height` interpolates them in f32, so the only difference to the
        // rule is the source samples' existing 5 cm decode.
        for size in [2usize, 5, 9, 17] {
            let data: Vec<u16> = (0..size * size)
                .map(|i| ((i as u16 * 37 + 8) % 7 * 32) << 4)
                .collect();
            let mut b = Reader {
                data: &node_bytes(0.0, 0.0, 0.0, 0.0001, size as i32, &|x, y| {
                    data[x as usize * size + y as usize]
                }),
                pos: 0,
            };
            let n = NodeCell::read(&mut b).unwrap();
            assert_eq!(n.n_size, size as i32);
            // The node is not upscaled up front; the samples stay as read.
            assert_eq!(n.data, data);
            for x in 0..33usize {
                for y in 0..33usize {
                    let want = client_upscaled_hdec(&data, size, x, y) * 0.05;
                    let got = n.get_height(x as u16, y as u16).unwrap();
                    assert!(
                        (got - want).abs() <= 1e-3,
                        "size {size} height ({x}, {y}): {got}, rule {want}"
                    );
                }
            }
            // The sample lattice: every source sample maps to its own grid target.
            for i in 0..size {
                let target = (i * 32) / (size - 1);
                assert_eq!(
                    n.get_height(target as u16, 0).unwrap(),
                    f32::from(data[i * size] >> 4) * 0.05,
                    "size {size} lattice x {i}"
                );
                assert_eq!(
                    n.get_height(0, target as u16).unwrap(),
                    f32::from(data[i] >> 4) * 0.05,
                    "size {size} lattice y {i}"
                );
            }
            // The far edge is the last sample (fraction 1), not a duplicated inner row.
            assert_eq!(
                n.get_height(32, 32).unwrap(),
                f32::from(data[(size - 1) * size + size - 1] >> 4) * 0.05
            );
        }
    }
}
