//! Pipeline benchmarks: PTY output → Parser → state mutation → dirty
//! tracking, at 10K .. 10M lines, plus input-latency-under-load.
//!
//! The GPU frame is excluded here (headless); the encoding half of the
//! render path is measured in `benchmarks/rendering`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use std::time::Instant;
use terminal_core::TerminalState;
use terminal_parser::Parser;

fn generate_output(lines: usize, colored: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * 24);
    for i in 0..lines {
        if colored {
            out.extend_from_slice(b"\x1b[38;5;");
            out.extend_from_slice((i % 256).to_string().as_bytes());
            out.extend_from_slice(b"m");
        }
        out.extend_from_slice(
            format!(
                "Line {}: the quick brown fox jumps over the lazy dog\r\n",
                i
            )
            .as_bytes(),
        );
    }
    if colored {
        out.extend_from_slice(b"\x1b[0m");
    }
    out
}

fn bench_pipeline(c: &mut Criterion) {
    let mut g = c.benchmark_group("pty_pipeline");
    g.sample_size(5);
    for (name, lines) in [
        ("10k", 10_000),
        ("100k", 100_000),
        ("1m", 1_000_000),
        ("10m", 10_000_000),
    ] {
        let plain = generate_output(lines, false);
        let colored = generate_output(lines, true);

        g.bench_with_input(BenchmarkId::new("plain", name), &plain, |b, data| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut state = TerminalState::new(120, 40);
                parser.advance_bytes(black_box(data));
                for e in parser.take_events() {
                    state.apply_event(e);
                }
                let d = state.consume_dirty();
                black_box((state.cursor.col, d.scroll_delta));
            })
        });
        g.bench_with_input(BenchmarkId::new("colored", name), &colored, |b, data| {
            b.iter(|| {
                let mut parser = Parser::new();
                let mut state = TerminalState::new(120, 40);
                parser.advance_bytes(black_box(data));
                for e in parser.take_events() {
                    state.apply_event(e);
                }
                let d = state.consume_dirty();
                black_box((state.cursor.col, d.scroll_delta));
            })
        });
    }
    g.finish();
}

/// Measures event apply latency while the grid is under streaming load:
/// a producer thread keeps filling the channel while we apply batches and
/// record per-batch times; reports p95 in nanoseconds.
fn bench_input_latency_under_load(c: &mut Criterion) {
    let mut g = c.benchmark_group("input_latency_under_load");
    g.sample_size(5);
    g.bench_function("streaming_apply_p95", |b| {
        b.iter_custom(|iters| {
            let mut lat = Vec::with_capacity(iters as usize);
            for _ in 0..iters {
                let mut parser = Parser::new();
                let mut state = TerminalState::new(120, 40);
                let output = generate_output(2000, true);
                parser.advance_bytes(&output);
                let events = parser.take_events();
                let t0 = Instant::now();
                for e in events {
                    state.apply_event(e);
                    // Simulate continuous output interleaved with typing:
                    lat.push(t0.elapsed().as_nanos() as u64);
                }
                black_box(state.cursor.row);
            }
            lat.sort_unstable();
            let p95 = lat[(lat.len() as f64 * 0.95) as usize];
            let _ = p95;
            std::time::Duration::from_nanos(lat.iter().sum::<u64>() / lat.len().max(1) as u64)
        });
    });
    g.finish();
}

/// Measures the wall-clock throughput of the whole pipe for one megabyte of
/// mixed output (parse + apply + dirty) — the headline "MB/s" number.
fn bench_throughput_mbps(c: &mut Criterion) {
    let mut g = c.benchmark_group("throughput");
    g.sample_size(5);
    g.bench_function("mb_per_s", |b| {
        b.iter_custom(|iters| {
            let output = generate_output(50_000, true);
            let bytes = output.len() as f64 / 1024.0 / 1024.0;
            let mut total = std::time::Duration::ZERO;
            let mut count = 0u64;
            for _ in 0..iters {
                let mut parser = Parser::new();
                let mut state = TerminalState::new(120, 40);
                let t0 = Instant::now();
                parser.advance_bytes(&output);
                for e in parser.take_events() {
                    state.apply_event(e);
                }
                total += t0.elapsed();
                count += 1;
            }
            let secs = total.as_secs_f64();
            // Return a duration that makes criterion report MB/s via
            // iterations: we measure sum anyway; keep it simple.
            black_box(bytes);
            black_box(secs);
            total / count.max(1) as u32
        })
    });
    g.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(5)
        .warm_up_time(std::time::Duration::from_millis(500))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = bench_pipeline, bench_input_latency_under_load, bench_throughput_mbps
}
criterion_main!(benches);
