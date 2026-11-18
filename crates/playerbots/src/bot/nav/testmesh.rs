//! Synthetic navigation meshes for the tests (not compiled outside them): a grid of square cells,
//! two triangles each, some cells left out as walls.

use std::collections::HashMap;
use std::sync::Arc;

use aa_world::nav::format::{radius_to_units, NO_TRI};
use aa_world::nav::{NavData, NavMesh};

use super::planner::NavWorld;

/// The flat height of the grid.
pub const FLOOR_Z: f32 = 120.0;

/// A `w` x `h` grid of cells of `cell` metres with its corner at (x0, y0); `wall(cx, cy)` leaves a
/// cell out. Every pair of triangles that share an edge is linked both ways (wide enough for any
/// agent); the connected pieces are the components.
pub fn grid(
    (x0, y0): (f32, f32),
    (w, h): (usize, usize),
    cell: f32,
    wall: impl Fn(usize, usize) -> bool,
) -> NavMesh {
    let mut d = NavData::default();
    let mut vert: HashMap<(usize, usize), u32> = HashMap::new();
    let mut v = |d: &mut NavData, i: usize, j: usize| -> u32 {
        *vert.entry((i, j)).or_insert_with(|| {
            d.vertices
                .push([x0 + i as f32 * cell, y0 + j as f32 * cell, FLOOR_Z]);
            (d.vertices.len() - 1) as u32
        })
    };
    for cy in 0..h {
        for cx in 0..w {
            if wall(cx, cy) {
                continue;
            }
            let (a, b, c, e) = (
                v(&mut d, cx, cy),
                v(&mut d, cx + 1, cy),
                v(&mut d, cx + 1, cy + 1),
                v(&mut d, cx, cy + 1),
            );
            for tri in [[a, b, c], [a, c, e]] {
                let p = tri.map(|i| d.vertices[i as usize]);
                d.node_pos.push([
                    (p[0][0] + p[1][0] + p[2][0]) / 3.0,
                    (p[0][1] + p[1][1] + p[2][1]) / 3.0,
                    FLOOR_Z,
                ]);
                d.node_tri.push(tri);
                d.node_flags.push(0);
            }
        }
    }
    let n = d.node_pos.len();
    // links over shared edges
    let mut edges: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (i, t) in d.node_tri.iter().enumerate() {
        assert_ne!(t[0], NO_TRI);
        for k in 0..3 {
            let (p, q) = (t[k], t[(k + 1) % 3]);
            edges.entry((p.min(q), p.max(q))).or_default().push(i);
        }
    }
    let mut links: Vec<(usize, usize)> = Vec::new();
    for nodes in edges.values() {
        if let [a, b] = nodes[..] {
            links.push((a, b));
            links.push((b, a));
        }
    }
    links.sort_unstable();
    links.dedup();
    let mut off = vec![0u32; n + 1];
    for l in &links {
        off[l.0 + 1] += 1;
    }
    for i in 0..n {
        off[i + 1] += off[i];
    }
    d.adj_off = off;
    d.adj_target = links.iter().map(|l| l.1 as u32).collect();
    d.adj_radius = vec![radius_to_units(3.0); links.len()];
    // components: the connected pieces (links are symmetric)
    let mut comp = vec![usize::MAX; n];
    let mut sizes = Vec::new();
    for s in 0..n {
        if comp[s] != usize::MAX {
            continue;
        }
        let id = sizes.len();
        let mut stack = vec![s];
        comp[s] = id;
        let mut size = 0u32;
        while let Some(a) = stack.pop() {
            size += 1;
            for &target in &d.adj_target[d.adj_off[a] as usize..d.adj_off[a + 1] as usize] {
                let b = target as usize;
                if comp[b] == usize::MAX {
                    comp[b] = id;
                    stack.push(b);
                }
            }
        }
        sizes.push(size);
    }
    d.node_comp = comp.iter().map(|c| *c as u32).collect();
    d.comp_wcc = (0..sizes.len() as u32).collect();
    d.comp_order = (0..sizes.len() as u32).collect();
    d.comp_size = sizes;
    d.comp_radius = radius_to_units(0.5);
    d.compute_bounds();
    d.compute_costs();
    d.compute_vertex_flags(radius_to_units(0.4));
    d.build_grid(8.0).unwrap();
    d.validate().unwrap();
    NavMesh::from_bytes(d.to_bytes().unwrap()).unwrap()
}

/// A world with one mesh for every name and a flat terrain at [`FLOOR_Z`].
pub struct FlatWorld {
    pub mesh: Arc<NavMesh>,
    /// Terrain height; `None` = no terrain.
    pub terrain: Option<f32>,
}

impl FlatWorld {
    pub fn new(mesh: NavMesh) -> Arc<FlatWorld> {
        Arc::new(FlatWorld {
            mesh: Arc::new(mesh),
            terrain: Some(FLOOR_Z),
        })
    }
}

impl NavWorld for FlatWorld {
    fn mesh(&self, _name: &str) -> Option<Arc<NavMesh>> {
        Some(self.mesh.clone())
    }

    fn ground(&self, _x: f32, _y: f32) -> Option<f32> {
        self.terrain
    }
}
