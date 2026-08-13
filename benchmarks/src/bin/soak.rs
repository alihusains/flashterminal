//! Phase 0.5.2 §17 soak test.
//!
//! Runs `duration` seconds with 10 panes: 5 continuous output streams
//! (`cat` fed from a producer thread) and 5 interactive `cat` passthrough
//! panes with a periodic trickle. Every `interval` seconds it samples
//! process-tree RSS, CPU, open FDs, thread count, and per-pane event queue
//! depth, appending a CSV line to `out`.
//!
//! Leak detection: compares the mean tree RSS of the first quarter of
//! samples against the last quarter. Growth beyond `max_growth_mb` over the
//! window reports a leak.
//!
//! Usage: `cargo run --release -p benchmarks --bin soak [secs] [out.csv]`
//! (defaults: 3600 s, /tmp/soak.csv)

use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;
/// Sum of RSS (KB) of this process and its whole child tree.
/// Samples `ps` twice and returns the larger sum to dodge ps snapshot races.
fn tree_rss_kb(pid: i64) -> u64 {
    fn sample(pid: i64) -> u64 {
        let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut rss: HashMap<i64, f64> = HashMap::new();
        let Ok(out) = Command::new("ps")
            .args(["-axo", "pid=,ppid=,rss="])
            .output()
        else {
            return 0;
        };
        for l in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = l.split_whitespace();
            let (Some(p), Some(pp), Some(r)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Ok(p), Ok(pp), Ok(r)) = (p.parse::<i64>(), pp.parse::<i64>(), r.parse::<f64>())
            else {
                continue;
            };
            children.entry(pp).or_default().push(p);
            rss.insert(p, r);
        }
        fn sum(
            pid: i64,
            children: &HashMap<i64, Vec<i64>>,
            rss: &HashMap<i64, f64>,
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
        sum(pid, &children, &rss, &mut Default::default()) as u64
    }
    let a = sample(pid);
    let b = sample(pid);
    a.max(b)
}

/// Open file descriptors of this process (all threads share them).
fn fd_count() -> usize {
    std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0)
}

/// Approximate thread count from `ps -M` line count.
fn thread_count() -> usize {
    let out = Command::new("ps")
        .args(["-M", "-o", "pid="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    out.lines().count()
}

struct Pane {
    session: Arc<Session>,
    state: TerminalState,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let duration = args
        .get(1)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(3600);
    let out_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| "/tmp/soak.csv".to_string());
    let my_pid = std::process::id() as i64;

    println!(
        "soak: {}s, 10 panes (5 stream, 5 interactive), out={}",
        duration, out_path
    );

    let pty = Arc::new(PtyManager::new().unwrap());
    let mut panes = Vec::new();
    for _ in 0..10 {
        let (s, _) = Session::spawn(Arc::clone(&pty), "/bin/cat", ".", 120, 40).unwrap();
        let s = Arc::new(s);
        let mut st = TerminalState::new(120, 40);
        for _ in 0..30 {
            s.drain(&mut st);
            std::thread::sleep(Duration::from_millis(2));
        }
        panes.push(Pane {
            session: s,
            state: st,
        });
    }

    // Continuous output producers for panes 0..5 (Session::write is
    // thread-safe — it forwards to the PTY master behind a mutex).
    let mut producers = Vec::new();
    for (i, p) in panes.iter().take(5).enumerate() {
        let s = Arc::clone(&p.session);
        producers.push(std::thread::spawn(move || {
            // ~96 MB per producer total (60 k × 1.6 KB), paced so the aggregate
            // stream rate is on the order of the pty drain rate (~17 MB/s) — a
            // realistic continuous stream, not a multi-GB backlog spike.
            let line = format!("[stream-{}] {}0123456789abcdef\n", i, i);
            let bytes = line.repeat(40);
            let mut buf = Vec::new();
            for _ in 0..60_000 {
                buf.extend_from_slice(bytes.as_bytes());
                if buf.len() >= 4 * 1024 * 1024 {
                    s.write(&buf);
                    buf.clear();
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            s.write(&buf);
        }));
    }

    // Interactive panes 5..10 get a periodic trickle to prove liveness.
    let mut trickles = Vec::new();
    for p in panes.iter().skip(5) {
        let s = Arc::clone(&p.session);
        trickles.push(std::thread::spawn(move || {
            for _ in 0..(duration.max(1) * 2) {
                std::thread::sleep(Duration::from_millis(500));
                s.write(b"x");
            }
        }));
    }

    let mut log = std::fs::File::create(&out_path).unwrap();
    let _ = writeln!(
        log,
        "t_sec, tree_rss_kb, fd_count, threads, chan_depths, state_mb, drained_mb"
    );
    let t0 = Instant::now();
    let interval = Duration::from_secs(5);
    let mut samples: Vec<(u64, u64)> = Vec::new();

    while t0.elapsed() < Duration::from_secs(duration) {
        let mut depths = Vec::new();
        let mut state_mb = 0.0f64;
        let mut drained_bytes = 0u64;
        for p in panes.iter_mut() {
            let before = p
                .session
                .stats()
                .bytes_read
                .load(std::sync::atomic::Ordering::Relaxed);
            p.session.drain(&mut p.state);
            let after = p
                .session
                .stats()
                .bytes_read
                .load(std::sync::atomic::Ordering::Relaxed);
            drained_bytes += after.saturating_sub(before);
            depths.push(p.session.pending_len());
            state_mb += p.state.retained_memory() as f64 / (1024.0 * 1024.0);
        }
        let t = t0.elapsed().as_secs();
        let rss = tree_rss_kb(my_pid);
        let line = format!(
            "{}, {}, {}, {}, {:?}, {:.2}, {:.1}\n",
            t,
            rss,
            fd_count(),
            thread_count(),
            depths,
            state_mb,
            drained_bytes as f64 / (1024.0 * 1024.0)
        );
        print!("{}", line);
        let _ = log.write_all(line.as_bytes());
        let _ = log.flush();
        samples.push((t, rss));
        std::thread::sleep(interval);
    }

    // Leak detection: mean of first quarter vs last quarter of samples.
    if samples.len() >= 8 {
        let q = samples.len() / 4;
        let first: f64 = samples[..q].iter().map(|s| s.1 as f64).sum::<f64>() / q as f64;
        let last: f64 = samples[samples.len() - q..]
            .iter()
            .map(|s| s.1 as f64)
            .sum::<f64>()
            / q as f64;
        let growth_mb = (last - first) / 1024.0;
        println!(
            "soak: first_q_mean={:.0}KB last_q_mean={:.0}KB growth={:.1}MB",
            first, last, growth_mb
        );
        println!(
            "soak: VERDICT {}",
            if growth_mb > 50.0 {
                "LEAK (tree RSS grew >50MB)"
            } else {
                "OK (plateaued)"
            }
        );
    }
    println!("soak: DONE");

    for h in producers.into_iter().chain(trickles) {
        let _ = h.join();
    }
}
