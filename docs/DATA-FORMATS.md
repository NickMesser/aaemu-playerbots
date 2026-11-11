# Baked data formats

Written by `aa-bake` (`playerbots bake`), read by `aa-world`. All integers little-endian.
World coordinates: metres, x east, y north, z up; `main_world` spans 35×38 cells of 1024 m
(0..35840 × 0..38912), sea level 100 m.

## `heights_<world>.bin` — terrain height grid

The magic doubles as the format version: `PBHGT002` since low-resolution sector nodes are
resampled like the original client. Files baked with `PBHGT001` hold the old C# heights and are
rejected with a re-bake hint: existing installs must re-run `playerbots bake`.

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `[u8; 8]` | magic `PBHGT002` |
| 8 | u32 | `width` — samples along x |
| 12 | u32 | `height` — samples along y |
| 16 | f32 | `origin_x` — world x of sample column 0 |
| 20 | f32 | `origin_y` — world y of sample row 0 |
| 24 | f32 | `step` — metres between samples (default 4.0; the server's native grid is 2.0) |
| 28 | f32 | `scale` — metres per raw unit (1/32, so heights up to 2047.97 m fit; the server itself uses 1/64) |
| 32 | f32 | `offset` — metres added after scaling (0.0) |
| 36 | u32 | `flags` — bit 0: cells without terrain hold raw 0 |
| 40 | `[u8; 24]` | reserved, zero |
| 64 | `[u16; width*height]` | samples, row-major: `index = row * width + col`, row = y, col = x |

Height at (x, y): `fx = (x - origin_x) / step`, `fy = (y - origin_y) / step`, bilinear blend of
the 4 surrounding samples, `h = raw * scale + offset`. Outside the grid: `None`.
`aa-world` memory-maps the file, so only the pages around bots become resident.

## `map_<world>.png` + `map_<world>.json` — world map image and calibration

```json
{
  "image": "map_main_world.png",
  "width": 1856,
  "height": 1112,
  "source": "painted",            // "painted" (client ui/map DDS) or "relief" (from heights)
  "world": { "min_x": -14731.0, "min_y": 46.0, "max_x": 49920.0, "max_y": 38911.0 }
}
```

Pixel (0,0) is the top-left corner = world (`min_x`, `max_y`).
`u = (x - min_x) / (max_x - min_x)`, `v = 1 - (y - min_y) / (max_y - min_y)`, pixel = (u·width, v·height).

## `housing_areas.json` — housing area polygons

```json
[ { "id": 52, "zone_key": 179, "points": [[13900.0, 14180.0], [13990.0, 14180.0], ...] } ]
```

`id` is `housing_areas.id` in compact.sqlite3 (`value1` in the client's `housing_area.xml`);
points are world x/y. `aa-world` joins group/category rules from compact.

## `farm_areas.json` — public farm polygons

```json
[ { "id": 966, "zone_key": 195, "points": [[12520.0, 15370.0], [12570.0, 15370.0], ...] } ]
```

Same layout as `housing_areas.json`, from the client's `level_design/zone/<zone_key>/client/subzone_area.xml`
(the area's own `Id`, the `sub_zones.id`; points as for the housing areas). Only the five sub zones the
server hard-codes as public farms are kept (`PublicFarmManager.Load`): 966 and 998 field farms, 968
nurseries, 967 ranches, 974 stables. A character plants for free (no labor) inside them, up to 10
plots on a field farm and 5 on the other kinds. The file is optional: without it the bots plant on
their own land only. Written by the full bake or alone by
`cargo run -p aa-bake --release --example bake -- --pak <game_pak> --out <dir> --compact <compact.sqlite3> --only farms`.

## `quest_spheres.json` — quest sign spheres

```json
[ { "quest": 1234, "component": 5678, "x": 15000.0, "y": 15000.0, "z": 120.0, "radius": 10.0 } ]
```

From `level_design/zone/<zone_key>/client/quest_sign_sphere.g`; world position =
zone origin cell × 1024 + local position.

## `zones_<world>.bin` — zone key per 64 m sector (optional)

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `[u8; 8]` | magic `PBZON001` |
| 8 | u32 | `width` (sectors along x) |
| 12 | u32 | `height` (sectors along y) |
| 16 | f32 | `sector` — metres per sector (64.0) |
| 20 | `[u8; 12]` | reserved |
| 32 | `[u16; width*height]` | zone key, row-major (`row * width + col`), 0 = none |

## `nav_<world>.bin` — navigation mesh (path finding)

Baked from the client's `.bai` navigation meshes (`game/worlds/<w>/paths/<bx>_<by>/{net,verts}mission0.bai`
for `main_world`, `game/worlds/<w>/zone/<key>/…` for instance worlds) by `aa-bake/src/nav`,
memory-mapped by `aa_world::nav::NavMesh`. One file per world: `nav_main_world.bin` plus one per
instance world (`nav_instance_nachashgar.bin`, `nav_arche_mall_world.bin`, …).

The bake merges the 256 m blocks (border copies of a vertex / node are the same when their x/y
is closer than 1e-4 m, nodes also need the same navType), drops blocked links (radius -1) and links
narrower than 0.4 m, drops weakly connected islands smaller than 50 nodes (4 in instance worlds) and
computes the strongly connected components for agents of 0.5 m. Nodes are the triangle centroids
(navType 2) and the human waypoints (navType 4: building interiors, stairs, bridges); the dummy
nodes at the origin of empty blocks are dropped. Nodes are stored in Z-order (Morton) of their
position so that neighbours are close in memory.

Header (64 bytes), then the section table, then the sections (each 64-byte aligned):

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `[u8; 8]` | magic `PBNAV001` |
| 8 | u32 | format version (1) |
| 12 | u32 | `nodes` N |
| 16 | u32 | `vertices` V |
| 20 | u32 | `links` L |
| 24 | u32 | `components` C |
| 28 | u32 | `sections` (15) |
| 32 | f32 ×4 | bounds `min_x, min_y, max_x, max_y` of all nodes and vertices |
| 48 | f32 | grid cell size in metres (8 .. 256; 32 for `main_world`) |
| 52 | u32 | grid width in cells |
| 56 | u32 | grid height in cells |
| 60 | u8 | `comp_radius`: the pass radius the component tables are exact for, in 0.05 m units (10 = 0.5 m) |
| 61 | `[u8; 3]` | reserved |

Section table, `sections` entries of 24 bytes starting at offset 64: `u32 id`, `u32 element size`,
`u64 byte offset` (multiple of 4, in practice 64), `u64 element count`. The reader checks element
size and count against the header and that the section lies inside the file; unknown ids are
skipped, missing required ids are an error.

| Id | Section | Element | Count | Content |
| --- | --- | --- | --- | --- |
| 1 | node positions | `[f32; 3]` | N | triangle centroid or waypoint position (world metres, z absolute) |
| 2 | node triangles | `[u32; 3]` | N | vertex indices of the triangle; `0xFFFFFFFF` ×3 for waypoints and nodes without usable geometry |
| 3 | node flags | u8 | N | bit 0 waypoint, bit 1 road (the triangle lies under a road prefab or carriage route) |
| 4 | node components | u32 | N | strongly connected component of the node; 0 is the largest, then by size |
| 5 | vertices | `[f32; 3]` | V | triangle corners (world metres) |
| 6 | adjacency offsets | u32 | N + 1 | CSR: the links of node `i` are `off[i] .. off[i+1]` |
| 7 | link targets | u32 | L | target node |
| 8 | link radii | u8 | L | widest agent radius that may pass, in 0.05 m units (floor); never below 8 (0.4 m); the radius is directional, the reverse link has its own |
| 9 | link costs | u16 | L | walking length in 2 cm units: node → middle of the shared edge → node for two triangles (2D), the straight 3D distance otherwise |
| 10 | component sizes | u32 | C | nodes per strongly connected component |
| 11 | component wcc | u32 | C | weakly connected component (over links ≥ 0.4 m) of each strongly connected component |
| 12 | component order | u32 | C | completion index of the component in the SCC search; **a link never leads to a component with a higher order**, so a goal whose component has a higher order than the start's is unreachable |
| 13 | grid offsets | u32 | W·H + 1 | CSR over the grid cells (row-major, `row = y`, `col = x`, origin `min_x, min_y`) |
| 14 | grid items | u32 | any | node ids per cell: a triangle in every cell it touches (padded, conservative), a waypoint in the cell of its position |
| 15 | vertex flags | u8 | V | bit 0 border vertex: an end of a triangle edge that no link of at least 0.4 m crosses (obstacle or mesh edge); the path smoother keeps the agent radius away from these only |

Road flags (`main_world` only): the bake reads the road prefabs (object type 13: paired left / right
edge points, cell-local) of every `cells/<cx>_<cy>/client/object.dat` and the carriage routes of
`level_design/zone/<id>/client/transfer_path.xml` (zone-local, only routes of which 60 % lies on the
mesh: the file also holds air and ship routes) and sets bit 1 of every triangle that a road strip
passes over within 6 m vertically. Path finding multiplies the cost of links between two road nodes
by `PathQuery::road_bias` (default 0.75).

Derived at run time, not stored: the portal (shared edge) of two linked triangles is the two vertex
indices they have in common (links between triangles without exactly two common vertices are
passed through the node position); point-in-triangle and plane z come from the three vertices.

Reading: nothing is copied. `NavMesh::open` checks the header and the section table; every value
read from the map that acts as an index is range checked where it is used, so a damaged file can
give wrong answers but not panics (`NavMesh::verify` scans everything). Size of `main_world`:
see the bake report (about 200 MB), instance worlds a few MB each.
