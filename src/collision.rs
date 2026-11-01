//! Collision snapshots (CONTRACT section 9.5): the collision boxes of the blocks around a player as
//! triangles, counter-clockwise seen from outside (CONTRACT section 4).
//!
//! The server already holds each block state's collision shapes as boxes in block-local coordinates
//! (the `collision-shapes` of the plugin API's `block-state`); nothing is invented here. A [`Scan`]
//! reads the blocks one horizontal layer at a time, so a tick never reads more than it is given
//! budget for, and [`Scan::build`] turns the boxes into triangles:
//!
//! * a box face lying on the boundary of its block is left out when a full block sits behind it, and
//!   the faces of two adjacent full blocks that touch disappear on both sides, so solid ground costs
//!   only its top;
//! * a snapshot has at most `MAX_COLLISION_CHUNKS * MAX_TRIANGLES` triangles (section 9.5, "Size
//!   limit"); a bigger one keeps the triangles nearest to the centre and announces a smaller radius.
//!   Nearness is the distance from the centre to the triangle's bounding box, ties keep scan order.

use crate::mapping::Mapping;
use rewrite_bridge::link::{MAX_COLLISION_CHUNKS, MAX_TRIANGLES};
use std::rc::Rc;

/// The most triangles one snapshot may hold (4096 chunks of 226).
pub const MAX_SNAPSHOT_TRIANGLES: usize = MAX_COLLISION_CHUNKS as usize * MAX_TRIANGLES;

/// A triangle in link space.
pub type Tri = [[f32; 3]; 3];

/// What one block holds for collision.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// The block is one full cube: its faces hide the faces behind them.
    pub full: bool,
    /// Boxes in block-local coordinates, `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub boxes: Vec<[f64; 6]>,
}

impl Cell {
    /// Air, fluids and anything else without a collision box.
    pub fn empty() -> Rc<Self> {
        Rc::new(Self {
            full: false,
            boxes: Vec::new(),
        })
    }

    /// A full cube.
    pub fn cube() -> Rc<Self> {
        Rc::new(Self {
            full: true,
            boxes: vec![[0.0, 0.0, 0.0, 1.0, 1.0, 1.0]],
        })
    }
}

/// Where the cells come from: the server's world.
pub trait BlockSource {
    /// The collision of the block at these block coordinates, or `None` for an unloaded chunk.
    fn cell(&mut self, x: i32, y: i32, z: i32) -> Option<Rc<Cell>>;
}

/// An in-progress read of the cube of blocks around a centre block.
pub struct Scan {
    min: [i32; 3],
    side: usize,
    cells: Vec<Option<Rc<Cell>>>,
    next_layer: usize,
    centre: [f64; 3],
    radius_blocks: u32,
}

/// A finished snapshot, in link space.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub centre: [f32; 3],
    /// What the snapshot covers, in metres: the requested radius, or less when triangles were cut.
    pub radius: f32,
    pub triangles: Vec<Tri>,
    /// Triangles dropped to stay within the limit, 0 when none.
    pub dropped: usize,
    /// How many triangles there were before the cut.
    pub total: usize,
}

impl Scan {
    /// A scan of the blocks within `radius` blocks (a cube `2 * radius + 1` wide) of the block that
    /// holds `centre` (the player's position, in blocks).
    pub fn new(centre: [f64; 3], radius: u32) -> Self {
        let c = [
            centre[0].floor() as i32,
            centre[1].floor() as i32,
            centre[2].floor() as i32,
        ];
        let r = radius as i32;
        let side = 2 * radius as usize + 1;
        Self {
            min: [c[0] - r, c[1] - r, c[2] - r],
            side,
            cells: vec![None; side * side * side],
            next_layer: 0,
            centre,
            radius_blocks: radius,
        }
    }

    pub fn layers(&self) -> usize {
        self.side
    }

    pub fn is_done(&self) -> bool {
        self.next_layer >= self.side
    }

    /// Reads up to `layers` more horizontal layers from the source, lowest first.
    pub fn advance(&mut self, src: &mut dyn BlockSource, layers: usize) {
        for _ in 0..layers {
            if self.is_done() {
                break;
            }
            let ly = self.next_layer;
            for lz in 0..self.side {
                for lx in 0..self.side {
                    let cell = src.cell(
                        self.min[0] + lx as i32,
                        self.min[1] + ly as i32,
                        self.min[2] + lz as i32,
                    );
                    let i = self.index(lx, ly, lz);
                    self.cells[i] = cell;
                }
            }
            self.next_layer += 1;
        }
    }

    fn index(&self, x: usize, y: usize, z: usize) -> usize {
        (y * self.side + z) * self.side + x
    }

    fn full_at(&self, x: i64, y: i64, z: i64) -> bool {
        let s = self.side as i64;
        if x < 0 || y < 0 || z < 0 || x >= s || y >= s || z >= s {
            return false;
        }
        self.cells[self.index(x as usize, y as usize, z as usize)]
            .as_ref()
            .is_some_and(|c| c.full)
    }

    /// The triangles of a finished scan, mapped to link space and cut to the size limit.
    pub fn build(&self, map: &Mapping) -> Snapshot {
        self.build_with_limit(map, MAX_SNAPSHOT_TRIANGLES)
    }

    pub fn build_with_limit(&self, map: &Mapping, limit: usize) -> Snapshot {
        let mut tris: Vec<[[f64; 3]; 3]> = Vec::new();
        for ly in 0..self.side {
            for lz in 0..self.side {
                for lx in 0..self.side {
                    let Some(cell) = &self.cells[self.index(lx, ly, lz)] else {
                        continue;
                    };
                    let origin = [
                        f64::from(self.min[0]) + lx as f64,
                        f64::from(self.min[1]) + ly as f64,
                        f64::from(self.min[2]) + lz as f64,
                    ];
                    for b in &cell.boxes {
                        for (axis, max_side) in FACES {
                            if on_boundary(b, axis, max_side) {
                                let mut d = [0i64; 3];
                                d[axis] = if max_side { 1 } else { -1 };
                                if self.full_at(
                                    lx as i64 + d[0],
                                    ly as i64 + d[1],
                                    lz as i64 + d[2],
                                ) {
                                    continue;
                                }
                            }
                            push_face(&mut tris, origin, b, axis, max_side);
                        }
                    }
                }
            }
        }
        let centre = map.position(self.centre);
        let radius = (f64::from(self.radius_blocks) * map.scale) as f32;
        let mut mapped: Vec<Tri> = tris.into_iter().map(|t| map.triangle(t)).collect();
        let total = mapped.len();
        if total <= limit {
            return Snapshot {
                centre,
                radius,
                triangles: mapped,
                dropped: 0,
                total,
            };
        }
        let mut keyed: Vec<(f32, usize)> = mapped
            .iter()
            .enumerate()
            .map(|(i, t)| (distance_to_bounds(centre, t), i))
            .collect();
        // Stable, so ties keep scan order.
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let nearest_dropped = keyed[limit].0;
        let mut keep: Vec<usize> = keyed[..limit].iter().map(|k| k.1).collect();
        keep.sort_unstable();
        let kept: Vec<Tri> = keep.into_iter().map(|i| mapped[i]).collect();
        mapped.clear();
        Snapshot {
            centre,
            radius: radius.min(nearest_dropped),
            triangles: kept,
            dropped: total - limit,
            total,
        }
    }
}

/// (axis, on the max side), in a fixed order.
const FACES: [(usize, bool); 6] = [
    (0, false),
    (0, true),
    (1, false),
    (1, true),
    (2, false),
    (2, true),
];

fn on_boundary(b: &[f64; 6], axis: usize, max_side: bool) -> bool {
    if max_side {
        b[3 + axis] >= 1.0
    } else {
        b[axis] <= 0.0
    }
}

/// Two triangles of one box face, counter-clockwise seen from outside. The face normal is `+e_axis`
/// or `-e_axis`; `u = e_(axis+1)` and `v = e_(axis+2)` satisfy `u x v = e_axis`, and for the min side
/// they swap, so the corner order `p, p+u, p+u+v, p+v` always winds counter-clockwise from outside.
fn push_face(
    out: &mut Vec<[[f64; 3]; 3]>,
    origin: [f64; 3],
    b: &[f64; 6],
    axis: usize,
    max_side: bool,
) {
    let (mut ua, mut va) = ((axis + 1) % 3, (axis + 2) % 3);
    if !max_side {
        std::mem::swap(&mut ua, &mut va);
    }
    let mut p = [origin[0] + b[0], origin[1] + b[1], origin[2] + b[2]];
    p[axis] = origin[axis] + if max_side { b[3 + axis] } else { b[axis] };
    let (w, h) = (b[3 + ua] - b[ua], b[3 + va] - b[va]);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let corner = |du: f64, dv: f64| {
        let mut q = p;
        q[ua] += du;
        q[va] += dv;
        q
    };
    let (c0, c1, c2, c3) = (
        corner(0.0, 0.0),
        corner(w, 0.0),
        corner(w, h),
        corner(0.0, h),
    );
    out.push([c0, c1, c2]);
    out.push([c0, c2, c3]);
}

fn distance_to_bounds(p: [f32; 3], t: &Tri) -> f32 {
    let mut d2 = 0.0f32;
    for k in 0..3 {
        let lo = t[0][k].min(t[1][k]).min(t[2][k]);
        let hi = t[0][k].max(t[1][k]).max(t[2][k]);
        let d = (lo - p[k]).max(p[k] - hi).max(0.0);
        d2 += d * d;
    }
    d2.sqrt()
}

/// A hash of a snapshot's contents, to tell whether a new scan found anything different (FNV-1a over
/// the bytes of the numbers; not a security measure).
pub fn fingerprint(s: &Snapshot) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for v in s.centre {
        eat(&v.to_le_bytes());
    }
    eat(&s.radius.to_le_bytes());
    for t in &s.triangles {
        for v in t {
            for c in v {
                eat(&c.to_le_bytes());
            }
        }
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Grid(HashMap<(i32, i32, i32), Rc<Cell>>);

    impl BlockSource for Grid {
        fn cell(&mut self, x: i32, y: i32, z: i32) -> Option<Rc<Cell>> {
            Some(self.0.get(&(x, y, z)).cloned().unwrap_or_else(Cell::empty))
        }
    }

    fn scan(grid: &mut Grid, centre: [f64; 3], r: u32) -> Scan {
        let mut s = Scan::new(centre, r);
        let n = s.layers();
        s.advance(grid, n);
        assert!(s.is_done());
        s
    }

    fn normal(t: &Tri) -> [f32; 3] {
        let (a, b, c) = (t[0], t[1], t[2]);
        let (u, v) = (
            [b[0] - a[0], b[1] - a[1], b[2] - a[2]],
            [c[0] - a[0], c[1] - a[1], c[2] - a[2]],
        );
        [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ]
    }

    #[test]
    fn a_lone_cube_has_twelve_triangles_all_facing_outward() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        assert_eq!(snap.triangles.len(), 12);
        let centre = [0.5f32; 3];
        for t in &snap.triangles {
            let n = normal(t);
            let mid = [
                (t[0][0] + t[1][0] + t[2][0]) / 3.0 - centre[0],
                (t[0][1] + t[1][1] + t[2][1]) / 3.0 - centre[1],
                (t[0][2] + t[1][2] + t[2][2]) / 3.0 - centre[2],
            ];
            let dot = n[0] * mid[0] + n[1] * mid[1] + n[2] * mid[2];
            assert!(dot > 0.0, "triangle {t:?} faces inward");
        }
    }

    #[test]
    fn each_face_normal_is_the_axis_it_lies_on() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        let mut seen = [[false; 2]; 3];
        for t in &snap.triangles {
            let n = normal(t);
            for axis in 0..3 {
                if n[axis].abs() > 0.5 {
                    seen[axis][usize::from(n[axis] > 0.0)] = true;
                }
            }
        }
        assert_eq!(seen, [[true; 2]; 3]);
    }

    #[test]
    fn touching_full_blocks_hide_their_shared_faces() {
        let mut g = Grid([((0, 0, 0), Cell::cube()), ((1, 0, 0), Cell::cube())].into());
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 2).build(&Mapping::default());
        // two cubes side by side: 10 faces remain, 20 triangles
        assert_eq!(snap.triangles.len(), 20);
    }

    #[test]
    fn solid_ground_costs_only_its_top_and_rim() {
        let mut cells = HashMap::new();
        for x in -2..=2 {
            for z in -2..=2 {
                for y in -2..=-1 {
                    cells.insert((x, y, z), Cell::cube());
                }
            }
        }
        let mut g = Grid(cells);
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 2).build(&Mapping::default());
        // top: 25 faces; the four sides of the 5x5x2 slab: 4 * 5 * 2 faces; the bottom is outside the scan
        // (y = -3 is out of the cube, so those 25 faces count too because nothing is known behind them)
        assert_eq!(snap.triangles.len(), 2 * (25 + 40 + 25));
    }

    #[test]
    fn a_half_slab_keeps_its_top_inside_the_block() {
        let slab = Rc::new(Cell {
            full: false,
            boxes: vec![[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]],
        });
        let mut g = Grid([((0, 0, 0), slab)].into());
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        assert_eq!(snap.triangles.len(), 12);
        assert!(snap.triangles.iter().flatten().all(|v| v[1] <= 0.5 + 1e-6));
        assert!(
            snap.triangles
                .iter()
                .flatten()
                .any(|v| (v[1] - 0.5).abs() < 1e-6)
        );
    }

    #[test]
    fn a_slab_below_a_full_block_still_shows_its_top_face() {
        // the top of a bottom slab does not touch the cell boundary, so the full block above does not hide it
        let slab = Rc::new(Cell {
            full: false,
            boxes: vec![[0.0, 0.0, 0.0, 1.0, 0.5, 1.0]],
        });
        let mut g = Grid([((0, 0, 0), slab), ((0, 1, 0), Cell::cube())].into());
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 2).build(&Mapping::default());
        let top_faces = snap
            .triangles
            .iter()
            .filter(|t| normal(t)[1] > 0.5 && (t[0][1] - 0.5).abs() < 1e-6)
            .count();
        assert_eq!(top_faces, 2);
    }

    #[test]
    fn the_scan_can_be_read_in_slices() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let mut s = Scan::new([0.5, 0.5, 0.5], 1);
        s.advance(&mut g, 1);
        assert!(!s.is_done());
        s.advance(&mut g, 1);
        s.advance(&mut g, 5);
        assert!(s.is_done());
        assert_eq!(s.build(&Mapping::default()).triangles.len(), 12);
    }

    #[test]
    fn unloaded_blocks_hide_nothing() {
        struct None_;
        impl BlockSource for None_ {
            fn cell(&mut self, _: i32, _: i32, _: i32) -> Option<Rc<Cell>> {
                None
            }
        }
        let mut s = Scan::new([0.0, 0.0, 0.0], 1);
        s.advance(&mut None_, 3);
        assert!(s.build(&Mapping::default()).triangles.is_empty());
    }

    #[test]
    fn a_mirror_keeps_triangles_facing_outward() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let map = Mapping {
            flip_x: true,
            ..Mapping::default()
        };
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&map);
        let centre = map.position([0.5, 0.5, 0.5]);
        for t in &snap.triangles {
            let n = normal(t);
            let mid = [
                (t[0][0] + t[1][0] + t[2][0]) / 3.0 - centre[0],
                (t[0][1] + t[1][1] + t[2][1]) / 3.0 - centre[1],
                (t[0][2] + t[1][2] + t[2][2]) / 3.0 - centre[2],
            ];
            assert!(n[0] * mid[0] + n[1] * mid[1] + n[2] * mid[2] > 0.0);
        }
    }

    #[test]
    fn scale_and_offset_apply_to_centre_radius_and_vertices() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let map = Mapping {
            scale: 2.0,
            offset: [0.0, -4.0, 0.0],
            ..Mapping::default()
        };
        let snap = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&map);
        assert_eq!(snap.centre, [1.0, -3.0, 1.0]);
        assert_eq!(snap.radius, 2.0);
        let ys: Vec<f32> = snap.triangles.iter().flatten().map(|v| v[1]).collect();
        assert!(ys.iter().all(|y| (-4.0..=-2.0).contains(y)));
    }

    #[test]
    fn over_the_limit_the_nearest_triangles_stay_and_the_radius_shrinks() {
        let mut cells = HashMap::new();
        for x in 0..5 {
            cells.insert((x, 0, 0), Cell::cube());
        }
        // isolated cubes so no face hides another
        let cells: HashMap<_, _> = cells
            .into_iter()
            .map(|((x, y, z), c)| ((x * 2, y, z), c))
            .collect();
        let mut g = Grid(cells);
        let s = scan(&mut g, [0.5, 0.5, 0.5], 9);
        let full = s.build(&Mapping::default());
        assert_eq!(full.total, 60);
        let cut = s.build_with_limit(&Mapping::default(), 24);
        assert_eq!(cut.triangles.len(), 24);
        assert_eq!(cut.dropped, 36);
        assert_eq!(cut.total, 60);
        // every kept triangle is at least as near as every dropped one, so the radius is the nearest dropped distance
        let far = cut
            .triangles
            .iter()
            .map(|t| distance_to_bounds(cut.centre, t))
            .fold(0.0f32, f32::max);
        assert!(
            cut.radius >= far - 1e-6,
            "radius {} < kept {}",
            cut.radius,
            far
        );
        assert!(cut.radius < 9.0);
        // the same world gives the same cut
        assert_eq!(cut, s.build_with_limit(&Mapping::default(), 24));
    }

    #[test]
    fn the_limit_is_the_contracts() {
        assert_eq!(MAX_SNAPSHOT_TRIANGLES, 925_696);
    }

    #[test]
    fn the_fingerprint_changes_with_the_content() {
        let mut g = Grid([((0, 0, 0), Cell::cube())].into());
        let a = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        let b = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        assert_eq!(fingerprint(&a), fingerprint(&b));
        g.0.insert((1, 0, 0), Cell::cube());
        let c = scan(&mut g, [0.5, 0.5, 0.5], 1).build(&Mapping::default());
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }
}
