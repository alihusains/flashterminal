//! Performance-benchmark-audit §4/§5: stage-split latency on the REAL PTY
//! path (write -> PTY read -> parse+apply-visible), to determine whether
//! variance in `input_latency_apply_p95_ms` (a synthetic, no-PTY,
//! in-memory metric — see docs/performance-benchmarking.md) is explained
//! by PTY/OS scheduling, or lives elsewhere entirely.
//!
//! One real shell PTY session, single keystroke round-trips, monotonic
//! clock timestamps at each stage boundary. No logging or allocation in
//! the timed region beyond a single Vec push per sample.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;

fn main() {
    let pty = Arc::new(PtyManager::new().unwrap());
    let shell = ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
        .copied()
        .unwrap_or("/bin/sh");

    // Tap fires on the reader thread the instant a raw chunk is read from
    // the PTY master, before parsing — this is the true "PTY read"
    // timestamp, upstream of any parse/apply cost.
    let read_marks: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let read_marks_tap = Arc::clone(&read_marks);
    let tap: terminal_session::OutputTap = Box::new(move |_chunk: &[u8]| {
        read_marks_tap.lock().unwrap().push(Instant::now());
    });

    let (session, _pid) = Session::spawn_with_options(
        Arc::clone(&pty),
        shell,
        &[],
        &std::env::temp_dir().to_string_lossy(),
        &[],
        120,
        40,
        None,
        Some(tap),
    )
    .unwrap();
    let mut state = TerminalState::new(120, 40);

    // Warmup: let the shell settle (prompt, rc files) before measuring.
    for _ in 0..30 {
        session.drain(&mut state);
        std::thread::sleep(Duration::from_millis(5));
    }
    read_marks.lock().unwrap().clear();

    let n = 500u32;
    let mut write_to_apply_ms = Vec::with_capacity(n as usize);
    let mut write_to_read_ms = Vec::with_capacity(n as usize);
    let mut read_to_apply_ms = Vec::with_capacity(n as usize);
    let mut timeouts = 0u32;

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
        match (t_apply, t_read) {
            (Some(t_apply), Some(t_read)) => {
                write_to_apply_ms.push(t_apply.duration_since(t_write).as_secs_f64() * 1e3);
                write_to_read_ms.push(t_read.duration_since(t_write).as_secs_f64() * 1e3);
                read_to_apply_ms.push(t_apply.duration_since(t_read).as_secs_f64() * 1e3);
            }
            _ => timeouts += 1,
        }
        // Consume the echoed byte's newline-free prompt noise before the
        // next sample (backspace is not needed — 'a' just appends).
        std::thread::sleep(Duration::from_micros(500));
    }
    session.terminate();

    fn stats(mut v: Vec<f64>, label: &str) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        if n == 0 {
            println!("{label}: no samples");
            return;
        }
        let p = |q: f64| v[((n as f64 - 1.0) * q).round() as usize];
        let mean = v.iter().sum::<f64>() / n as f64;
        let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n as f64;
        println!(
            "{label:<24} n={n:>4} p50={:>7.3} p95={:>7.3} p99={:>7.3} max={:>7.3} mean={:>7.3} stddev={:>7.3} ms",
            p(0.50), p(0.95), p(0.99), v[n - 1], mean, var.sqrt()
        );
    }
    println!("=== staged_latency_probe: {n} samples, {timeouts} timeouts ===");
    stats(write_to_read_ms, "write_to_pty_read");
    stats(read_to_apply_ms, "read_to_apply_visible");
    stats(write_to_apply_ms, "write_to_apply (total)");
}
