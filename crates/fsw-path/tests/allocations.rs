//! Allocation contract for the resolver: steady-state resolution must not
//! allocate. This is the property the "lightning fast" claim actually rests
//! on for the broker's Enter path — deterministic, unlike timing tests, so it
//! runs in the default suite rather than behind `--ignored`.
//!
//! `RenderBuf::with_capacity` exists precisely so a reused buffer never
//! reallocates (see its doc comment); this test pins that promise.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

mod common;

use fsw_path::{RenderBuf, resolve};

#[test]
fn steady_state_resolution_allocates_nothing() {
    let mut buf = RenderBuf::with_capacity(512);

    // Warm-up pass: exercises every case once so any lazy one-time
    // initialization happens before the counter is reset. Correctness itself
    // is resolver.rs's job; only allocation is asserted here.
    let mut checked = 0_u32;
    for (input, ctx) in common::contexts() {
        let _ = std::hint::black_box(resolve(input, &ctx, &mut buf));
        checked += 1;
    }
    assert!(checked > 500, "corpus shrank unexpectedly: {checked}");

    // Measured pass. The corpus's longest render is far below the buffer's
    // 512-byte reservation, so no growth allocation is possible either.
    ALLOCATIONS.store(0, Relaxed);
    for (input, ctx) in common::contexts() {
        let _ = std::hint::black_box(resolve(input, &ctx, &mut buf));
    }
    assert_eq!(
        ALLOCATIONS.load(Relaxed),
        0,
        "steady-state resolution allocated; the broker's Enter path regressed"
    );

    // Same promise for the single hot case repeated hard.
    let hot = common::LEADING.iter().find(|input| **input == "/tmp");
    let Some(hot) = hot else {
        panic!("corpus lost its /tmp case");
    };
    let ctx = fsw_path::Context {
        registry: common::REGISTERED,
        mode: fsw_path::BareSlashMode::DefaultDistribution,
        preferred: Some("Ubuntu"),
        wsl_default: Some("Ubuntu"),
    };
    ALLOCATIONS.store(0, Relaxed);
    for _ in 0..10_000 {
        let _ = std::hint::black_box(resolve(hot, &ctx, &mut buf));
    }
    assert_eq!(
        ALLOCATIONS.load(Relaxed),
        0,
        "hot-path resolution allocated across 10,000 resolves"
    );
}
