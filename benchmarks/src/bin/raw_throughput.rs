//! Phase 0.5.2 §12 — raw terminal throughput (cat passthrough).
//!
//! Spawns `/bin/cat` as the PTY process (non-interactive, no line editor),
//! feeds 10 MB / 100 MB of terminal-shaped output in 4 KB chunks, and
//! measures the full pipeline: PTY write → kernel → reader thread → VT
//! parser → bounded channel → state application → (simulated) render.
//!
//! Usage: `cargo run --release -p benchmarks --bin raw_throughput [mb]`

use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;

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

fn main() {
    let mb_arg = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10);
    let sizes = [mb_arg, mb_arg * 10];
    println!("=== Phase 0.5.2 §12 raw throughput (cat passthrough) ===");

    for mb in sizes {
        let pty = Arc::new(PtyManager::new().expect("pty"));
        let (session, _) = Session::spawn(Arc::clone(&pty), "/bin/cat", ".", 120, 40).unwrap();
        let mut state = TerminalState::new(120, 40);
        for _ in 0..40 {
            session.drain(&mut state);
            std::thread::sleep(Duration::from_millis(2));
        }
        session.write(b"\x1b[2J\x1b[H");

        // Build the payload: ANSI noise + text lines + a trailing marker.
        let target = mb * 1024 * 1024;
        let mut payload = Vec::with_capacity(target + 64);
        let noise = [
            b"\x1b[38;5;196m".as_slice(),
            b"\x1b[0m",
            b"\x1b[K",
            b"\x1b[2K\r",
            b"\x1b[?25l",
            b"\x1b[?25h",
            b"\x1b]0;probe\x07",
        ];
        let line = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\r\n";
        while payload.len() < target {
            for n in noise {
                payload.extend_from_slice(n);
            }
            payload.extend_from_slice(line);
        }
        payload.extend_from_slice(b"RAW_END_98765\r\n");

        // Feed at full speed in 64 KB chunks (non-blocking writer since
        // 0.5.1). No pacing sleeps while feeding — only a short one at the
        // end while waiting for the trailing marker to drain.
        let t0 = Instant::now();
        let mut written = 0usize;
        let mut renders = 0u64;
        let mut prev_batches = 0u64;
        let mut peak_queue = 0usize;
        let mut next_progress = 20 * 1024 * 1024;
        let deadline = Instant::now() + Duration::from_secs(600);
        let mut done = false;
        let mut last_bytes = 0u64;
        let mut last_t = Instant::now();
        while Instant::now() < deadline {
            if written < payload.len() {
                let end = (written + 65536).min(payload.len());
                session.write(&payload[written..end]);
                written = end;
            }
            let changed = session.drain(&mut state);
            let b = session
                .stats()
                .batches
                .load(std::sync::atomic::Ordering::Relaxed);
            if b != prev_batches {
                prev_batches = b;
            }
            if changed {
                render_visible(&mut state);
                renders += 1;
            }
            peak_queue = peak_queue.max(session.pending_len());
            let bytes = session
                .stats()
                .bytes_read
                .load(std::sync::atomic::Ordering::Relaxed);
            if bytes >= next_progress {
                let dt = last_t.elapsed().as_secs_f64();
                println!(
                    "    progress {:.0} MB read: {:.1} s (+{:.1} s for {:.0} MB -> {:.1} MB/s)",
                    bytes as f64 / 1e6,
                    t0.elapsed().as_secs_f64(),
                    dt,
                    (bytes - last_bytes) as f64 / 1e6,
                    (bytes - last_bytes) as f64 / 1e6 / dt.max(1e-9)
                );
                next_progress += 20 * 1024 * 1024;
                last_bytes = bytes;
                last_t = Instant::now();
            }
            if written == payload.len() && grid_has(&mut state, b"RAW_END_98765") {
                done = true;
                break;
            }
            if written == payload.len() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let elapsed = t0.elapsed().as_secs_f64();
        let events = session
            .stats()
            .events_read
            .load(std::sync::atomic::Ordering::Relaxed);
        let batches = session
            .stats()
            .batches
            .load(std::sync::atomic::Ordering::Relaxed);
        let bytes = session
            .stats()
            .bytes_read
            .load(std::sync::atomic::Ordering::Relaxed);
        let mb_s = bytes as f64 / (1024.0 * 1024.0) / elapsed.max(1e-9);
        println!(
            "{} MB: done={done} in {elapsed:.2}s | {mb_s:.1} MB/s | {} B read | {} events | {} batches | {} renders | peak queue {} | state {:.2} MB",
            mb,
            bytes,
            events,
            batches,
            renders,
            peak_queue,
            state.retained_memory() as f64 / 1e6
        );
        session.terminate();
    }
    println!("DONE");
}

fn render_visible(state: &mut TerminalState) {
    let snap = state.snapshot();
    let mut acc = 0u64;
    for r in 0..snap.rows {
        for c in 0..snap.cols {
            acc ^= snap.visible_cell(r, c).ch as u64;
        }
    }
    std::hint::black_box(acc);
}
