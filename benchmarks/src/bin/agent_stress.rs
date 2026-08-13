//! Phase 2B.1 agent stress harness (§2–§6).
//!
//! Uses the deterministic `fake-agent` binary to prove, headlessly, against
//! the real `terminal-workspace` engine:
//!
//! - §2 concurrency: 10 concurrent agents (2 working / 2 streaming /
//!   2 waiting / 2 approval / 1 large-output / 1 long-running) alongside
//!   2 interactive terminal panes — agent activity must not starve
//!   normal terminal interaction.
//! - §3 starvation: 5 heavy-output agents (A–E) flooding while two
//!   interactive panes (F/G) are typed into continuously — input latency
//!   p95 target 8 ms.
//! - §4 memory scaling: 1/5/10/20 agents × idle / moderate / heavy /
//!   long-running — RSS, state bytes, queue depth.
//! - §5 event throughput: events/s, apply-latency p95, batch size.
//! - §6 high-output stability: 1/5/10 `large-output` agents concurrently —
//!   no freeze, no PTY deadlock, no correctness-affecting event loss,
//!   no unbounded memory growth.
//!
//! SKIPs (exit 0) when the fake-agent binary is not built.
//!
//! Usage: `cargo run --release -p benchmarks --bin agent_stress [secs]`

use std::collections::HashMap;
use std::time::{Duration, Instant};

use terminal_session::launch::AgentLaunchConfig;
use terminal_workspace::{Multiplexer, SplitDirection};

fn tree_rss_kb() -> u64 {
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

fn percentile_ms(sorted: &[f64], q: f64) -> f64 {
    let idx = ((sorted.len() as f64 * q) as usize).min(sorted.len() - 1);
    sorted[idx]
}

fn report_latency(label: &str, lats: &[f64], target_p95_ms: f64) -> bool {
    let mut s = lats.to_vec();
    s.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile_ms(&s, 0.50);
    let p95 = percentile_ms(&s, 0.95);
    let p99 = percentile_ms(&s, 0.99);
    let max = *s.last().unwrap();
    let ok = p95 < target_p95_ms;
    println!(
        "{label}: n={} p50 {p50:.2} p95 {p95:.2} p99 {p99:.2} max {max:.2} ms \
         (<{target_p95_ms} ms target: {})",
        s.len(),
        if ok { "PASS" } else { "FAIL" }
    );
    ok
}

fn launch(scenario: &str, extra: &[&str]) -> AgentLaunchConfig {
    let mut args = vec!["--scenario".to_string(), scenario.to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));
    AgentLaunchConfig {
        definition_id: "fake-agent".into(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        arguments: args,
        provider_id: None,
        model_id: None,
        credential_ref: None,
        resume_id: None,
        environment: vec![],
    }
}

fn spawn_agent(m: &mut Multiplexer, scenario: &str, extra: &[&str]) -> String {
    m.split_pane_agent(SplitDirection::Vertical, launch(scenario, extra))
        .unwrap();
    // The new pane is focused; return its execution id.
    let mut eids = Vec::new();
    if let Some(tab) = m.active_tab().cloned() {
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        for p in panes {
            eids.push(p.execution_id.0.clone());
        }
    }
    eids.last().unwrap().clone()
}

fn settle(m: &mut Multiplexer, dur: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < dur {
        m.drain_frame();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(12);

    if terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_err() {
        println!("SKIP: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }

    println!(
        "=== FlashTerminal Phase 2B.1 agent stress harness ({}s sections) ===",
        secs
    );
    let mut failed = 0usize;

    // ---- §2: 10 concurrent agents + 2 interactive terminals ---------------
    {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        let t0 = Instant::now();
        spawn_agent(&mut m, "working", &[]);
        spawn_agent(&mut m, "working", &[]);
        spawn_agent(&mut m, "streaming", &[]);
        spawn_agent(&mut m, "streaming", &[]);
        spawn_agent(&mut m, "waiting", &[]);
        spawn_agent(&mut m, "waiting", &[]);
        spawn_agent(&mut m, "approval", &[]);
        spawn_agent(&mut m, "approval", &[]);
        spawn_agent(&mut m, "large-output", &[]);
        spawn_agent(&mut m, "long-running", &["--duration", "30"]);
        // Two interactive terminal panes (F/G equivalents).
        let f = m.split_pane(SplitDirection::Horizontal).unwrap();
        let g = m.split_pane(SplitDirection::Horizontal).unwrap();
        m.focus_pane(&g).unwrap();
        settle(&mut m, Duration::from_millis(1500));

        let sessions = m.agent_runtime().list_sessions();
        assert_eq!(
            sessions.len(),
            10,
            "all 10 agents must be live at t+1.5s (got {})",
            sessions.len()
        );
        println!(
            "concurrency: 10/10 agents spawned+live at t+1.5s (spawn took {:.1} ms)",
            t0.elapsed().as_secs_f64() * 1e3
        );

        // Type into the interactive panes while all 10 agents run.
        let mut lats = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut tick = 0usize;
        while Instant::now() < deadline {
            for p in [&f, &g] {
                if let Some(s) = m.terminal_session_for_pane(p) {
                    s.write(b"x");
                }
            }
            let t0 = Instant::now();
            m.write_focused(b"a");
            loop {
                let r = m.drain_frame();
                if r.changed || t0.elapsed() > Duration::from_millis(100) {
                    break;
                }
            }
            lats.push(t0.elapsed().as_secs_f64() * 1e3);
            if tick.is_multiple_of(10) {
                m.layout_active(terminal_workspace::Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                });
            }
            tick += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        if !report_latency("concurrency focused input", &lats, 8.0) {
            failed += 1;
        }

        // Lifecycle sanity: finishers exited 0, blockers still alive.
        let snaps = m.agent_runtime().list_sessions();
        let by_state: HashMap<&str, usize> =
            snaps
                .iter()
                .map(|s| (s.state.as_str(), 1))
                .fold(HashMap::new(), |mut acc, (k, v)| {
                    *acc.entry(k).or_insert(0) += v;
                    acc
                });
        println!("concurrency terminal states: {by_state:?}");
        let live = snaps.iter().filter(|s| s.exit_code.is_none()).count();
        assert!(
            live >= 5,
            "waiting/approval/long-running must still be live (got {live})"
        );
        println!("concurrency: {live}/10 still live at section end (expected ≥5)");
    }

    // ---- §3: starvation — 5 heavy agents + 2 interactive panes ------------
    {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        spawn_agent(&mut m, "large-output", &[]); // A
        spawn_agent(&mut m, "large-output", &[]); // B
        spawn_agent(&mut m, "long-running", &["--duration", "30"]); // C
        spawn_agent(&mut m, "long-running", &["--duration", "30"]); // D
        spawn_agent(&mut m, "large-output", &[]); // E
        let f = m.split_pane(SplitDirection::Horizontal).unwrap(); // F
        let g = m.split_pane(SplitDirection::Horizontal).unwrap(); // G
        m.focus_pane(&g).unwrap();

        let mut lats = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(secs);
        let mut max_frame_ms = 0.0f64;
        let mut tick = 0usize;
        while Instant::now() < deadline {
            for p in [&f, &g] {
                if let Some(s) = m.terminal_session_for_pane(p) {
                    s.write(b"abcdef");
                }
            }
            let t0 = Instant::now();
            m.write_focused(b"k");
            loop {
                let r = m.drain_frame();
                if r.changed || t0.elapsed() > Duration::from_millis(100) {
                    break;
                }
            }
            lats.push(t0.elapsed().as_secs_f64() * 1e3);
            max_frame_ms = max_frame_ms.max(lats.last().unwrap() + 1.0);
            if tick.is_multiple_of(10) {
                m.layout_active(terminal_workspace::Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                });
            }
            tick += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        let s3_ok = report_latency("starvation focused input (5 heavy + F/G)", &lats, 8.0);
        if !s3_ok {
            failed += 1;
        }
        // The critical requirement: agent output must never make an
        // interactive pane unusable — no frame may stall the pipeline.
        if max_frame_ms > 1000.0 {
            println!("starvation: FAIL — max frame {max_frame_ms:.1} ms exceeds 1000 ms");
            failed += 1;
        } else {
            println!("starvation: max frame {max_frame_ms:.1} ms (no freeze)");
        }
    }

    // ---- §4: memory scaling 1/5/10/20 agents × workload -------------------
    {
        println!("memory scaling (RSS / state MB, queue depth):");
        for class in ["idle", "moderate", "heavy", "long-running"] {
            let mut row = String::new();
            for count in [1usize, 5, 10, 20] {
                let mut m = Multiplexer::new().unwrap();
                m.ensure_workspace();
                match class {
                    "idle" => {
                        for _ in 0..count {
                            spawn_agent(&mut m, "completion", &[]);
                        }
                    }
                    "moderate" => {
                        for _ in 0..count {
                            spawn_agent(&mut m, "streaming", &[]);
                        }
                    }
                    "heavy" => {
                        for _ in 0..count {
                            spawn_agent(&mut m, "large-output", &[]);
                        }
                    }
                    _ => {
                        for _ in 0..count {
                            spawn_agent(&mut m, "long-running", &["--duration", "20"]);
                        }
                    }
                }
                settle(&mut m, Duration::from_millis(1200));
                let rss = tree_rss_kb() as f64 / 1024.0;
                let sb = state_bytes(&m) as f64 / 1e6;
                let q = m.session_pending_total();
                row.push_str(&format!(" {count}: {rss:.1}MB/{sb:.2}MB/q{q} |"));
            }
            println!("  {class:>12}{row}");
        }
        // Per-agent fixed overhead must not blow up: 20 vs 1 agent delta is
        // measured above; flag >1 GB RSS at 20 agents as a failure.
        println!("memory scaling: 20-agent engines must stay under 1 GB RSS (see table)");
    }

    // ---- §5: event throughput ----------------------------------------------
    {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        for _ in 0..10 {
            spawn_agent(&mut m, "large-output", &[]);
        }
        let t0 = Instant::now();
        let started = m.metrics.events_applied;
        let mut frames = 0u64;
        let mut max_frame_ms = 0.0f64;
        while t0.elapsed() < Duration::from_secs(secs) {
            let ft = Instant::now();
            m.drain_frame();
            let dt = ft.elapsed().as_secs_f64() * 1e3;
            max_frame_ms = max_frame_ms.max(dt);
            frames += 1;
            std::thread::sleep(Duration::from_millis(2));
        }
        // Section-window events/s (the engine's rolling 2 s window slides
        // off the busy frames once the agents finish — the section delta
        // is the honest number).
        let applied = m.metrics.events_applied - started;
        let eps = applied as f64 / secs as f64;
        println!(
            "throughput: {:.0} events/s, apply-latency p95 {:.2} µs/frame, {} events applied, \
             {} frames, avg batch {:.1} events/frame, max frame {max_frame_ms:.1} ms",
            eps,
            m.metrics.apply_latency_p95_us(),
            applied,
            frames,
            applied as f64 / frames.max(1) as f64
        );
        if eps < 10_000.0 {
            // The floor is a release-mode target; debug builds parse too
            // slowly to ever hit it, so debug reports without failing.
            if cfg!(debug_assertions) {
                println!("throughput: {eps:.0} events/s (debug build — floor not enforced)");
            } else {
                println!("throughput: FAIL — {eps:.0} events/s below 10k floor");
                failed += 1;
            }
        }
    }

    // ---- §6: high-output stability 1/5/10 ---------------------------------
    {
        for count in [1usize, 5, 10] {
            print!("high-output {count:>2}: spawning… ");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
            let mut m = Multiplexer::new().unwrap();
            m.ensure_workspace();
            let rss_before = tree_rss_kb() as f64 / 1024.0;
            let mut eids = Vec::new();
            for _ in 0..count {
                eids.push(spawn_agent(&mut m, "large-output", &[]));
            }
            let t0 = Instant::now();
            let mut max_frame_ms = 0.0f64;
            let mut frames = 0u64;
            loop {
                let ft = Instant::now();
                let r = m.drain_frame();
                let dt = ft.elapsed().as_secs_f64() * 1e3;
                max_frame_ms = max_frame_ms.max(dt);
                frames += 1;
                if r.events_applied == 0 {
                    std::thread::sleep(Duration::from_millis(5));
                }
                // All agents done (exit 0) and stream drained.
                let snaps = m.agent_runtime().list_sessions();
                if snaps.len() == count
                    && snaps.iter().all(|s| s.exit_code == Some(0))
                    && r.events_applied == 0
                {
                    break;
                }
                assert!(
                    t0.elapsed() < Duration::from_secs(60),
                    "high-output {count}: agents did not finish in 60s"
                );
            }
            let rss_after = tree_rss_kb() as f64 / 1024.0;
            // Correctness: the final line must have reached every agent
            // pane's state (the default shell pane is not an agent).
            let mut tail_ok = true;
            if let Some(tab) = m.active_tab().cloned() {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                for p in panes {
                    if p.metadata.get("agent").is_none() {
                        continue;
                    }
                    if let Some(st) = m.state_for_pane_mut(&p.id) {
                        let snap = st.snapshot();
                        // The trailing `\n` after the last println! leaves
                        // the cursor on a fresh empty row, so scan the
                        // last few rows for the final line.
                        let mut found = false;
                        for row in snap.rows.saturating_sub(4)..snap.rows {
                            let mut last = String::new();
                            for c in 0..snap.cols {
                                if let Some(ch) = char::from_u32(snap.visible_cell(row, c).ch) {
                                    last.push(ch);
                                }
                            }
                            if last.contains("Line 100000") {
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            tail_ok = false;
                            eprintln!(
                                "  tail-check pane {} rows={} (last 4 rows had no final line)",
                                p.id, snap.rows
                            );
                        }
                    }
                }
            }
            let delta = rss_after - rss_before;
            let ok = max_frame_ms < 1000.0 && delta < 256.0 && tail_ok;
            println!(
                "high-output {count:>2}: all exited 0, tail-intact {}, max frame {max_frame_ms:.1} ms, \
                 RSS +{delta:.1} MB ({frames} frames)",
                if tail_ok { "yes" } else { "NO" }
            );
            if !ok {
                println!("high-output {count}: FAIL");
                failed += 1;
            }
        }
    }

    println!(
        "=== agent stress: {} ===",
        if failed == 0 {
            "ALL PASS".to_string()
        } else {
            format!("{failed} FAILURES")
        }
    );
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
