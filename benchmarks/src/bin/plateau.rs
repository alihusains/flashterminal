//! Phase 0.5.2 §7 — memory plateau test.
//!
//! In a fresh process, spawns 1 / 10 / 20 `yes`-flooded panes, drains them
//! continuously, and samples process-tree RSS + per-pane state memory over
//! time. With the tiered scrollback the state memory must **plateau** as
//! history grows (cold storage is compressed and capped), and the process
//! RSS should approach a steady level rather than growing linearly with the
//! volume of output.
//!
//! Usage: `cargo run --release -p benchmarks --bin plateau [seconds]`

use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;

fn tree_rss_mb(pid: i64) -> f64 {
    let mut children: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    let mut rss: std::collections::HashMap<i64, f64> = std::collections::HashMap::new();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .output()
    else {
        return 0.0;
    };
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = l.split_whitespace();
        let (Some(p), Some(pp), Some(r)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(p), Ok(pp), Ok(r)) = (p.parse::<i64>(), pp.parse::<i64>(), r.parse::<f64>()) else {
            continue;
        };
        children.entry(pp).or_default().push(p);
        rss.insert(p, r);
    }
    fn sum(
        pid: i64,
        children: &std::collections::HashMap<i64, Vec<i64>>,
        rss: &std::collections::HashMap<i64, f64>,
        seen: &mut std::collections::HashSet<i64>,
    ) -> f64 {
        if !seen.insert(pid) {
            return 0.0;
        }
        let mut total = rss.get(&pid).copied().unwrap_or(0.0);
        for c in children.get(&pid).into_iter().flatten() {
            total += sum(*c, children, rss, seen);
        }
        total
    }
    sum(pid, &children, &rss, &mut Default::default()) / 1024.0
}

fn shell() -> &'static str {
    ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
        .copied()
        .unwrap_or("/bin/sh")
}

fn main() {
    let secs = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(60);
    let my_pid = std::process::id() as i64;
    println!("=== Phase 0.5.2 §7 plateau: fresh process, {secs}s, yes-flooded panes ===");
    println!(
        "baseline tree RSS (no panes): {:.1} MB",
        tree_rss_mb(my_pid)
    );

    for panes in [1usize, 10, 20] {
        let pty = Arc::new(PtyManager::new().expect("pty"));
        let mut sessions: Vec<Session> = Vec::new();
        let mut states: Vec<TerminalState> = Vec::new();
        for _ in 0..panes {
            let (s, _) = Session::spawn(Arc::clone(&pty), shell(), ".", 120, 40).unwrap();
            s.write(b"yes 0123456789abcdefghijklmnopqrstuvwxyz\n");
            sessions.push(s);
            states.push(TerminalState::new(120, 40));
        }
        // Warm up: let output flow for 3 s so scrollback reaches the cap.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            for (s, st) in sessions.iter_mut().zip(states.iter_mut()) {
                s.drain(st);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // Sample the plateau over the remaining window. The drain runs at a
        // desktop-like cadence (2 ms) so the channel never backs up; RSS is
        // sampled once per second without starving it.
        let start = Instant::now();
        let mut next_sample = 0u64;
        loop {
            for (s, st) in sessions.iter_mut().zip(states.iter_mut()) {
                s.drain(st);
            }
            std::thread::sleep(Duration::from_millis(2));
            let t = start.elapsed().as_secs();
            if t >= next_sample {
                next_sample = t + 1;
                let state_mb: f64 = states
                    .iter()
                    .map(|st| st.retained_memory() as f64 / 1e6)
                    .sum();
                let state_rows: usize = states.iter().map(|st| st.grid_len()).sum();
                let cold_blocks: usize = states.iter().map(|st| st.cold.len()).sum();
                println!(
                    "  {panes:>2} panes t={t:>3}s | tree RSS {:.1} MB | state {:.2} MB | rows {:.0}K | cold blocks {cold_blocks}",
                    tree_rss_mb(my_pid),
                    state_mb,
                    state_rows as f64 / 1000.0
                );
            }
            if t >= secs {
                break;
            }
        }
        for s in &sessions {
            s.terminate();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    println!("DONE");
}
