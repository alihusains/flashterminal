//! Phase 1 multiplexer benchmark (§25–26, §37).
//!
//! Measures, headlessly, against the real `terminal-workspace` engine:
//!
//! - workspace creation, tab creation, pane split, focus switch, layout cost
//! - pane scaling: 1 / 5 / 10 / 20 / 50 panes (RSS + per-pane state memory)
//! - 20-pane mixed stress (5 idle / 5 moderate / 5 heavy / 5 interactive)
//!   with focused-pane input latency while background panes flood
//!   (fairness, §27)
//! - state batching: events/s and apply-latency p95 (§22–23)
//!
//! Phase 3F hardening (§12–14):
//!
//! - The heavy-pane flood is **deterministic by default**: instead of
//!   launching `yes` (an infinite, scheduler-dependent flood whose total
//!   output volume is unbounded), the bench writes a fixed amount of
//!   pre-generated output into each heavy pane. The flood is bounded, so
//!   the stress section always terminates regardless of machine speed.
//!   `--flood yes` restores the old infinite-flood mode for A/B runs.
//! - A **watchdog thread** tracks last-progress timestamps (last drain,
//!   last state application, last PTY read, last frame). If the bench
//!   stops making progress for more than `--stall-ms` (default 5 s), it
//!   prints a diagnostic snapshot (per-session pending/stat counters,
//!   engine metrics, thread states) and exits non-zero instead of
//!   hanging forever.
//! - Every measurement loop is bounded by both wall-clock and an
//!   iteration cap, so a slow machine degrades numbers but never wedges
//!   the harness.
//! - `--stall-probe-secs N` runs a **deterministic watchdog proof** (§14)
//!   before the real sections: on a dedicated mini engine the bench makes
//!   progress, then deliberately halts (engine lock held, zero ticks) for
//!   the requested duration and asserts the monitor detects the stall and
//!   sets its fatal flag. No timing races — the halt is explicit.
//! - Phase 3F §38 **orchestration scaling**: 1 / 5 / 10 / 20 workflows
//!   (3 tasks each) exercising task creation, live fake-agent spawns,
//!   timeline/dashboard/summary reads, and replan signals under the
//!   focused-pane input probe.
//! - Phase 3F §39 **agent flood**: 10 live fake-agent sessions, each fed
//!   a deterministic 1000 lines/s flood, while the focused pane's echo
//!   latency is measured (target p95 < 8 ms — the agent system must never
//!   dominate the terminal render path).
//!
//! Usage: `cargo run --release -p benchmarks --bin multiplex_bench [secs] [--flood yes|fixture] [--stall-ms N] [--stall-probe-secs N]`

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use terminal_workspace::terminal_session::adapters::find_fake_agent_bin;
use terminal_workspace::terminal_session::launch::AgentLaunchConfig;
use terminal_workspace::terminal_session::work::AgentFilter;
use terminal_workspace::{Multiplexer, Rect, SplitDirection};

/// Deterministic heavy-pane fixture: 150K lines × ~36 bytes ≈ 5.4 MB per
/// heavy pane — enough to keep the reader/drain pipeline saturated for the
/// duration of a stress window without unbounded memory growth.
const FIXTURE_LINES_PER_PANE: usize = 150_000;
const FIXTURE_LINE: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz0123456789\n"; // 37 bytes

// ---------------------------------------------------------------------------
// Watchdog (§14): last-progress timestamps + stall diagnostics
// ---------------------------------------------------------------------------

struct Watchdog {
    /// Monotonic millis of the last observed progress (any of drain / state
    /// apply / PTY read / render).
    last_progress_ms: AtomicU64,
    /// Fatal flag: set by the monitor when a stall is detected; the main
    /// thread polls it and aborts the stress loop.
    stalled: AtomicBool,
    /// Asked to exit: the monitor drops its engine handle so the section's
    /// engine can be freed (a leaked monitor otherwise keeps every previous
    /// section's engine + sessions alive for the whole bench run).
    stop: AtomicBool,
}

impl Watchdog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            last_progress_ms: AtomicU64::new(now_ms()),
            stalled: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        })
    }

    /// Called from the stress loop on every drain (cheap atomic store).
    fn tick(&self) {
        self.last_progress_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn is_stalled(&self) -> bool {
        self.stalled.load(Ordering::Relaxed)
    }

    /// Asks the monitor to exit (takes effect within one 250 ms poll).
    fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Spawns the monitor thread. `snapshot` produces the diagnostic dump.
    fn spawn(self: &Arc<Self>, stall_ms: u64, snapshot: impl Fn() -> String + Send + 'static) {
        let wd = Arc::clone(self);
        std::thread::Builder::new()
            .name("bench-watchdog".into())
            .spawn(move || loop {
                if wd.stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
                let last = wd.last_progress_ms.load(Ordering::Relaxed);
                if now_ms().saturating_sub(last) >= stall_ms {
                    eprintln!(
                        "\nWATCHDOG: no progress for {} ms (last progress {} ms ago). \
                             Dumping diagnostic snapshot:\n{}",
                        stall_ms,
                        now_ms().saturating_sub(last),
                        snapshot()
                    );
                    wd.stalled.store(true, Ordering::SeqCst);
                    break;
                }
            })
            .expect("spawn watchdog thread");
    }
}

/// macOS caps the system PTY pool at `kern.tty.ptmx_max` (511 by default).
/// When the pool is exhausted `openpty` fails with ENXIO; under parallel
/// bench/test load this is a *harness* failure mode (Phase 3F §12), not an
/// engine bug. We read the limit so the harness can degrade gracefully
/// instead of unwrap-panicking on an environmental resource error.
fn ptmx_max() -> u64 {
    if let Ok(out) = std::process::Command::new("sysctl")
        .args(["-n", "kern.tty.ptmx_max"])
        .output()
    {
        if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            return n;
        }
    }
    511 // macOS default when sysctl is unavailable
}

/// Creates a workspace, mapping PTY-pool exhaustion to a clear diagnostic
/// instead of a raw unwrap panic. Returns `Err` with an explicit note when
/// the pool is exhausted.
fn create_ws(m: &mut Multiplexer, name: &str, cwd: &str, ptmx: u64) -> anyhow::Result<()> {
    match m.create_workspace(name, cwd) {
        Ok(_) => Ok(()),
        Err(e) => {
            // Full cause chain ({e:#}) — the openpty ENXIO is the root cause.
            let msg = format!("{e:#}");
            if msg.contains("openpty") || msg.contains("Device not configured") {
                anyhow::bail!(
                    "PTY pool exhausted (kern.tty.ptmx_max={ptmx}): {e:#} — \
                     reduce concurrent bench/test parallelism",
                )
            }
            Err(e)
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Per-session + engine counters for the stall diagnostic.
fn diagnostic_snapshot(m: &Multiplexer) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  engine: events_applied={} events/s={:.0} apply_p95_us={:.2} sessions={}\n",
        m.metrics.events_applied,
        m.metrics.events_per_second(),
        m.metrics.apply_latency_p95_us(),
        m.terminal_session_count()
    ));
    if let Some(tab) = m.active_tab() {
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        for p in panes {
            if let Some(s) = m.terminal_session_for_pane(&p.id) {
                let st = s.stats();
                out.push_str(&format!(
                    "  pane {}: pending_batches={} bytes_read={} events_read={} batches={} applied={} exited={}\n",
                    &p.id[..p.id.len().min(8)],
                    s.pending_len(),
                    st.bytes_read.load(Ordering::Relaxed),
                    st.events_read.load(Ordering::Relaxed),
                    st.batches.load(Ordering::Relaxed),
                    st.applied_batches.load(Ordering::Relaxed),
                    s.has_exited()
                ));
            }
        }
    }
    out.push_str("  threads:\n");
    // macOS: `ps -M -p <pid>` lists per-thread state (running/ sleeping /
    // uninterruptible etc.). Best-effort; may be empty on other platforms.
    if let Ok(o) = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
    {
        out.push_str(&String::from_utf8_lossy(&o.stdout));
    }
    out
}

fn tree_rss_kb() -> u64 {
    use std::collections::HashMap;
    fn sum(
        pid: i64,
        c: &HashMap<i64, Vec<i64>>,
        r: &HashMap<i64, f64>,
        seen: &mut std::collections::HashSet<i64>,
    ) -> f64 {
        if !seen.insert(pid) {
            return 0.0;
        }
        let mut t = r.get(&pid).copied().unwrap_or(0.0);
        for ch in c.get(&pid).into_iter().flatten() {
            t += sum(*ch, c, r, seen);
        }
        t
    }
    let mut c: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut r: HashMap<i64, f64> = HashMap::new();
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .output()
    {
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = l.split_whitespace();
            if let (Some(p), Some(pp), Some(rs)) = (it.next(), it.next(), it.next()) {
                if let (Ok(p), Ok(pp), Ok(rs)) =
                    (p.parse::<i64>(), pp.parse::<i64>(), rs.parse::<f64>())
                {
                    c.entry(pp).or_default().push(p);
                    r.insert(p, rs);
                }
            }
        }
    }
    sum(std::process::id() as i64, &c, &r, &mut Default::default()) as u64
}

fn state_bytes(m: &Multiplexer) -> u64 {
    // Sum retained memory across the active workspace's panes.
    let mut total = 0u64;
    if let Some(tab) = m.active_tab() {
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        for p in panes {
            if let Some(st) = m.state_for_pane(&p.id) {
                total += st.retained_memory() as u64;
            }
        }
    }
    total
}

/// One focused-input sample against a busy engine: write a byte to
/// `pane_id`, drain until that pane's applied-batch counter advances or the
/// bounded caps (100 ms / 10k drains) expire. `None` means the echo cap was
/// hit — a stall candidate, counted separately from real measurements
/// (Phase 3F §13).
fn input_sample(m: &std::sync::Mutex<Multiplexer>, pane_id: &str) -> Option<std::time::Duration> {
    let before = {
        let m = m.lock().unwrap();
        let session = m.terminal_session_for_pane(&pane_id.to_string())?;
        session.write(b"a");
        session.stats().applied_batches.load(Ordering::Relaxed)
    };
    let t0 = Instant::now();
    let mut drains = 0u64;
    loop {
        {
            let mut m = m.lock().unwrap();
            m.drain_frame();
        }
        drains += 1;
        let done = {
            let m = m.lock().unwrap();
            m.terminal_session_for_pane(&pane_id.to_string())
                .map(|s| s.stats().applied_batches.load(Ordering::Relaxed))
                .unwrap_or(before)
                > before
        };
        if done || t0.elapsed() > Duration::from_millis(100) || drains > 10_000 {
            return done.then_some(t0.elapsed());
        }
        // Yield: the desktop event loop wakes on drain callbacks, so a
        // 100%-CPU spin here would starve the reader threads and inflate
        // the measured latency.
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// A percentile (ms) of sorted input latencies, NaN when empty.
fn pct_ms(sorted: &[Duration], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let idx = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
    sorted[idx].as_secs_f64() * 1e3
}

fn main() {
    let ptmx = ptmx_max();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut secs: u64 = 20;
    let mut flood = "fixture";
    let mut stall_ms: u64 = 5_000;
    let mut stall_probe_secs: u64 = 0;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--flood" => flood = it.next().map(|s| s.as_str()).unwrap_or("fixture"),
            "--stall-ms" => stall_ms = it.next().and_then(|s| s.parse().ok()).unwrap_or(stall_ms),
            "--stall-probe-secs" => {
                stall_probe_secs = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(stall_probe_secs)
            }
            other => {
                if let Ok(n) = other.parse::<u64>() {
                    secs = n;
                }
            }
        }
    }
    let outer = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    println!("=== FlashTerminal Phase 1 multiplexer benchmark ===");
    println!(
        "mode: secs={secs} flood={flood} stall-ms={stall_ms} stall-probe-secs={stall_probe_secs} \
         ptmx_max={ptmx} (Phase 3F: deterministic + watchdog)"
    );

    // ---- workspace creation -------------------------------------------------
    {
        let mut m = Multiplexer::new().unwrap();
        // Hold at most this many live PTYs at once so concurrent benches or
        // a running test suite cannot exhaust the system pool (the original
        // 100-workstation section aborted under parallel load).
        let n = 100usize.min((ptmx / 3).max(8) as usize);
        let mut failures = 0usize;
        let t0 = Instant::now();
        for i in 0..n {
            match create_ws(&mut m, &format!("ws{i}"), "/tmp", ptmx) {
                Ok(_) => {}
                Err(e) => {
                    failures += 1;
                    if failures >= 4 {
                        eprintln!("ABORT: {e}");
                        std::process::exit(3);
                    }
                }
            }
        }
        let created = n - failures;
        let el = t0.elapsed() / created.max(1) as u32;
        println!(
            "workspace create: {created}x total {:?}, avg {:?} (<100 ms target: {})",
            t0.elapsed(),
            el,
            if el < Duration::from_millis(100) {
                "PASS"
            } else {
                "FAIL"
            }
        );
        if failures > 0 {
            println!("  note: {failures} creates skipped (PTY pool pressure, ptmx_max={ptmx})");
        }
        println!("  (spawns a real shell each — dominated by process spawn)");
    }

    // ---- Phase 3F §14: deterministic watchdog proof -------------------------
    // `--stall-probe-secs N` makes the bench *deliberately* halt all
    // progress on a dedicated mini engine (engine lock held, zero ticks and
    // drains) for the requested duration and asserts the monitor detects
    // the stall and raises its fatal flag. The halt is explicit, so the
    // proof is deterministic — no clock races, no scheduling luck.
    if stall_probe_secs > 0 {
        println!(
            "\n=== watchdog proof (--stall-probe-secs {stall_probe_secs}, --stall-ms {stall_ms}) ==="
        );
        let probe = Arc::new(std::sync::Mutex::new(Multiplexer::new().unwrap()));
        {
            let mut eng = probe.lock().unwrap();
            if let Err(e) = create_ws(&mut eng, "wdproof", "/tmp", ptmx) {
                eprintln!("ABORT: {e}");
                std::process::exit(3);
            }
        }
        let wd = Watchdog::new();
        {
            let p = Arc::clone(&probe);
            wd.spawn(stall_ms, move || match p.try_lock() {
                Ok(eng) => diagnostic_snapshot(&eng),
                Err(_) => "  (engine lock held by the probe — expected: the halt holds the lock)"
                    .to_string(),
            });
        }
        // Establish progress (fresh last-progress timestamps)…
        {
            let mut eng = probe.lock().unwrap();
            for _ in 0..10 {
                eng.drain_frame();
                wd.tick();
            }
        }
        // …then halt for at least the stall threshold + 1 s so detection is
        // guaranteed for any configured stall_ms.
        let halt_secs = stall_probe_secs.max(stall_ms / 1000 + 1);
        let halt_start = Instant::now();
        {
            let _guard = probe.lock().unwrap();
            std::thread::sleep(Duration::from_secs(halt_secs));
        }
        // The monitor polls every 250 ms: detection lands within
        // stall_ms + monitor slop; allow a generous 3 s of slop.
        let deadline = halt_start + Duration::from_millis(stall_ms + 3000);
        let mut fired = wd.is_stalled();
        while !fired && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            fired = wd.is_stalled();
        }
        let observed_in = halt_start.elapsed();
        println!(
            "watchdog proof: {} — {halt_secs}s deliberate halt (threshold {stall_ms} ms), fatal flag observed {:.1}s after halt start",
            if fired { "PASS" } else { "FAIL" },
            observed_in.as_secs_f64()
        );
        if !fired {
            eprintln!("ABORT: watchdog did not fire — deadlock detection is broken");
            std::process::exit(1);
        }
        // Stop the monitor so the probe engine is freed.
        wd.stop();
        drop(probe);
    }

    // ---- tab + split + focus latencies -------------------------------------
    {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("bench", "/tmp").unwrap();
        let t0 = Instant::now();
        for _ in 0..50 {
            m.new_tab().unwrap();
        }
        let el = t0.elapsed() / 50;
        println!(
            "tab create: 50x avg {:?} (<100 ms: {})",
            el,
            if el < Duration::from_millis(100) {
                "PASS"
            } else {
                "FAIL"
            }
        );

        // A single tab with a 50-pane binary tree: alternate splits.
        m.switch_tab(m.active_tab_id().unwrap().as_str()).unwrap();
        // close the extra tabs we made? keep 1: close until 1 remains.
        while m.active_workspace().tabs.len() > 1 {
            let id = m.active_workspace().tabs[1].id.clone();
            m.close_tab(&id).unwrap();
        }
        let t0 = Instant::now();
        for i in 0..49 {
            let dir = if i % 2 == 0 {
                SplitDirection::Horizontal
            } else {
                SplitDirection::Vertical
            };
            m.split_pane(dir).unwrap();
        }
        let el = t0.elapsed() / 49;
        println!(
            "pane split: 49x avg {:?} (<30 ms: {})",
            el,
            if el < Duration::from_millis(30) {
                "PASS"
            } else {
                "FAIL"
            }
        );
        assert_eq!(m.active_workspace().tabs[0].root.pane_count(), 50);

        // layout cost for 50 panes
        let t0 = Instant::now();
        let iters = 10_000u32;
        for _ in 0..iters {
            let _l = m.layout_active(outer);
        }
        println!(
            "layout 50 panes: {:?}/layout (<5 ms: {})",
            t0.elapsed() / iters,
            t0.elapsed() / iters < Duration::from_millis(5)
        );

        // focus switching: cycle all 50
        let t0 = Instant::now();
        for _ in 0..50 {
            m.focus_next().unwrap();
        }
        println!(
            "focus switch: 50x avg {:?} (<10 ms: {})",
            t0.elapsed() / 50,
            t0.elapsed() / 50 < Duration::from_millis(10)
        );

        // resize a pane
        let pane = m.focused_pane().unwrap();
        let t0 = Instant::now();
        for _ in 0..100 {
            m.resize_pane(&pane, 10.0).unwrap();
        }
        println!("resize pane: 100x avg {:?}", t0.elapsed() / 100);

        // scaling memory snapshot at 50 panes
        println!(
            "scaling 50 panes: tree RSS {:.1} MB, state {:.1} MB",
            tree_rss_kb() as f64 / 1024.0,
            state_bytes(&m) as f64 / 1e6
        );
    }

    // ---- 1/5/10/20/50 pane scaling (fresh engines) --------------------------
    for panes in [1usize, 5, 10, 20, 50] {
        let mut m = Multiplexer::new().unwrap();
        if let Err(e) = create_ws(&mut m, "scale", "/tmp", ptmx) {
            eprintln!("ABORT: {e}");
            std::process::exit(3);
        }
        for _ in 1..panes {
            if let Err(e) = m.split_pane(SplitDirection::Horizontal) {
                eprintln!("ABORT: {e}");
                std::process::exit(3);
            }
        }
        // settle + drain (bounded: wall-clock AND iteration cap)
        let t0 = Instant::now();
        let mut drained = 0u64;
        while t0.elapsed() < Duration::from_millis(800) && drained < 10_000 {
            m.drain_frame();
            drained += 1;
            std::thread::sleep(Duration::from_millis(5));
        }
        println!(
            "scale {panes:>2} panes: tree RSS {:.1} MB, state {:.2} MB, sessions {}",
            tree_rss_kb() as f64 / 1024.0,
            state_bytes(&m) as f64 / 1e6,
            m.terminal_session_count()
        );
    }

    // ---- 20-pane mixed stress + fairness (§26, §27) -------------------------
    // The engine is shared behind a mutex so the watchdog thread can take a
    // diagnostic snapshot if progress stalls (§14).
    {
        let m = Arc::new(std::sync::Mutex::new(Multiplexer::new().unwrap()));
        {
            let mut eng = m.lock().unwrap();
            if let Err(e) = create_ws(&mut eng, "stress", "/tmp", ptmx) {
                eprintln!("ABORT: {e}");
                std::process::exit(3);
            }
            for _ in 1..20 {
                if let Err(e) = eng.split_pane(SplitDirection::Vertical) {
                    eprintln!("ABORT: {e}");
                    std::process::exit(3);
                }
            }
        }
        // Owned (pane_id, session_id) pairs so they outlive the borrow.
        let mut panes: Vec<(String, String)> = Vec::new();
        if let Some(tab) = m.lock().unwrap().active_tab().cloned() {
            let mut refs = Vec::new();
            tab.root.panes(&mut refs);
            for p in refs {
                panes.push((p.id.clone(), p.execution_id.0.clone()));
            }
        }
        assert_eq!(panes.len(), 20);
        // 5 idle, 5 moderate, 5 heavy, 5 interactive (by index)
        let workloads: Vec<u8> = (0..20)
            .map(|i| match i {
                0..=4 => 0,   // idle
                5..=9 => 1,   // moderate
                10..=14 => 2, // heavy
                _ => 3,       // interactive
            })
            .collect();
        let focused = {
            let m = m.lock().unwrap();
            m.focused_pane().unwrap()
        };
        let start = Instant::now();

        // Flood the heavy panes. Deterministic fixture (Phase 3F §13): each
        // heavy pane receives a fixed volume of pre-generated output written
        // in bounded per-iteration chunks, so (a) the total work is identical
        // across machines and the section always terminates, and (b) the
        // flood is sustained across the whole window like the old `yes` mode.
        // `--flood yes` restores the infinite `yes` mode (kernel
        // backpressure, unbounded total) for A/B runs.
        let mut fixture: Option<(Vec<u8>, Vec<usize>)> = None;
        if flood == "fixture" {
            let fixture_bytes = FIXTURE_LINE.repeat(FIXTURE_LINES_PER_PANE);
            // Remaining fixture bytes per heavy pane (pane index → remaining).
            let mut remaining = vec![0usize; panes.len()];
            for (i, wl) in workloads.iter().enumerate() {
                if *wl == 2 {
                    remaining[i] = fixture_bytes.len();
                }
            }
            fixture = Some((fixture_bytes, remaining));
        } else {
            let m = m.lock().unwrap();
            for (i, p) in panes.iter().enumerate() {
                if workloads[i] == 2 {
                    if let Some(s) = m.terminal_session_for_pane(&p.0) {
                        s.write(b"yes 0123456789abcdefghijklmnopqrstuvwxyz\n");
                    }
                }
            }
        }

        // Watchdog (§14): any stall longer than `stall_ms` dumps a snapshot
        // and aborts instead of hanging. It only *tries* to lock the engine
        // (a stuck `drain_frame` would hold the lock — the thread-state dump
        // still prints), so the watchdog itself can never deadlock.
        let wd = Watchdog::new();
        {
            let m = Arc::clone(&m);
            wd.spawn(stall_ms, move || match m.try_lock() {
                Ok(eng) => diagnostic_snapshot(&eng),
                Err(_) => {
                    let mut out = String::new();
                    out.push_str(
                        "  (engine lock held by main thread — likely stuck in drain_frame/apply)\n",
                    );
                    if let Ok(o) = std::process::Command::new("ps")
                        .args(["-M", "-p", &std::process::id().to_string()])
                        .output()
                    {
                        out.push_str(&String::from_utf8_lossy(&o.stdout));
                    }
                    out
                }
            });
        }

        // input latency to the FOCUSED pane while heavy panes are busy
        let mut lats = Vec::new();
        let mut echo_timeouts = 0u64;
        // Did the focused pane's shell exit during the measurement window?
        // (If it did, "no echo" is a shell-lifecycle artifact of the
        // synthetic flood, not a renderer stall — the verdict reflects that.)
        let mut focused_exited = false;
        let mut t = 0usize;
        let deadline = start + Duration::from_secs(secs);
        // Hard iteration cap: even if the clock is weird, the section ends.
        let max_iters: usize = secs.saturating_mul(2_000).max(10_000) as usize;
        // Fixture chunk per iteration: total spread over ~500 iters/sec.
        let fixture_chunk = if let Some((b, _)) = &fixture {
            (b.len() / secs.saturating_mul(400).max(1) as usize).max(1024)
        } else {
            0
        };
        while Instant::now() < deadline && t < max_iters {
            if wd.is_stalled() {
                eprintln!("ABORT: watchdog detected a stall in the stress section");
                std::process::exit(1);
            }
            // deterministic heavy-pane flood: bounded chunk per iteration
            if let Some((bytes, remaining)) = &mut fixture {
                let m = m.lock().unwrap();
                for (i, p) in panes.iter().enumerate() {
                    if remaining[i] == 0 {
                        continue;
                    }
                    let n = remaining[i].min(fixture_chunk);
                    if let Some(s) = m.terminal_session_for_pane(&p.0) {
                        s.write(&bytes[..n]);
                    }
                    remaining[i] -= n;
                }
            }
            // interactive panes trickle. The shell sits in canonical mode:
            // bare bytes without a newline fill the line discipline's input
            // queue (~1 KB) and the tty stops echoing (Phase 3F §12: this
            // is what made the original bench bimodal — the focused pane's
            // echoes silently wedged). A periodic newline drains the queue
            // so echo latency measures the renderer, not line discipline.
            for (i, p) in panes.iter().enumerate() {
                if workloads[i] == 3 {
                    let m = m.lock().unwrap();
                    if let Some(s) = m.terminal_session_for_pane(&p.0) {
                        if t.is_multiple_of(100) {
                            s.write(b"x\n");
                        } else {
                            s.write(b"x");
                        }
                    }
                }
            }
            // focused input: write + measure until the FOCUSED pane's echo is
            // applied (per-session `applied_batches`, not any-pane `changed`
            // — otherwise heavy-pane output would understate the latency).
            let t0 = Instant::now();
            let focused_before = {
                let m = m.lock().unwrap();
                if let Some(s) = m.terminal_session_for_pane(&focused) {
                    s.write(b"a");
                    s.stats().applied_batches.load(Ordering::Relaxed)
                } else {
                    0
                }
            };
            let mut drains = 0u64;
            let echoed = loop {
                {
                    let mut m = m.lock().unwrap();
                    m.drain_frame();
                }
                drains += 1;
                wd.tick();
                let done = {
                    let m = m.lock().unwrap();
                    m.terminal_session_for_pane(&focused)
                        .map(|s| s.stats().applied_batches.load(Ordering::Relaxed))
                        .unwrap_or(focused_before)
                        > focused_before
                };
                if done || t0.elapsed() > Duration::from_millis(100) || drains > 10_000 {
                    break done;
                }
                // Yield instead of spinning: the desktop event loop waits on
                // the wake callback, so a 100%-CPU busy-wait here would
                // starve the reader threads and inflate the measured latency.
                std::thread::sleep(Duration::from_micros(200));
            };
            if echoed {
                lats.push(t0.elapsed());
            } else {
                // A sample that hit the echo cap is a *stall* candidate, not
                // a latency measurement — count it separately (Phase 3F §13:
                // separate perf measurement from timeout-sensitive behavior).
                if echo_timeouts == 0 {
                    // First-timeout forensic dump (§14): is the focused
                    // session wedged on the reader side (bytes_read frozen)
                    // or the apply side (bytes still arriving)?
                    let m = m.lock().unwrap();
                    if let Some(s) = m.terminal_session_for_pane(&focused) {
                        let st = s.stats();
                        eprintln!(
                            "[wedge-dump t={t}] focused {}: pending={} bytes_read={} \
                             events_read={} batches={} applied={} exited={}",
                            &focused[..focused.len().min(8)],
                            s.pending_len(),
                            st.bytes_read.load(Ordering::Relaxed),
                            st.events_read.load(Ordering::Relaxed),
                            st.batches.load(Ordering::Relaxed),
                            st.applied_batches.load(Ordering::Relaxed),
                            s.has_exited()
                        );
                        eprintln!(
                            "  engine: events_applied={} events/s={:.0} sessions={}",
                            m.metrics.events_applied,
                            m.metrics.events_per_second(),
                            m.terminal_session_count()
                        );
                    }
                }
                echo_timeouts += 1;
                // Did the pane's shell die while we were probing? Tracked so
                // the verdict can label a no-echo window as a fixture
                // lifecycle artifact instead of a renderer stall.
                if !focused_exited {
                    let m = m.lock().unwrap();
                    focused_exited = m
                        .terminal_session_for_pane(&focused)
                        .map(|s| s.has_exited())
                        .unwrap_or(false);
                }
            }
            if t.is_multiple_of(10) {
                let m = m.lock().unwrap();
                m.layout_active(outer);
            }
            t += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        wd.tick();
        wd.stop();

        let mut sorted = lats.clone();
        sorted.sort_unstable();
        let p = |q: f64| {
            if sorted.is_empty() {
                return f64::NAN;
            }
            let idx = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
            sorted[idx].as_secs_f64() * 1e3
        };
        let m = m.lock().unwrap();
        let eps = m.metrics.events_per_second();
        let timeout_rate =
            echo_timeouts as f64 / (lats.len() + echo_timeouts as usize).max(1) as f64;
        let latency_ok = sorted.len() >= 10 && p(0.95) < 8.0;
        // A sustained echo-stall rate (>5% of samples never echoing within
        // 100 ms) means the pipeline is wedged under load — fail that.
        // Exception: if the focused pane's *shell itself* exited under the
        // synthetic flood, there is no echo target left; that is a fixture
        // lifecycle artifact, diagnosed by the wedge-dump above, not proof
        // the render path stalled (the p50/p95 above still measure it).
        let stall_ok = timeout_rate <= 0.05;
        let verdict = if latency_ok && stall_ok {
            "PASS"
        } else if !stall_ok && focused_exited {
            "FAIL (focused shell exited under flood — fixture artifact, see wedge-dump; renderer latency still measured)"
        } else if !stall_ok {
            "FAIL (echo stall rate)"
        } else {
            "FAIL"
        };
        println!(
            "stress 20 panes (5/5/5/5): focused input p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms \
             echo_timeouts {echo_timeouts} ({:.1}%) (<8 ms target: {verdict})",
            p(0.50),
            p(0.95),
            p(0.99),
            sorted
                .last()
                .map(|d| d.as_secs_f64() * 1e3)
                .unwrap_or(f64::NAN),
            timeout_rate * 100.0,
        );
        println!(
            "state batching: {:.0} events/s, apply-latency p95 {:.2} µs/frame, {} events applied total",
            eps,
            m.metrics.apply_latency_p95_us(),
            m.metrics.events_applied
        );
        println!(
            "stress RSS {:.1} MB, state {:.1} MB, sessions {}",
            tree_rss_kb() as f64 / 1024.0,
            state_bytes(&m) as f64 / 1e6,
            m.terminal_session_count()
        );
    }

    // ---- Phase 3F §38: orchestration scaling (1/5/10/20 workflows) ---------
    // Each workflow owns 3 live fake-agent tasks ("multiple agents each")
    // with FAKE_AGENT_SCENARIO=completion so they actually run and complete
    // through the scheduler, bus and timeline. Every measurement row covers
    // create µs, event throughput, state-apply latency, focused input p95,
    // dashboard/summary/replan-signal latency and RSS. The non-git bench
    // root makes GitWorktree isolation degrade gracefully to the shared
    // workspace (documented engine behavior), so no git subprocess churn.
    {
        println!("\n=== orchestration scaling (§38) ===");
        let mut rows = Vec::new();
        for workflow_count in [1usize, 5, 10, 20] {
            let task_count = workflow_count * 3;
            let m = Arc::new(std::sync::Mutex::new(Multiplexer::new().unwrap()));
            {
                let mut eng = m.lock().unwrap();
                if let Err(e) = create_ws(&mut eng, "wfscale", "/tmp", ptmx) {
                    eprintln!("ABORT: {e}");
                    std::process::exit(3);
                }
                let mut policy = eng.task_policy();
                policy.max_parallel_tasks = task_count.max(4);
                policy.max_agents = task_count.max(4);
                eng.set_task_policy(policy);
            }
            let ws_id = {
                let eng = m.lock().unwrap();
                eng.workspaces()[0].id.clone()
            };
            let mut task_ids = Vec::new();
            let t0 = Instant::now();
            {
                let mut eng = m.lock().unwrap();
                for wf in 0..workflow_count {
                    for task in 0..3 {
                        let id = eng
                            .task_create(
                                &ws_id,
                                &format!("wf{wf}-task{task}"),
                                "§38 orchestration scaling bench",
                                "fake-agent",
                                &[],
                                false,
                            )
                            .expect("task create");
                        eng.task_set_environment(
                            &id,
                            &[("FAKE_AGENT_SCENARIO".to_string(), "completion".to_string())],
                        )
                        .expect("task env");
                        task_ids.push(id);
                    }
                }
            }
            let create_el = t0.elapsed();
            let create_us = create_el.as_secs_f64() * 1e6 / task_count as f64;

            // Schedule the whole graph. Spawning N×3 real agents takes
            // seconds for the largest row — a legitimate burst, so the
            // watchdog for this section uses a generous window and starts
            // AFTER the burst (its job is deadlock detection over the
            // measurement window, not bounding legitimate spawn work).
            let t_sched = Instant::now();
            {
                let mut eng = m.lock().unwrap();
                eng.task_run();
            }
            let sched_millis = t_sched.elapsed().as_secs_f64() * 1e3;

            // Fixed measurement window with the input probe, dashboard,
            // summary and replan-signal latency samples.
            let wd = Watchdog::new();
            {
                let m2 = Arc::clone(&m);
                wd.spawn(stall_ms.max(10_000), move || match m2.try_lock() {
                    Ok(eng) => diagnostic_snapshot(&eng),
                    Err(_) => {
                        let mut out = String::new();
                        out.push_str("  (engine lock held by main thread)\n");
                        out
                    }
                });
            }
            let focused = {
                let eng = m.lock().unwrap();
                eng.active_workspace()
                    .active_tab()
                    .unwrap()
                    .active_pane
                    .clone()
                    .unwrap()
            };
            let window = Duration::from_millis(3_000);
            let deadline = Instant::now() + window;
            let mut lats = Vec::new();
            let mut timeouts = 0u64;
            let mut replan_us_avg = 0.0f64;
            let mut dash_us_sum = 0.0f64;
            let mut dash_n = 0u64;
            let mut summary_us_sum = 0.0f64;
            let mut summary_n = 0u64;
            let mut iter = 0usize;
            while Instant::now() < deadline {
                if wd.is_stalled() {
                    eprintln!("ABORT: watchdog detected a stall in the scaling section");
                    std::process::exit(1);
                }
                if let Some(d) = input_sample(&m, &focused) {
                    lats.push(d);
                } else {
                    timeouts += 1;
                }
                if iter.is_multiple_of(50) {
                    let t = Instant::now();
                    let eng = m.lock().unwrap();
                    let _d = eng.agent_dashboard(AgentFilter::All);
                    dash_us_sum += t.elapsed().as_secs_f64() * 1e6;
                    dash_n += 1;
                    let t = Instant::now();
                    let _s = eng.workflow_summary();
                    summary_us_sum += t.elapsed().as_secs_f64() * 1e6;
                    summary_n += 1;
                }
                if iter.is_multiple_of(200) {
                    let t = Instant::now();
                    let mut eng = m.lock().unwrap();
                    eng.signal_replan("bench-scaling", &format!("{workflow_count} workflows"));
                    let replan_us = t.elapsed().as_secs_f64() * 1e6;
                    if iter == 0 {
                        replan_us_avg = replan_us;
                    }
                    replan_us_avg = replan_us_avg * 0.8 + replan_us * 0.2;
                }
                wd.tick();
                iter += 1;
            }
            wd.tick();

            let mut sorted = lats.clone();
            sorted.sort_unstable();
            let p95 = pct_ms(&sorted, 0.95);
            let eng = m.lock().unwrap();
            let eps = eng.metrics.events_per_second();
            let apply_us = eng.metrics.apply_latency_p95_us();
            let sched = eng.scheduler_status();
            let states: Vec<(String, usize)> = {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for (_, st) in &sched.states {
                    let label = st.label().to_string();
                    if let Some((_, n)) = counts.iter_mut().find(|(l, _)| *l == label) {
                        *n += 1;
                    } else {
                        counts.push((label, 1));
                    }
                }
                counts
            };
            drop(eng);
            wd.stop();
            let rss_mb = tree_rss_kb() as f64 / 1024.0;
            rows.push((
                workflow_count,
                p95,
                timeouts,
                lats.len(),
                eps,
                apply_us,
                create_us,
                sched_millis,
                dash_us_sum / dash_n.max(1) as f64,
                summary_us_sum / summary_n.max(1) as f64,
                replan_us_avg,
                rss_mb,
                states,
            ));
        }
        println!(
            "{:>7} | µs/create | sched ms | ev/s | apply µs | p95 ms | dash µs | sum µs | replan µs | RSS MB",
            "workflows"
        );
        for (
            n,
            p95,
            timeouts,
            samples,
            eps,
            apply_us,
            create_us,
            sched_ms,
            dash,
            sum,
            replan,
            rss,
            states,
        ) in &rows
        {
            let summary = states
                .iter()
                .map(|(st, c)| format!("{st}:{c}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!(
                "{n:>7} | {create_us:>8.1} | {sched_ms:>8.0} | {eps:>4.0} | {apply_us:>7.2} | {p95:>6.2} ({timeouts}/{samples} t/o) | {dash:>6.0} | {sum:>6.0} | {replan:>6.0} | {rss:>6.1} | [{summary}]"
            );
        }
        let pass = rows.iter().all(|(_, p95, timeouts, samples, ..)| {
            let rate = *timeouts as f64 / (*samples as f64 + *timeouts as f64).max(1.0);
            !p95.is_nan() && *p95 < 8.0 && rate <= 0.05
        });
        println!(
            "orchestration scaling verdict: {} (per row: input p95 < 8 ms, echo timeouts ≤ 5%)",
            if pass { "PASS" } else { "FAIL" }
        );
    }

    // ---- Phase 3F §39: agent flood (10 agents × 1000 events/s) -------------
    // Ten live fake-agent sessions self-generate a deterministic flood —
    // the `flood` scenario clocks exactly 1000 lines/s per agent for the
    // bench window — through the same PTY → reader → bus → drain path every
    // agent uses. The focused shell pane carries the input-latency probe.
    // Target (§39): interactive input p95 < 8 ms — the agent system must
    // never dominate the terminal render path.
    if find_fake_agent_bin().is_some() {
        println!("\n=== agent flood (§39) ===");
        let m = Arc::new(std::sync::Mutex::new(Multiplexer::new().unwrap()));
        {
            let mut eng = m.lock().unwrap();
            if let Err(e) = create_ws(&mut eng, "flood", "/tmp", ptmx) {
                eprintln!("ABORT: {e}");
                std::process::exit(3);
            }
        }
        let mut agent_panes = Vec::new();
        {
            let mut eng = m.lock().unwrap();
            for _ in 0..10 {
                let launch = AgentLaunchConfig {
                    definition_id: "fake-agent".to_string(),
                    cwd: "/tmp".to_string(),
                    arguments: vec![
                        "--scenario".to_string(),
                        "flood".to_string(),
                        "--duration".to_string(),
                        secs.to_string(),
                    ],
                    provider_id: None,
                    model_id: None,
                    credential_ref: None,
                    resume_id: None,
                    environment: Vec::new(),
                };
                match eng.split_pane_agent(SplitDirection::Vertical, launch) {
                    Ok(pane) => agent_panes.push(pane),
                    Err(e) => {
                        eprintln!("ABORT: agent flood spawn failed: {e:#}");
                        std::process::exit(3);
                    }
                }
            }
        }
        assert_eq!(agent_panes.len(), 10);
        let focused = {
            let mut eng = m.lock().unwrap();
            eng.split_pane(SplitDirection::Horizontal)
                .expect("probe pane")
        };
        let wd = Watchdog::new();
        {
            let m2 = Arc::clone(&m);
            wd.spawn(stall_ms.max(2_000), move || match m2.try_lock() {
                Ok(eng) => diagnostic_snapshot(&eng),
                Err(_) => {
                    let mut out = String::new();
                    out.push_str("  (engine lock held by main thread)\n");
                    out
                }
            });
        }
        let deathline = Instant::now() + Duration::from_secs(secs);
        let max_iters: usize = secs.saturating_mul(2_000).max(10_000) as usize;
        let mut lats = Vec::new();
        let mut timeouts = 0u64;
        let mut iter = 0usize;
        while Instant::now() < deathline && iter < max_iters {
            if wd.is_stalled() {
                eprintln!("ABORT: watchdog detected a stall in the flood section");
                std::process::exit(1);
            }
            if let Some(d) = input_sample(&m, &focused) {
                lats.push(d);
            } else {
                timeouts += 1;
            }
            if iter.is_multiple_of(10) {
                let eng = m.lock().unwrap();
                eng.layout_active(outer);
            }
            wd.tick();
            iter += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        wd.tick();
        wd.stop();
        let mut sorted = lats.clone();
        sorted.sort_unstable();
        let eng = m.lock().unwrap();
        let eps = eng.metrics.events_per_second();
        let apply_us = eng.metrics.apply_latency_p95_us();
        let sessions = eng.terminal_session_count();
        drop(eng);
        let p50 = pct_ms(&sorted, 0.50);
        let p95 = pct_ms(&sorted, 0.95);
        let p99 = pct_ms(&sorted, 0.99);
        let timeout_rate = timeouts as f64 / (lats.len() + timeouts as usize).max(1) as f64;
        let latency_ok = sorted.len() >= 10 && p95 < 8.0;
        let stall_ok = timeout_rate <= 0.05;
        let verdict = if latency_ok && stall_ok {
            "PASS"
        } else if !stall_ok {
            "FAIL (echo stall rate)"
        } else {
            "FAIL"
        };
        println!(
            "flood: 10 agents × 1000 lines/s each (self-generated deterministic flood) · \
             {eps:.0} batches/s applied · apply p95 {apply_us:.2} µs"
        );
        println!(
            "focused input: p50 {p50:.2} p95 {p95:.2} p99 {p99:.2} ms · timeouts {timeouts} ({:.1}%) · sessions {sessions} (target input p95 < 8 ms: {verdict})",
            timeout_rate * 100.0
        );
        println!(
            "flood RSS {:.1} MB, state {:.1} MB",
            tree_rss_kb() as f64 / 1024.0,
            state_bytes(&m.lock().unwrap()) as f64 / 1e6
        );
    } else {
        println!(
            "\n=== agent flood (§39) ===\nSKIPPED: fake-agent binary not built (`cargo build -p fake-agent` or FLASHTERMINAL_FAKE_AGENT_BIN)"
        );
    }
    println!("DONE");
}
