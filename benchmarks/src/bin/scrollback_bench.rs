//! Phase 0.5.2 §5 — scrollback benchmark suite (state-level).
//!
//! Fills a `TerminalState` with 1k → 1M rows of representative terminal
//! output and measures, per size:
//!
//!   * live heap bytes + allocation count (counting global allocator)
//!   * process RSS (`ps`)
//!   * tier breakdown: hot grid rows, cold blocks, cold compressed bytes
//!   * insertion cost (rows/s)
//!   * scroll latency (materialize viewport at 25/50/90 % of history)
//!   * random historical access (cold `grid_row`)
//!   * rendering visible history (snapshot + read all cells)
//!   * codec cost (encode/decode one cold block)
//!
//! Usage: `cargo run --release -p benchmarks --bin scrollback_bench`
//!        `cargo run --release -p benchmarks --bin scrollback_bench -- 1000000`

use std::alloc::{GlobalAlloc, Layout, System};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use terminal_core::TerminalState;

// ---------------------------------------------------------------------------
// Counting global allocator (tracks live bytes + alloc count)
// ---------------------------------------------------------------------------

struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE_ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        LIVE_ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        LIVE_ALLOCS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        LIVE_ALLOCS.fetch_sub(1, Ordering::Relaxed);
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
    LIVE_BYTES.store(0, Ordering::Relaxed);
    LIVE_ALLOCS.store(0, Ordering::Relaxed);
}
fn allocs_total() -> usize {
    ALLOC_COUNT.load(Ordering::Relaxed)
}

fn rss_kb() -> f64 {
    let pid = std::process::id();
    let Ok(out) = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
    else {
        return 0.0;
    };
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Filling
// ---------------------------------------------------------------------------

/// Writes one row of content at the cursor then moves down.
/// `kind`: "seq" (incrementing), "yes" (identical), "code" (styled), "blank".
fn fill_rows(state: &mut TerminalState, rows: usize, kind: &str) {
    for i in 0..rows {
        match kind {
            "blank" => {
                state.cursor_to_beginning_of_line();
                state.cursor_down(1);
            }
            "yes" => {
                for b in "0123456789abcdefghijklmnopqrstuvwxyz"
                    .chars()
                    .cycle()
                    .take(100)
                {
                    state.write_char(b);
                }
                state.cursor_to_beginning_of_line();
                state.cursor_down(1);
            }
            "seq" => {
                let mut buf = [0u8; 24];
                let digits = format!("line {i:08} ");
                for (c, b) in digits.bytes().enumerate() {
                    buf[c % 24] = b;
                }
                for k in 0..100usize {
                    state.write_char(char::from(buf[k % 24]));
                }
                state.cursor_to_beginning_of_line();
                state.cursor_down(1);
            }
            "code" => {
                if i % 5 == 0 {
                    state.sgr(&[38, 2, 50, 120, 200]);
                }
                let text = format!("  let value_{i} = compute(&input); // phase 0.5.2 scan {i}");
                for ch in text.chars().take(100) {
                    state.write_char(ch);
                }
                if i % 5 == 4 {
                    state.sgr(&[0]);
                }
                state.cursor_to_beginning_of_line();
                state.cursor_down(1);
            }
            _ => {}
        }
    }
}

/// State-accounted memory: hot rows (16 B cells + Vec/Row overhead) + cold
/// blocks + pending span buffer. Exact and allocator-independent.
fn state_bytes(state: &TerminalState) -> usize {
    state.retained_memory()
}

fn measure(state: &TerminalState, label: &str) {
    let cold_bytes: usize = state.cold.blocks.iter().map(|b| b.retained_bytes()).sum();
    println!(
        "{label:>10} | state {:.2} MB | rss {:.2} MB | hot {} rows | cold {} rows in {} blks ({:.2} MB) | allocs {}",
        state_bytes(state) as f64 / 1e6,
        rss_kb() / 1024.0,
        state.grid.len() - state.rows as usize,
        state.cold.total_rows,
        state.cold.len(),
        cold_bytes as f64 / 1e6,
        allocs_total(),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let unbounded = args.iter().any(|a| a == "--unbounded");
    let n_arg = args
        .iter()
        .find(|a| !a.starts_with('-') && a.parse::<usize>().is_ok());
    let sizes: Vec<usize> = match n_arg {
        Some(s) => vec![s.parse().expect("row count")],
        None => vec![1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000],
    };
    let kinds = ["seq", "yes", "code", "blank"];
    println!(
        "Phase 0.5.2 §5 scrollback benchmark (120x40){}; sizes: {:?}",
        if unbounded {
            " [--unbounded: retain ALL history]"
        } else {
            ""
        },
        sizes
    );

    for kind in kinds {
        println!("\n=== content kind: {kind} ===");
        for n in &sizes {
            let mut state = TerminalState::new(120, 40);
            if unbounded {
                state.scrollback_limit = 2_000_000;
            }
            reset_counters();
            let t0 = Instant::now();
            fill_rows(&mut state, *n, kind);
            let insert_ms = t0.elapsed().as_secs_f64() * 1000.0;
            measure(&state, &format!("{n}"));
            let rows_per_s = *n as f64 / (insert_ms / 1000.0);
            println!(
                "          | insert {insert_ms:.0} ms ({:.0} rows/s)",
                rows_per_s
            );

            // Scroll latency: snapshot (viewport decode when deep) + render.
            for frac in [0.25f64, 0.50, 0.90] {
                let off = (state.scrollback_len() as f64 * frac) as u32;
                let mut st = state.clone();
                let t = Instant::now();
                st.set_scroll_offset(off);
                let snap = st.snapshot();
                let mut acc = 0u64;
                for r in 0..snap.rows {
                    for c in 0..snap.cols {
                        acc ^= snap.visible_cell(r, c).ch as u64;
                    }
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                std::hint::black_box(acc);
                println!(
                    "          | scroll to {:.0}%: {ms:.3} ms (snapshot + read 40x120)",
                    frac * 100.0
                );
            }

            // Correctness: scrolled content must match the logical row.
            if kind == "seq" && *n >= 500 {
                let mut st = state.clone();
                let mut ok = true;
                for off in [
                    0u32,
                    state.scrollback_len() / 3,
                    state.scrollback_len() * 2 / 3,
                    state.scrollback_len(),
                ] {
                    st.set_scroll_offset(off);
                    let snap = st.snapshot();
                    // First visible logical row should start with "line " text.
                    let c = snap.visible_cell(0, 0);
                    if c.ch != b'l' as u32 && c.ch != 0 {
                        ok = false;
                        println!(
                            "          | CORRECTNESS FAIL at off {off}: cell(0,0) = {:#x}",
                            c.ch
                        );
                    }
                }
                if ok {
                    println!("          | deep-scroll content check: OK");
                }
            }

            // Random historical access across the cold tier.
            if state.cold.total_rows > 0 {
                let mut st = state.clone();
                let cold_rows = st.cold.total_rows;
                let mut acc = 0u64;
                let t = Instant::now();
                for _ in 0..200 {
                    let idx = (fast_rng() as usize) % cold_rows;
                    let row = st.grid_row(idx);
                    acc ^= row.cells[0].ch as u64;
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                std::hint::black_box(acc);
                println!("          | random cold access (200 rows): {ms:.3} ms");
            }

            // Rendering the visible history: snapshot + read all cells.
            {
                let mut st = state.clone();
                st.set_scroll_offset(st.scrollback_len() / 2);
                let t = Instant::now();
                let snap = st.snapshot();
                let mut acc = 0u64;
                for r in 0..snap.rows {
                    for c in 0..snap.cols {
                        acc ^= snap.visible_cell(r, c).ch as u64;
                    }
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0;
                std::hint::black_box(acc);
                println!("          | render scrolled viewport: {ms:.3} ms");
            }

            // Codec cost: encode + decode one cold block.
            if let Some(blk) = state.cold.blocks.front() {
                let t = Instant::now();
                let mut total = 0usize;
                for _ in 0..10 {
                    let rows = terminal_core::decode_block(blk);
                    total += rows.len();
                }
                let dec_ms = t.elapsed().as_secs_f64() * 1000.0;
                std::hint::black_box(total);
                println!(
                    "          | decode block (10x, {} rows): {dec_ms:.3} ms ({} B compressed)",
                    blk.rows,
                    blk.data.len()
                );
            }
        }
    }
    println!("\nDONE");
}

fn fast_rng() -> u64 {
    use std::cell::Cell;
    thread_local! {
        static SEED: Cell<u64> = const { Cell::new(0x9E3779B97F4A7C15) };
    }
    SEED.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}
