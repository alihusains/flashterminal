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
//! Usage: `cargo run --release -p benchmarks --bin multiplex_bench [secs]`

use std::time::{Duration, Instant};

use terminal_workspace::{Multiplexer, Rect, SplitDirection};

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

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let outer = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };
    println!("=== FlashTerminal Phase 1 multiplexer benchmark ===");

    // ---- workspace creation -------------------------------------------------
    {
        let mut m = Multiplexer::new().unwrap();
        let n = 100;
        let t0 = Instant::now();
        for i in 0..n {
            m.create_workspace(&format!("ws{i}"), "/tmp").unwrap();
        }
        let el = t0.elapsed() / n;
        println!(
            "workspace create: {n}x total {:?}, avg {:?} (<100 ms target: {})",
            t0.elapsed(),
            el,
            if el < Duration::from_millis(100) {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!("  (spawns a real shell each — dominated by process spawn)");
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
        m.create_workspace("scale", "/tmp").unwrap();
        for _ in 1..panes {
            m.split_pane(SplitDirection::Horizontal).unwrap();
        }
        // settle + drain
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(800) {
            m.drain_frame();
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
    {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("stress", "/tmp").unwrap();
        for _ in 1..20 {
            m.split_pane(SplitDirection::Vertical).unwrap();
        }
        // Owned (pane_id, session_id) pairs so they outlive the borrow.
        let mut panes: Vec<(String, String)> = Vec::new();
        if let Some(tab) = m.active_tab().cloned() {
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
        let focused = m.focused_pane().unwrap();
        let start = Instant::now();

        // Flood the heavy panes by running `yes` inside them — the shell
        // blocks in the kernel PTY when full (natural backpressure), exactly
        // like the Phase 0.5.1 stress harness.
        for (i, p) in panes.iter().enumerate() {
            if workloads[i] == 2 {
                if let Some(s) = m.terminal_session_for_pane(&p.0) {
                    s.write(b"yes 0123456789abcdefghijklmnopqrstuvwxyz\n");
                }
            }
        }

        // input latency to the FOCUSED pane while heavy panes are busy
        let mut lats = Vec::new();
        let mut t = 0usize;
        let deadline = start + Duration::from_secs(secs);
        while Instant::now() < deadline {
            // interactive panes trickle
            for (i, p) in panes.iter().enumerate() {
                if workloads[i] == 3 {
                    if let Some(s) = m.terminal_session_for_pane(&p.0) {
                        s.write(b"x");
                    }
                }
            }
            // focused input: write + measure until the event is applied
            let t0 = Instant::now();
            if let Some(s) = m.terminal_session_for_pane(&focused) {
                s.write(b"a");
            }
            loop {
                let r = m.drain_frame();
                if r.changed || t0.elapsed() > Duration::from_millis(100) {
                    break;
                }
            }
            lats.push(t0.elapsed());
            if t.is_multiple_of(10) {
                m.layout_active(outer);
            }
            t += 1;
            std::thread::sleep(Duration::from_millis(2));
        }

        let mut sorted = lats.clone();
        sorted.sort_unstable();
        let p = |q: f64| {
            let idx = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
            sorted[idx].as_secs_f64() * 1e3
        };
        let eps = m.metrics.events_per_second();
        println!(
            "stress 20 panes (5/5/5/5): focused input p50 {:.2} p95 {:.2} p99 {:.2} max {:.2} ms \
             (<8 ms target: {})",
            p(0.50),
            p(0.95),
            p(0.99),
            sorted.last().unwrap().as_secs_f64() * 1e3,
            if p(0.95) < 8.0 { "PASS" } else { "FAIL" }
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
    println!("DONE");
}
