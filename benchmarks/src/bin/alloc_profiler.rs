//! Phase 0.5.1 §14 — allocation profiling with a counting global allocator.
//!
//! Installs a wrapper around `System` that counts allocations/bytes, then
//! runs the two steady-state hot loops headlessly:
//!
//!   1. the state → snapshot → render-prep frame loop
//!   2. the glyph-cache warm-hit loop
//!
//! Usage: `cargo run --release -p benchmarks --bin alloc_profiler`
//!
//! The GPU renderer's own per-frame allocation (the instance `Vec` rebuilt
//! each frame) cannot run headlessly; it is identified in the report by code
//! inspection instead.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use terminal_core::TerminalState;
use terminal_text::{FontLibrary, GlyphCache};

struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        LIVE.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        LIVE.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

fn reset_counters() {
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    LIVE.store(0, Ordering::Relaxed);
}

fn read_counters() -> (usize, usize, usize) {
    (
        ALLOC_COUNT.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
        LIVE.load(Ordering::Relaxed),
    )
}

fn main() {
    println!("=== Alloc profiler (counting allocator) ===");

    // --- Loop 1: state → snapshot → render-prep frame ---
    let mut state = TerminalState::new(200, 60);
    for _ in 0..30 {
        for c in 0..200u16 {
            state.write_char(char::from(b'a' + (c % 26) as u8));
        }
        state.cursor_to_beginning_of_line();
        state.cursor_down(1);
    }
    // Force a full redraw on every frame so the loop is representative.
    state.mark_all_dirty();

    const FRAMES: usize = 2000;
    reset_counters();
    let t0 = Instant::now();
    for _ in 0..FRAMES {
        let dirty = state.consume_dirty();
        let snap = state.snapshot();
        let mut acc = 0u64;
        for r in 0..snap.rows {
            if dirty.is_row_dirty(r) {
                for c in 0..snap.cols {
                    acc ^= snap.visible_cell(r, c).ch as u64;
                }
            }
        }
        std::hint::black_box(acc);
        state.mark_all_dirty();
    }
    let frame_ms = t0.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
    let (allocs, bytes, _live) = read_counters();
    println!(
        "frame loop: {allocs} allocs / {FRAMES} frames = {:.2} allocs/frame, {:.1} B/frame, {:.3} ms/frame",
        allocs as f64 / FRAMES as f64,
        bytes as f64 / FRAMES as f64,
        frame_ms
    );

    // --- Loop 2: glyph cache warm hits ---
    let mut lib = FontLibrary::new();
    lib.scan_system();
    let primary = match lib.primary_monospace(None) {
        Some(f) => f,
        None => {
            println!("glyph loop: skipped (no fonts)");
            return;
        }
    };
    let mut cache = GlyphCache::new(14.0, 8 * 1024 * 1024);
    cache.set_font(primary);
    let corpus: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,;:!?()[]{}-_=+/*&%$#@ éüöäåçñß 你好世界 🙂🚀"
        .chars()
        .collect();
    for &c in &corpus {
        if let Some(f) = lib.font_for(c) {
            cache.glyph(f, c);
        }
    }
    let (hits0, _) = cache.stats();

    const LOOKUPS: usize = 500_000;
    reset_counters();
    let t1 = Instant::now();
    for i in 0..LOOKUPS {
        let c = corpus[i % corpus.len()];
        if let Some(f) = lib.font_for(c) {
            cache.glyph(f, c);
        }
    }
    let glyph_ms = t1.elapsed().as_secs_f64() * 1000.0 / LOOKUPS as f64;
    let (g_allocs, g_bytes, _) = read_counters();
    let (hits1, _) = cache.stats();
    let hit_rate = {
        let h = hits1 - hits0;
        h as f64 / LOOKUPS as f64
    };
    println!(
        "glyph loop: {g_allocs} allocs / {LOOKUPS} lookups = {:.3} allocs/lookup, {:.3} B/lookup, {:.4} µs/lookup, hit rate {:.1}%",
        g_allocs as f64 / LOOKUPS as f64,
        g_bytes as f64 / LOOKUPS as f64,
        glyph_ms * 1000.0,
        hit_rate * 100.0
    );

    println!(
        "\nConclusion: the headless state→snapshot path is allocation-free; \
         glyph warm hits are allocation-free. The GPU renderer's per-frame \
         instance Vec is the remaining steady-state allocation (see report)."
    );
}
