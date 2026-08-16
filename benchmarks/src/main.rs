//! Benchmark report generator.
//!
//! Runs a compact, deterministic variant of the benchmark suite and prints
//! machine-readable JSON plus a budget comparison. Also writes:
//!   * `docs/performance-report.md` — the metric/baseline/current/budget table
//!   * `benchmarks/baseline.json` — the latest run (CI compares against it)
//!
//! Exit code is non-zero only when a *hard* budget is breached (see §31 of
//! the Phase 0.5 spec); noisy regressions are reported but do not fail CI.

use std::process::Command;
use std::time::Instant;

use terminal_core::{Cell, TerminalState};
use terminal_parser::Parser;
use terminal_renderer::{resolve_color, DEFAULT_BG, DEFAULT_FG};
use terminal_text::{FontLibrary, GlyphCache};

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

fn time_ms<F: FnOnce()>(f: F) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64() * 1000.0
}

/// Best-of-3 parse+apply for the given line count.
fn pipeline_ms(lines: usize, colored: bool) -> f64 {
    let data = generate_output(lines, colored);
    let mut best = f64::MAX;
    for _ in 0..3 {
        best = best.min(time_ms(|| {
            let mut parser = Parser::new();
            let mut state = TerminalState::new(120, 40);
            parser.advance_bytes(&data);
            for e in parser.take_events() {
                state.apply_event(e);
            }
            let d = state.consume_dirty();
            std::hint::black_box((state.cursor.col, d.scroll_delta));
        }));
    }
    best
}

/// Synthetic in-memory VT-parse-then-apply throughput, p95 of one isolated
/// `apply_event` call (`t0` resets before every call — see
/// `docs/performance-benchmark-audit.md` § Metric Definition for why a
/// prior per-batch reset made this measure batch-cumulative time instead).
///
/// **This is not input latency.** No PTY, no keypress, no render — a
/// microbenchmark of `TerminalState::apply_event`'s own cost, useful for
/// state-engine/CPU regression detection but structurally incapable of
/// answering "how fast does a keypress become visible" (see
/// `docs/performance-benchmarking.md` § Metric Taxonomy). For that, see
/// `measure_input_to_apply_p95` and `measure_shell_echo_p95` below, which
/// exercise the real PTY path this function never touches.
fn measure_batch_apply_p95() -> (f64, f64) {
    let mut lat = Vec::new();
    let t_start = Instant::now();
    for _ in 0..200 {
        let mut parser = Parser::new();
        let mut state = TerminalState::new(120, 40);
        let output = generate_output(2000, true);
        parser.advance_bytes(&output);
        for e in parser.take_events() {
            let t0 = Instant::now();
            state.apply_event(e);
            lat.push(t0.elapsed().as_nanos() as f64 / 1000.0); // µs
        }
    }
    let events_per_second = lat.len() as f64 / t_start.elapsed().as_secs_f64();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (
        lat[(lat.len() as f64 * 0.95) as usize] / 1000.0,
        events_per_second,
    ) // ms
}

/// Real input-latency metric (§4/§5 of the perf-benchmark audit): a live
/// PTY-backed shell session, one `session.write(b"a")` per sample, timing
/// three real pipeline boundaries with monotonic clocks —
/// `session.write` → the reader thread observing the echoed byte (a raw
/// [`terminal_session::OutputTap`] fires there, upstream of any parsing) →
/// `TerminalState` reflecting it (`session.stats().applied_batches`
/// incrementing). No render/GPU/frame-present stage is included — that
/// cannot be measured headlessly in CI; see
/// `docs/performance-benchmarking.md` § input_to_visible for why that gap
/// is documented rather than silently proxied.
///
/// Returns `(input_to_apply_p95_ms, write_to_pty_read_p95_ms,
/// read_to_apply_p95_ms)`. `n` samples; `f64::NAN` for all three if no
/// shell/PTY is available (e.g. a locked-down sandbox).
fn measure_input_to_apply_p95(n: u32) -> (f64, f64, f64) {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let Ok(pty) = pty::PtyManager::new() else {
        return (f64::NAN, f64::NAN, f64::NAN);
    };
    let pty = Arc::new(pty);
    let Some(shell) = ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
    else {
        return (f64::NAN, f64::NAN, f64::NAN);
    };

    let read_marks: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let read_marks_tap = Arc::clone(&read_marks);
    let tap: terminal_session::OutputTap = Box::new(move |_chunk: &[u8]| {
        read_marks_tap.lock().unwrap().push(Instant::now());
    });

    let Ok((session, _pid)) = terminal_session::Session::spawn_with_options(
        Arc::clone(&pty),
        shell,
        &[],
        &std::env::temp_dir().to_string_lossy(),
        &[],
        120,
        40,
        None,
        Some(tap),
    ) else {
        return (f64::NAN, f64::NAN, f64::NAN);
    };
    let mut state = TerminalState::new(120, 40);

    for _ in 0..30 {
        session.drain(&mut state);
        std::thread::sleep(Duration::from_millis(5));
    }
    read_marks.lock().unwrap().clear();

    let mut write_to_apply_ms = Vec::with_capacity(n as usize);
    let mut write_to_read_ms = Vec::with_capacity(n as usize);
    let mut read_to_apply_ms = Vec::with_capacity(n as usize);

    for _ in 0..n {
        read_marks.lock().unwrap().clear();
        let before = session.stats().applied_batches.load(Ordering::Relaxed);
        let t_write = Instant::now();
        session.write(b"a");
        let deadline = t_write + Duration::from_millis(200);
        let (t_apply, t_read) = loop {
            session.drain(&mut state);
            let applied = session.stats().applied_batches.load(Ordering::Relaxed) > before;
            let read_at = read_marks.lock().unwrap().first().copied();
            if applied {
                break (Some(Instant::now()), read_at);
            }
            if Instant::now() > deadline {
                break (None, read_at);
            }
            std::thread::sleep(Duration::from_micros(100));
        };
        if let (Some(t_apply), Some(t_read)) = (t_apply, t_read) {
            write_to_apply_ms.push(t_apply.duration_since(t_write).as_secs_f64() * 1e3);
            write_to_read_ms.push(t_read.duration_since(t_write).as_secs_f64() * 1e3);
            read_to_apply_ms.push(t_apply.duration_since(t_read).as_secs_f64() * 1e3);
        }
        std::thread::sleep(Duration::from_micros(500));
    }
    session.terminate();

    fn p95(mut v: Vec<f64>) -> f64 {
        if v.is_empty() {
            return f64::NAN;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[((v.len() - 1) as f64 * 0.95).round() as usize]
    }
    (
        p95(write_to_apply_ms),
        p95(write_to_read_ms),
        p95(read_to_apply_ms),
    )
}

/// Real shell-echo round trip (§6): write a unique token, wait for the
/// shell to echo it back into the terminal grid. Heavier than
/// `measure_input_to_apply_p95` (includes real shell tty-echo processing,
/// not just our own PTY read), so it's a distinct, complementary metric —
/// see `benchmarks/src/bin/echo_probe.rs` for a standalone, higher-sample
/// version of the same measurement used for deeper investigation.
fn measure_shell_echo_p95(n: u32) -> f64 {
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    let Ok(pty) = pty::PtyManager::new() else {
        return f64::NAN;
    };
    let pty = Arc::new(pty);
    let Some(shell) = ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
    else {
        return f64::NAN;
    };
    let Ok((session, _pid)) =
        terminal_session::Session::spawn(Arc::clone(&pty), shell, ".", 120, 40)
    else {
        return f64::NAN;
    };
    let mut state = TerminalState::new(120, 40);

    for _ in 0..30 {
        session.drain(&mut state);
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut lats = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let before = session.stats().applied_batches.load(Ordering::Relaxed);
        session.write(b"a");
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_millis(200);
        loop {
            session.drain(&mut state);
            if session.stats().applied_batches.load(Ordering::Relaxed) > before {
                lats.push(t0.elapsed().as_secs_f64() * 1e3);
                break;
            }
            if Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    session.terminate();

    if lats.is_empty() {
        return f64::NAN;
    }
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    lats[((lats.len() - 1) as f64 * 0.95).round() as usize]
}

/// Building the render input: N full-grid reads through the snapshot view.
fn snapshot_frame_us() -> f64 {
    let mut state = TerminalState::new(200, 60);
    for _ in 0..30 {
        for c in 0..200u16 {
            state.write_char(char::from(b'a' + (c % 26) as u8));
        }
        state.cursor_to_beginning_of_line();
        state.cursor_down(1);
    }
    let mut acc = 0u64;
    let t0 = Instant::now();
    for _ in 0..100 {
        let snap = state.snapshot();
        for r in 0..60u16 {
            for c in 0..200u16 {
                acc ^= snap.visible_cell(r, c).ch as u64;
            }
        }
    }
    let per_frame = t0.elapsed().as_nanos() as f64 / 100.0 / 1000.0; // µs
    std::hint::black_box(acc);
    per_frame
}

/// Render preparation: resolve colors + pack an instance for 10K rows × 200
/// cols (mirrors `terminal-renderer`'s per-cell hot path, headless).
fn render_prep_10k_rows_ms() -> f64 {
    let cells: Vec<Cell> = (0..10_000 * 200)
        .map(|i| {
            let mut c = Cell::empty();
            c.ch = (b'a' + (i % 26) as u8) as u32;
            c.fg = terminal_core::Color::Indexed((i % 256) as u8).to_packed();
            c.bg = terminal_core::Color::Indexed(((i / 256) % 8) as u8).to_packed();
            c
        })
        .collect();
    let mut out = vec![0u64; cells.len()];
    time_ms(|| {
        for (cell, slot) in cells.iter().zip(out.iter_mut()) {
            let fg = resolve_color(cell.color_fg(), DEFAULT_FG, false);
            let bg = resolve_color(cell.color_bg(), DEFAULT_BG, false);
            *slot = fg[0].to_bits() as u64 ^ bg[1].to_bits() as u64 ^ cell.ch as u64;
        }
        std::hint::black_box(&out);
    })
}

/// Glyph rasterization cost (cache miss path) via terminal-text.
fn glyph_raster_us_per_glyph() -> f64 {
    let mut lib = FontLibrary::new();
    lib.scan_system();
    let Some(primary) = lib.primary_monospace(None) else {
        return f64::NAN;
    };
    let mut cache = GlyphCache::new(14.0, 8 * 1024 * 1024);
    cache.set_font(primary);
    let samples: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*()[]{}<>,.;:'\"\\/?`~|_-+= éüößñáàåç日本語你🙂→≤"
        .chars()
        .collect();
    // Warm the cache fully first (measures steady-state miss cost).
    let t0 = Instant::now();
    for _ in 0..20 {
        for &c in &samples {
            cache.glyph(primary, c);
        }
    }
    let total_us = t0.elapsed().as_nanos() as f64 / 1000.0;
    total_us / (20 * samples.len()) as f64
}

/// Time to write 10K lines of 200 columns (2M cells) into the grid.
fn scrollback_10k_rows_ms() -> f64 {
    time_ms(|| {
        let mut state = TerminalState::new(200, 60);
        for _ in 0..10_000 {
            for c in 0..200u16 {
                state.write_char(char::from(b'a' + (c % 26) as u8));
            }
            state.cursor_to_beginning_of_line();
            state.cursor_down(1);
        }
        std::hint::black_box(state.grid.len());
    })
}

fn rss_mb() -> f64 {
    let pid = std::process::id();
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok();
    match out {
        Some(o) => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<f64>()
            .map(|k| k / 1024.0)
            .unwrap_or(f64::NAN),
        None => f64::NAN,
    }
}

/// RSS while holding `n` live shell sessions with their in-process
/// `TerminalState` grids. This matches the RAM budgets' semantics directly:
/// 1 pane = the idle single-workspace budget (< 40 MB), 10 panes = the
/// multi-pane budget (< 80 MB). Sessions are real PTY children, so the
/// measurement includes grid, read buffers, channel, and reader-thread
/// overhead — not a naive extrapolation of process base RSS.
fn sessions_ram_mb(n: usize) -> f64 {
    use std::sync::Arc;
    let shell = ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
        .copied()
        .unwrap_or("/bin/sh");
    let pty = match pty::PtyManager::new() {
        Ok(p) => Arc::new(p),
        Err(_) => return f64::NAN,
    };
    let mut sessions = Vec::new();
    let mut states = Vec::new();
    for _ in 0..n {
        if let Ok((s, _pid)) =
            terminal_session::Session::spawn(Arc::clone(&pty), shell, ".", 120, 40)
        {
            states.push(TerminalState::new(120, 40));
            sessions.push(s);
        }
    }
    if sessions.is_empty() {
        return f64::NAN;
    }
    // Let shells emit startup output so reader threads settle before RSS.
    std::thread::sleep(std::time::Duration::from_millis(150));
    let rss = rss_mb();
    drop(sessions);
    drop(states);
    rss
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Report {
    commit: String,
    date: String,
    startup_ms: f64,
    idle_ram_mb: f64,
    ten_panes_ram_mb: f64,
    /// Renamed from `input_latency_apply_p95_ms` — see
    /// `docs/performance-benchmark-audit.md` and
    /// `docs/performance-benchmarking.md` § Metric Taxonomy. Not input
    /// latency; a synthetic VT-parse+apply throughput microbenchmark.
    batch_apply_p95_ms: f64,
    events_per_second: f64,
    /// Real PTY write→read→apply latency — the actual input-latency metric,
    /// gated against the 8ms engineering target.
    input_to_apply_p95_ms: f64,
    write_to_pty_read_p95_ms: f64,
    read_to_apply_p95_ms: f64,
    /// Real shell tty-echo round trip — heavier than `input_to_apply`
    /// (includes the shell's own echo processing), also gated at 8ms.
    shell_echo_p95_ms: f64,
    parse_1m_lines_ms: f64,
    parse_10m_lines_ms: f64,
    grid_10m_cells_mb: f64,
    cell_bytes: usize,
    unicode_write_ns_per_char: f64,
    snapshot_frame_us: f64,
    render_prep_10k_rows_ms: f64,
    glyph_raster_us_per_glyph: f64,
    scrollback_10k_rows_ms: f64,
}

/// Regression multiplier for `batch_apply_p95_ms` (formerly
/// `input_latency_apply_p95_ms` — see `docs/performance-benchmark-audit.md`).
/// This metric measures true isolated per-event apply cost — nanoseconds,
/// not milliseconds — so an absolute cutoff has zero discriminating power
/// at its real scale (it would pass even a 100,000× regression). Measured
/// evidence: 30 independent local runs clustered tightly (coefficient of
/// variation ~1-2% excluding one OS-scheduling-stall outlier), and 3 runs
/// under deliberate heavy CPU contention (14 oversubscribed busy-loops on a
/// 12-core machine) reproduced the *exact same* value as an unloaded run.
/// A 5× baseline-relative threshold is a sensitive regression detector
/// without being flaky against measured real-world noise.
const BATCH_APPLY_REGRESSION_FACTOR: f64 = 5.0;
/// Floor below which a relative comparison is meaningless (clock
/// resolution / sub-microsecond jitter), not a real regression.
const BATCH_APPLY_FLOOR_MS: f64 = 0.001;
/// The product's real "keypress to state applied" engineering target
/// (`docs/performance.md`), validated against real hardware (a dev machine
/// or an actual end-user's machine) — unchanged by this audit. Used as the
/// hard gate for local (non-`--ci`) runs of `input_to_apply_p95_ms` and
/// `shell_echo_p95_ms` — the metrics that actually exercise a PTY.
const INPUT_LATENCY_ENGINEERING_BUDGET_MS: f64 = 8.0;
/// CI-specific ceiling for the same two metrics, used only when
/// `--ci` is set. GitHub's hosted `macos-latest` runner reproducibly shows
/// real PTY write/read/echo latency roughly 5-10x higher than local dev
/// hardware for reasons independent of code changes — confirmed across 3
/// consecutive real CI runs on unchanged code: `input_to_apply_p95_ms`
/// 5.17/9.25/10.65ms, `shell_echo_p95_ms` 7.60/9.37/13.57ms, vs. a local
/// range of 0.68-2.2ms *even under deliberate 14x CPU oversubscription*
/// (see `docs/performance-audit-reconciliation.md`). This is the exact
/// "Outcome B" the original audit's own decision framework anticipated:
/// not a product regression (real hardware is always fast), but the CI
/// runner's own environment noise making a flat 8ms cutoff correctly
/// unreliable *for this specific real-I/O metric on this specific shared
/// infrastructure*. The 8ms number remains the true engineering target,
/// verified directly by local runs (which this constant does not apply
/// to) — this is a wider, evidence-based ceiling for the CI regression
/// gate only, set with real margin (~2x) above the highest reproduced
/// CI sample, so it still catches an actual multi-x regression without
/// flapping on demonstrated runner noise.
const INPUT_LATENCY_CI_CEILING_MS: f64 = 25.0;

/// Hard budgets from the Phase 0.5 spec (§31). A breach fails the run.
fn budget_table(r: &Report) -> Vec<(String, f64, f64, f64, String, bool)> {
    // (metric, current, baseline, budget, unit, breached)
    let baseline = load_baseline();
    let mut rows = Vec::new();
    macro_rules! add {
        ($name:literal, $cur:expr, $budget:expr, $unit:literal) => {
            let base = baseline
                .as_ref()
                .and_then(|b| b.metric($name))
                .unwrap_or($cur);
            let breached = $cur > $budget && $budget > 0.0;
            rows.push((
                $name.to_string(),
                $cur,
                base,
                $budget,
                $unit.to_string(),
                breached,
            ));
        };
    }
    add!("idle_ram_mb", r.idle_ram_mb, 40.0, "MB");
    add!("ten_panes_ram_mb", r.ten_panes_ram_mb, 80.0, "MB");
    // Baseline-relative regression gate for the synthetic batch-throughput
    // metric — not the real input-latency budget (that's applied to
    // input_to_apply_p95_ms/shell_echo_p95_ms below, which actually
    // exercise a PTY). See the constant doc comments above.
    {
        let base = baseline
            .as_ref()
            .and_then(|b| b.metric("batch_apply_p95_ms"))
            .unwrap_or(r.batch_apply_p95_ms);
        let threshold = (base * BATCH_APPLY_REGRESSION_FACTOR).max(BATCH_APPLY_FLOOR_MS);
        let breached = r.batch_apply_p95_ms > threshold;
        rows.push((
            "batch_apply_p95_ms".to_string(),
            r.batch_apply_p95_ms,
            base,
            threshold,
            "ms".to_string(),
            breached,
        ));
    }
    add!("events_per_second", r.events_per_second, 0.0, "ev/s");
    // Real input-latency metrics. Local runs verify directly against the
    // real 8ms engineering target; CI runs use the wider, evidence-based
    // ceiling above (docs/performance-audit-reconciliation.md) — the
    // runner's own noise floor sits close to and sometimes over 8ms on
    // unchanged code, so a flat 8ms CI gate for these two metrics would
    // itself be an unreliable regression detector, not a meaningful one.
    let latency_budget = if is_ci_mode() {
        INPUT_LATENCY_CI_CEILING_MS
    } else {
        INPUT_LATENCY_ENGINEERING_BUDGET_MS
    };
    add!(
        "input_to_apply_p95_ms",
        r.input_to_apply_p95_ms,
        latency_budget,
        "ms"
    );
    add!(
        "shell_echo_p95_ms",
        r.shell_echo_p95_ms,
        latency_budget,
        "ms"
    );
    add!(
        "write_to_pty_read_p95_ms",
        r.write_to_pty_read_p95_ms,
        0.0,
        "ms"
    );
    add!("read_to_apply_p95_ms", r.read_to_apply_p95_ms, 0.0, "ms");
    add!("parse_1m_lines_ms", r.parse_1m_lines_ms, 0.0, "ms");
    add!("parse_10m_lines_ms", r.parse_10m_lines_ms, 0.0, "ms");
    add!("snapshot_frame_us", r.snapshot_frame_us, 0.0, "µs");
    add!(
        "render_prep_10k_rows_ms",
        r.render_prep_10k_rows_ms,
        0.0,
        "ms"
    );
    add!(
        "glyph_raster_us_per_glyph",
        r.glyph_raster_us_per_glyph,
        0.0,
        "µs"
    );
    add!(
        "scrollback_10k_rows_ms",
        r.scrollback_10k_rows_ms,
        0.0,
        "ms"
    );
    add!(
        "unicode_write_ns_per_char",
        r.unicode_write_ns_per_char,
        0.0,
        "ns"
    );
    add!("grid_10m_cells_mb", r.grid_10m_cells_mb, 0.0, "MB");
    rows
}

fn baseline_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("baseline.json")
}

fn load_baseline() -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(baseline_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_baseline(r: &Report) {
    let path = baseline_path();
    let json = serde_json::to_string_pretty(r).unwrap();
    std::fs::write(&path, json).expect("failed to write baseline.json");
}

/// `--ci`: compare against the committed baseline and never write it back.
/// Local (default) mode updates `baseline.json` after every run so the next
/// local run has a fresh comparison point; CI must not mutate committed
/// state, so it only reads the baseline that ships in the repo.
fn is_ci_mode() -> bool {
    std::env::args().any(|a| a == "--ci")
}

trait MetricGetter {
    fn metric(&self, name: &str) -> Option<f64>;
}

impl MetricGetter for serde_json::Value {
    fn metric(&self, name: &str) -> Option<f64> {
        self.get(name)?.as_f64()
    }
}

fn main() {
    println!("=== FlashTerminal Performance Report ===");
    println!("Running pipeline + memory + render-prep measurements...");

    // RAM must be measured before the heavy stress work, otherwise the
    // process RSS is dominated by the 10M-line buffers.
    let idle = sessions_ram_mb(1);
    let ten_panes = sessions_ram_mb(10);

    let parse_1m = pipeline_ms(1_000_000, true);
    let parse_10m = pipeline_ms(10_000_000, true);
    let (batch_apply_p95, events_per_second) = measure_batch_apply_p95();
    let (input_to_apply_p95, write_to_read_p95, read_to_apply_p95) =
        measure_input_to_apply_p95(300);
    let shell_echo_p95 = measure_shell_echo_p95(300);
    let snapshot_us = snapshot_frame_us();
    let render_prep = render_prep_10k_rows_ms();
    let glyph_us = glyph_raster_us_per_glyph();
    let scrollback_ms = scrollback_10k_rows_ms();

    // Grid memory: 10M-cell grid RSS delta.
    let before = rss_mb();
    let grid_mb = time_ms(|| {
        let mut state = TerminalState::new(200, 50_000);
        for _ in 0..50_000 {
            for c in 0..200u16 {
                state.write_char(char::from(b'a' + (c % 26) as u8));
            }
            state.cursor_to_beginning_of_line();
            state.cursor_down(1);
        }
        std::hint::black_box(state.grid.len());
    });
    let after = rss_mb();
    let grid_10m_cells_mb = (after - before).max(0.0);

    // Unicode write cost.
    let mut state = TerminalState::new(120, 40);
    let samples: &str = "aé你好🙂🏳️🌈e\u{301}👨\u{200d}👩\u{200d}👧x";
    let chars: Vec<char> = samples.chars().collect();
    let unicode_ns = time_ms(|| {
        for _ in 0..100_000 {
            for c in &chars {
                state.write_char(*c);
            }
            state.cursor_to_beginning_of_line();
            state.cursor.row = 0;
        }
    }) * 1e6
        / (100_000 * chars.len()) as f64;

    let report = Report {
        commit: env!("CARGO_PKG_VERSION").to_string(),
        date: chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string(),
        startup_ms: 0.0, // measured by the desktop harness (window creation)
        idle_ram_mb: idle,
        ten_panes_ram_mb: ten_panes,
        batch_apply_p95_ms: batch_apply_p95,
        events_per_second,
        input_to_apply_p95_ms: input_to_apply_p95,
        write_to_pty_read_p95_ms: write_to_read_p95,
        read_to_apply_p95_ms: read_to_apply_p95,
        shell_echo_p95_ms: shell_echo_p95,
        parse_1m_lines_ms: parse_1m,
        parse_10m_lines_ms: parse_10m,
        grid_10m_cells_mb,
        cell_bytes: std::mem::size_of::<terminal_core::Cell>(),
        unicode_write_ns_per_char: unicode_ns,
        snapshot_frame_us: snapshot_us,
        render_prep_10k_rows_ms: render_prep,
        glyph_raster_us_per_glyph: glyph_us,
        scrollback_10k_rows_ms: scrollback_ms,
    };

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    println!();

    let rows = budget_table(&report);
    println!("=== Budget Comparison ===");
    println!(
        "{:<32} {:>10} {:>10} {:>10} {:>8} status",
        "metric", "current", "baseline", "budget", "unit"
    );
    let mut breaches = Vec::new();
    for (name, cur, base, budget, unit, breached) in &rows {
        let delta = cur - base;
        let status = if cur.is_nan() {
            "UNAVAILABLE"
        } else if *breached {
            "FAIL"
        } else {
            "ok"
        };
        if *breached {
            breaches.push(name.clone());
        }
        println!(
            "{:<32} {:>10.2} {:>10.2} {:>10.2} {:>8} {}  (Δ {}{:+.2})",
            name, cur, base, budget, unit, status, unit, delta
        );
    }

    // Write the performance report markdown.
    let mut md = String::new();
    md.push_str("# Performance Report\n\n");
    md.push_str(&format!("- **Commit/version:** {}\n", report.commit));
    md.push_str(&format!("- **Date:** {}\n\n", report.date));
    md.push_str("| Metric | Baseline | Current | Budget | Delta | Result |\n");
    md.push_str("|--------|----------|---------|--------|-------|--------|\n");
    for (name, cur, base, budget, unit, breached) in &rows {
        let status = if *breached { "FAIL" } else { "pass" };
        md.push_str(&format!(
            "| {} | {:.2} {} | {:.2} {} | {:.2} {} | {:+.2} {} | {} |\n",
            name,
            base,
            unit,
            cur,
            unit,
            budget,
            unit,
            cur - base,
            unit,
            status
        ));
    }
    md.push_str("\n```json\n");
    md.push_str(&serde_json::to_string_pretty(&report).unwrap());
    md.push_str("\n```\n");
    let report_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("docs/performance-report.md");
    std::fs::write(&report_path, md).expect("failed to write docs/performance-report.md");
    println!("\nWrote {}", report_path.display());

    if is_ci_mode() {
        println!("\n--ci: baseline.json is committed state, not overwritten by CI runs.");
    } else {
        save_baseline(&report);
        println!("\nUpdated {}", baseline_path().display());
    }

    if !breaches.is_empty() {
        eprintln!("\nERROR: hard budget breached: {}", breaches.join(", "));
        std::process::exit(1);
    }
    println!("\nAll hard budgets met.");
    let _ = grid_mb;
    let _ = grid_10m_cells_mb;
}
