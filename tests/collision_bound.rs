//! The collision snapshot never holds more than its limit of triangles: the faces of each layer are made as the
//! scan reads it and only the nearest ones are kept, so the memory a scan needs does not grow with the number of
//! exposed faces in the cube. Measured with a counting allocator, which is why this test has a file of its own.

use pumpkin_link::collision::{BlockSource, Cell, Scan};
use pumpkin_link::mapping::Mapping;
use std::alloc::{GlobalAlloc, Layout, System};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc` is passed on unchanged.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as for `alloc`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Cubes on every second block: every face is exposed, 12 triangles a cube.
struct Checker(Rc<Cell>, Rc<Cell>);

impl BlockSource for Checker {
    fn cell(&mut self, x: i32, y: i32, z: i32) -> Option<Rc<Cell>> {
        Some(if (x + y + z).rem_euclid(2) == 0 {
            self.0.clone()
        } else {
            self.1.clone()
        })
    }
}

#[test]
fn a_scan_over_the_limit_never_holds_every_triangle() {
    const RADIUS: u32 = 24;
    const LIMIT: usize = 1000;
    let mut src = Checker(Cell::cube(), Cell::empty());
    let side = 2 * RADIUS as usize + 1;
    let cubes = (side * side * side).div_ceil(2);

    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let mut scan = Scan::with_limit([0.5, 0.5, 0.5], RADIUS, Mapping::default(), LIMIT);
    let layers = scan.layers();
    for _ in 0..layers {
        scan.advance(&mut src, 1);
    }
    let snap = scan.finish();
    let peak = PEAK.load(Ordering::SeqCst) - before;

    assert_eq!(snap.total, 12 * cubes);
    assert_eq!(snap.triangles.len(), LIMIT);
    // What the scan may hold: one pointer per block of the cube, the kept triangles with their keys, and slack.
    // Every triangle of this cube as 36-byte link triangles alone would be 12 * 58 825 * 36 bytes, about 25 MB.
    let cells = side * side * side * std::mem::size_of::<Option<Rc<Cell>>>();
    let allowed = cells + LIMIT * 64 * 2 + 256 * 1024;
    assert!(
        peak <= allowed,
        "peak {peak} bytes over the {allowed} a bounded scan needs ({} triangles in all)",
        snap.total
    );
}
