//! Terminal performance benchmark suite (criterion).
//!
//! Covers the Phase 0.5 audit list: cell_write, row_scroll, cursor_move,
//! clear_line, clear_screen, insert_line, delete_line, resize,
//! random_cell_access, sequential_output — at 10K / 100K / 1M / 10M cells.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use terminal_core::TerminalState;

const CELL_SIZES: [(u16, u16, &str); 4] = [
    (80, 125, "10k"),
    (160, 625, "100k"),
    (160, 6250, "1m"),
    (200, 50_000, "10m"),
];

/// Creates a state with `total_cells` (cols × rows) plus a fill pattern.
fn make_state(cols: u16, rows: u16) -> TerminalState {
    let mut s = TerminalState::new(cols, rows);
    for _ in 0..rows.saturating_sub(1) {
        for c in 0..cols {
            let ch = (b'a' + (c % 26) as u8) as char;
            s.write_char(ch);
        }
        s.cursor_to_beginning_of_line();
        s.cursor_down(1);
    }
    s.cursor_to_beginning_of_line();
    s
}

fn bench_cell_write(c: &mut Criterion) {
    let mut g = c.benchmark_group("cell_write");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = TerminalState::new(cols, rows);
        g.bench_with_input(BenchmarkId::new("sequential", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for i in 0..cols {
                    t.write_char(char::from(b'a' + (i % 26) as u8));
                }
                t.cursor_to_beginning_of_line();
                t.cursor_down(1);
                black_box(t.cursor.col);
            })
        });
    }
    g.finish();
}

fn bench_row_scroll(c: &mut Criterion) {
    let mut g = c.benchmark_group("row_scroll");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("wrap", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for _ in 0..1000 {
                    t.cursor_down(1); // at bottom row → wraps and scrolls
                }
                black_box(t.scrollback_len());
            })
        });
    }
    g.finish();
}

fn bench_cursor_move(c: &mut Criterion) {
    let mut g = c.benchmark_group("cursor_move");
    g.sample_size(20);
    for (cols, rows, name) in CELL_SIZES {
        let s = TerminalState::new(cols, rows);
        g.bench_with_input(BenchmarkId::new("row_col", name), &s, |b, _| {
            b.iter(|| {
                let mut t = s.clone();
                for i in 0..cols {
                    t.cursor_position(i, rows % 2);
                    t.cursor_position(0, i % rows);
                }
                black_box(t.cursor.col);
            })
        });
    }
    g.finish();
}

fn bench_clear(c: &mut Criterion) {
    let mut g = c.benchmark_group("clear");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("line", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for r in 0..rows {
                    t.cursor.row = r;
                    t.cursor.col = 0;
                    t.clear_line(2);
                }
                black_box(t.cursor.col);
            })
        });
        g.bench_with_input(BenchmarkId::new("screen", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for _ in 0..100 {
                    t.clear_screen(2);
                }
                black_box(t.cursor.col);
            })
        });
    }
    g.finish();
}

fn bench_insert_delete_lines(c: &mut Criterion) {
    let mut g = c.benchmark_group("insert_delete_line");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("insert", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for _ in 0..100 {
                    t.insert_lines(1);
                }
                black_box(t.cursor.row);
            })
        });
        g.bench_with_input(BenchmarkId::new("delete", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for _ in 0..100 {
                    t.delete_lines(1);
                }
                black_box(t.cursor.row);
            })
        });
    }
    g.finish();
}

fn bench_resize(c: &mut Criterion) {
    let mut g = c.benchmark_group("resize");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("grow", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                t.resize(cols + 20, rows + 5);
                black_box(t.cols);
            })
        });
        g.bench_with_input(BenchmarkId::new("shrink", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                t.resize(cols - 20, rows.saturating_sub(5).max(2));
                black_box(t.cols);
            })
        });
    }
    g.finish();
}

fn bench_random_access(c: &mut Criterion) {
    let mut g = c.benchmark_group("random_cell_access");
    g.sample_size(10);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("inspect", name), &s, |b, state| {
            b.iter(|| {
                let mut acc = 0u64;
                for i in 0..1000u64 {
                    let r = ((i * 2654435761) % rows as u64) as u16;
                    let col = ((i * 40503) % cols as u64) as u16;
                    acc ^= state.visible_cell(r, col).ch as u64;
                }
                black_box(acc);
            })
        });
        // Random writes are dominated by write_char; sample a modest grid.
        g.bench_with_input(BenchmarkId::new("write", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for i in 0..1000u64 {
                    let r = ((i * 2654435761) % rows as u64) as u16;
                    let col = ((i * 40503) % cols as u64) as u16;
                    t.cursor.row = r;
                    t.cursor.col = col;
                    t.write_char('x');
                }
                black_box(t.cursor.col);
            })
        });
    }
    g.finish();
}

fn bench_dirty_tracking(c: &mut Criterion) {
    let mut g = c.benchmark_group("dirty_tracking");
    g.sample_size(20);
    for (cols, rows, name) in CELL_SIZES {
        let s = make_state(cols, rows);
        g.bench_with_input(BenchmarkId::new("consume", name), &s, |b, state| {
            b.iter(|| {
                let mut t = state.clone();
                for _ in 0..rows {
                    t.write_char('x');
                    t.cursor.col = 0;
                    t.cursor_down(1);
                }
                let d = t.consume_dirty();
                black_box(d.dirty_rows().len());
            })
        });
    }
    g.finish();
}

fn bench_unicode(c: &mut Criterion) {
    let mut g = c.benchmark_group("unicode");
    g.sample_size(20);
    let samples: &str = "aé你好🙂🏳️🌈e\u{301}x👨\u{200d}👩\u{200d}👧";
    let s = TerminalState::new(80, 24);
    g.bench_function("mixed_clusters", |b| {
        b.iter(|| {
            let mut t = s.clone();
            for _ in 0..100 {
                for ch in samples.chars() {
                    t.write_char(black_box(ch));
                }
                t.cursor_to_beginning_of_line();
            }
            black_box(t.cursor.col);
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(std::time::Duration::from_millis(300))
        .measurement_time(std::time::Duration::from_secs(2));
    targets = bench_cell_write, bench_row_scroll, bench_cursor_move, bench_clear,
        bench_insert_delete_lines, bench_resize, bench_random_access,
        bench_dirty_tracking, bench_unicode
}
criterion_main!(benches);
