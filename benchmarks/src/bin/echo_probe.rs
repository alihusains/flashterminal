//! Isolated echo-latency probe (Phase 3F §12 investigation): one pane, no
//! flood. Measures write→echo latency to characterize the reader-thread
//! wake path without heavy-pane contention.
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use terminal_workspace::Multiplexer;

fn main() {
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("probe", "/tmp").unwrap();
    let pane = m.focused_pane().unwrap();

    // Warm up the shell + reader path.
    for _ in 0..20 {
        let before = {
            let s = m.terminal_session_for_pane(&pane).unwrap();
            s.write(b"a");
            s.stats().applied_batches.load(Ordering::Relaxed)
        };
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(5) {
            m.drain_frame();
            let done = {
                let s = m.terminal_session_for_pane(&pane).unwrap();
                s.stats().applied_batches.load(Ordering::Relaxed) > before
            };
            if done {
                break;
            }
        }
    }

    let mut lats = Vec::new();
    let mut timeouts = 0u64;
    let n = 2000u32;
    for _ in 0..n {
        let before = {
            let s = m.terminal_session_for_pane(&pane).unwrap();
            s.write(b"a");
            s.stats().applied_batches.load(Ordering::Relaxed)
        };
        let t0 = Instant::now();
        let mut drains = 0u64;
        let echoed = loop {
            m.drain_frame();
            drains += 1;
            let now = {
                let s = m.terminal_session_for_pane(&pane).unwrap();
                s.stats().applied_batches.load(Ordering::Relaxed)
            };
            if now > before || t0.elapsed() > Duration::from_millis(100) || drains > 10_000 {
                break now > before;
            }
            std::thread::sleep(Duration::from_micros(200));
        };
        if echoed {
            lats.push(t0.elapsed().as_secs_f64() * 1e3);
        } else {
            timeouts += 1;
        }
    }
    lats.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| {
        if lats.is_empty() {
            return f64::NAN;
        }
        let idx = ((lats.len() as f64 * q) as usize).min(lats.len() - 1);
        lats[idx]
    };
    println!(
        "single-pane echo: n={} p50 {:.3} p95 {:.3} p99 {:.3} max {:.3} ms, timeouts {} ({:.1}%)",
        lats.len(),
        p(0.50),
        p(0.95),
        p(0.99),
        lats.last().copied().unwrap_or(f64::NAN),
        timeouts,
        timeouts as f64 / n as f64 * 100.0
    );
}
