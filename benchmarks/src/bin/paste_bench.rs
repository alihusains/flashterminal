//! Phase 0.5.2 §13 — interactive paste benchmark.
//!
//! 100 `echo` commands written to a real zsh at 2 ms pacing (a realistic
//! paste), measuring first-marker-visible, last-marker-visible, total
//! completion, and per-item input latency. Distinct from §12: this is the
//! interactive ZLE path (shell echo, prompt redraw, command execution);
//! §12 measures raw PTY→parser→state throughput with no shell line editor.
//!
//! Usage: `cargo run --release -p benchmarks --bin paste_bench`

use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;

const CMDS: u32 = 100;
const PACE_MS: u64 = 2;

fn grid_has(state: &mut TerminalState, needle: &[u8]) -> bool {
    let total = state.grid_len();
    let window = total.min(4096);
    let start = total - window;
    for idx in (start..total).rev() {
        let row = state.grid_row(idx);
        let mut matched = 0usize;
        for cell in &row.cells {
            if cell.ch == 0 || cell.ch >= 128 {
                matched = 0;
                continue;
            }
            let b = cell.ch as u8;
            if b == needle[matched] {
                matched += 1;
                if matched == needle.len() {
                    return true;
                }
            } else if b == needle[0] {
                matched = 1;
            } else {
                matched = 0;
            }
        }
    }
    false
}

fn shell() -> &'static str {
    ["/bin/zsh", "/bin/bash", "/bin/sh"]
        .iter()
        .find(|s| std::path::Path::new(s).exists())
        .copied()
        .unwrap_or("/bin/sh")
}

fn main() {
    println!("=== Phase 0.5.2 §13 interactive paste: {CMDS} cmds @ {PACE_MS} ms ===");
    let pty = Arc::new(PtyManager::new().expect("pty"));
    let (session, _) = Session::spawn(Arc::clone(&pty), shell(), ".", 120, 300).unwrap();
    let mut state = TerminalState::new(120, 300);
    for _ in 0..40 {
        session.drain(&mut state);
        std::thread::sleep(Duration::from_millis(2));
    }
    session.resize(120, 300);
    state.resize(120, 300);

    let t0 = Instant::now();
    let mut first_us: Option<f64> = None;
    let mut last_us: Option<f64> = None;
    for i in 0..CMDS {
        let cmd = format!("echo _VALB{i}_DONE_\n");
        let t = Instant::now();
        session.write(cmd.as_bytes());
        // Busy-drain to catch the first command's echo appearing.
        loop {
            session.drain(&mut state);
            if grid_has(&mut state, b"_VALB0_DONE_") && first_us.is_none() {
                first_us = Some(t0.elapsed().as_secs_f64() * 1e6);
            }
            if grid_has(&mut state, &format!("_VALB{i}_DONE_").into_bytes()) && last_us.is_none() {
                last_us = Some(t.elapsed().as_secs_f64() * 1e6);
            }
            if first_us.is_some() {
                break;
            }
            if t.elapsed() > Duration::from_millis(500) {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(PACE_MS));
    }
    // Wait for the last marker (all commands drained).
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        session.drain(&mut state);
        if grid_has(&mut state, b"_VALB99_DONE_") {
            last_us = Some(Instant::now().duration_since(t0).as_secs_f64() * 1e6);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let total_us = t0.elapsed().as_secs_f64() * 1e6;
    let bytes = session
        .stats()
        .bytes_read
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "first marker visible: {:.2} ms | last marker visible: {:.2} ms | total: {:.2} ms | per-item: {:.2} ms | bytes read: {bytes}",
        first_us.unwrap_or(-1.0) / 1000.0,
        last_us.unwrap_or(-1.0) / 1000.0,
        total_us / 1000.0,
        total_us / 1000.0 / CMDS as f64
    );
    println!(
        "note: interactive ZLE path (echo + prompt + execution); raw pipeline numbers are in raw_throughput."
    );
    session.terminate();
    println!("DONE");
}
