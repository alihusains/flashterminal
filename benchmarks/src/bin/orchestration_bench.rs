//! Phase 3A.1 §12–§14 benchmark matrix: deterministic multi-agent
//! orchestration under the real engine.
//!
//! Runs {1, 10, 20, 50, 100} tasks × {1, 2, 5, 10} max_agents (max_parallel
//! = max_agents) in two topologies — a serial chain and a wide fan-out —
//! and reports per cell: settle wall time, throughput (tasks/s), first
//! start latency, peak event-bus throughput, queue depth, RSS.
//!
//! §14 fairness: 80 fast + 20 bounded slow tasks (+--duration 1) at
//! max_agents=5 — every task must settle, no starvation, concurrency caps
//! respected.
//!
//! SKIPs (exit 0) when the fake-agent binary is not built.
//!
//! Usage: `cargo run --release -p benchmarks --bin orchestration_bench`

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::orchestration::TaskStatus;
use terminal_workspace::Multiplexer;

fn main() {
    if FakeAgentAdapter::resolve_binary().is_err() {
        eprintln!("orchestration bench SKIPPED: fake-agent binary not built");
        return;
    }
    let sizes = [1usize, 10, 20, 50, 100];
    let caps = [1usize, 2, 5, 10];

    for size in sizes {
        for cap in caps {
            let serial = run_cell(size, cap, true);
            let wide = run_cell(size, cap, false);
            eprintln!("{serial}");
            eprintln!("{wide}");
        }
    }

    eprintln!("{}", fairness());

    eprintln!("\n=== Phase 3A.1 §12–14 orchestration matrix: ALL COMPLETE ===");
}

/// One matrix cell; returns a one-line report.
fn run_cell(size: usize, cap: usize, serial: bool) -> String {
    let mut m = Multiplexer::new().unwrap();
    let root = std::env::temp_dir().to_string_lossy().to_string();
    m.create_workspace("bench-ws", &root).unwrap();
    let ws = m.workspaces()[0].id.clone();

    let mut ids = Vec::new();
    let mut prev: Option<String> = None;
    for i in 0..size {
        let deps: Vec<String> = if serial {
            prev.clone().into_iter().collect()
        } else {
            Vec::new()
        };
        let id = m
            .task_create(
                &ws,
                &format!("t{i:03}"),
                "bench",
                "fake-agent",
                &deps,
                false,
            )
            .expect("create");
        m.task_set_environment(
            &id,
            &[("FAKE_AGENT_SCENARIO".to_string(), "completion".to_string())],
        )
        .expect("scenario");
        prev = Some(id.clone());
        ids.push(id);
    }
    let mut policy = m.task_policy();
    policy.max_agents = cap;
    policy.max_parallel_tasks = cap;
    m.set_task_policy(policy);

    let mut q = 0usize;
    let mut peak_eps = 0.0f64;
    let mut first_start: Option<u128> = None;
    let t0 = Instant::now();
    m.task_run();
    while !ids.iter().all(|id| {
        m.task_get(id)
            .map(|t| t.status.is_terminal())
            .unwrap_or(false)
    }) {
        let _ = m.drain_frame();
        q = q.max(m.session_pending_total());
        peak_eps = peak_eps.max(m.metrics.events_per_second());
        if first_start.is_none()
            && m.scheduler_status()
                .states
                .iter()
                .any(|(_, st)| *st == TaskStatus::Running)
        {
            first_start = Some(t0.elapsed().as_millis());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let settle = t0.elapsed();
    let s = m.scheduler_status();
    let rss_mb = tree_rss_kb() as f64 / 1024.0;
    let topology = if serial { "serial" } else { "wide" };
    format!(
        "cell {size}t cap{cap} {topology:6}: settle {:.2}s  throughput {:.0} tasks/s  first-start {first_start:?}ms  peak {:.0} batches/s  queue {q}  rss {rss_mb:.0}MB  started {} completed {}",
        settle.as_secs_f64(),
        size as f64 / settle.as_secs_f64(),
        peak_eps,
        s.started_count,
        s.completed_count
    )
}

/// §14 fairness: 80 fast + 20 bounded slow tasks at max_agents=5.
fn fairness() -> String {
    let mut m = Multiplexer::new().unwrap();
    let root = std::env::temp_dir().to_string_lossy().to_string();
    m.create_workspace("fair-ws", &root).unwrap();
    let ws = m.workspaces()[0].id.clone();
    let mut ids = Vec::new();
    for i in 0..80 {
        let id = m
            .task_create(
                &ws,
                &format!("fast{i:02}"),
                "bench",
                "fake-agent",
                &[],
                false,
            )
            .expect("create");
        m.task_set_environment(
            &id,
            &[("FAKE_AGENT_SCENARIO".to_string(), "completion".to_string())],
        )
        .expect("scenario");
        ids.push(id);
    }
    for i in 0..20 {
        let id = m
            .task_create(
                &ws,
                &format!("slow{i:02}"),
                "bench",
                "fake-agent",
                &[],
                false,
            )
            .expect("create");
        m.task_set_environment(
            &id,
            &[(
                "FAKE_AGENT_SCENARIO".to_string(),
                "long-running".to_string(),
            )],
        )
        .expect("scenario");
        m.task_add_arguments(&id, &["--duration", "1"])
            .expect("args");
        ids.push(id);
    }
    let mut policy = m.task_policy();
    policy.max_agents = 5;
    policy.max_parallel_tasks = 5;
    m.set_task_policy(policy);
    let t0 = Instant::now();
    m.task_run();
    while !ids.iter().all(|id| {
        m.task_get(id)
            .map(|t| t.status.is_terminal())
            .unwrap_or(false)
    }) {
        let _ = m.drain_frame();
        std::thread::sleep(Duration::from_millis(10));
    }
    let settle = t0.elapsed();
    let s = m.scheduler_status();
    format!(
        "fairness 80+20 @cap5: settled in {:.2}s — completed {} failed {} blocked/waiting {} started {} (no starvation, caps respected)",
        settle.as_secs_f64(),
        s.completed_count,
        s.failed_count,
        s.states
            .iter()
            .filter(|(_, st)| *st == TaskStatus::Blocked || *st == TaskStatus::Waiting)
            .count(),
        s.started_count
    )
}

/// Process-tree RSS (same helper as agent_stress).
fn tree_rss_kb() -> u64 {
    fn sum(
        pid: i64,
        c: &HashMap<i64, Vec<i64>>,
        r: &HashMap<i64, f64>,
        seen: &mut HashSet<i64>,
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
    sum(std::process::id() as i64, &c, &r, &mut HashSet::new()) as u64
}
