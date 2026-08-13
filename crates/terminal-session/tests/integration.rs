//! Integration tests: real PTY → parser → TerminalState via `Session`.
//!
//! These spawn a real shell in a PTY on the test machine (macOS/Linux),
//! exercise the full ingestion path, and verify the Phase 0.5 guarantees:
//!
//! * PTY ingestion is not stalled by a slow consumer (backpressure test).
//! * Massive output eventually lands in the state without OOM.
//! * Resize propagates to the kernel/child.
//! * The alternate-screen smoke path does not corrupt state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_core::TerminalState;
use terminal_session::Session;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Picks a shell available on the machine; returns None if none exists.
fn shell() -> Option<&'static str> {
    ["/bin/sh", "/bin/bash", "/bin/zsh"]
        .iter()
        .find(|sh| std::path::Path::new(sh).exists())
        .copied()
}

/// Renders the visible grid as text (for assertions).
fn grid_text(state: &TerminalState) -> String {
    let mut out = String::new();
    for r in 0..state.rows {
        for c in 0..state.cols {
            let cell = state.visible_cell(r, c);
            if let Some(ch) = char::from_u32(cell.ch) {
                out.push(ch);
            } else {
                out.push(' ');
            }
        }
        out.push('\n');
    }
    out
}

/// Drains until `pred` holds or the deadline passes; returns whether it held.
fn drain_until<F: Fn(&TerminalState) -> bool>(
    session: &Session,
    state: &mut TerminalState,
    deadline: Instant,
    pred: F,
) -> bool {
    while Instant::now() < deadline {
        session.drain(state);
        if pred(state) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn spawn_session() -> Option<(Session, TerminalState)> {
    let sh = shell()?;
    let pty = Arc::new(PtyManager::new().ok()?);
    let (session, _pid) = Session::spawn(pty, sh, ".", COLS, ROWS).ok()?;
    let state = TerminalState::new(COLS, ROWS);
    Some((session, state))
}

#[test]
fn pty_to_state_echo() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    session.write(b"echo INTEGRATION_MARKER_42\n");
    let ok = drain_until(
        &session,
        &mut state,
        Instant::now() + Duration::from_secs(15),
        |s| grid_text(s).contains("INTEGRATION_MARKER_42"),
    );
    assert!(
        ok,
        "echo output never reached the state:\n{}",
        grid_text(&state)
    );
    assert!(!session.has_exited(), "shell exited unexpectedly");
    session.terminate();
}

#[test]
fn massive_output_lands_in_state() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    // 200K lines (~1.2 MB) through the PTY. The marker is composed so the
    // shell's command echo cannot match it — only the final echo can.
    session.write(b"seq 1 200000; echo _MAS$(echo SIVE)_DONE_\n");
    let ok = drain_until(
        &session,
        &mut state,
        Instant::now() + Duration::from_secs(60),
        |s| grid_text(s).contains("_MASSIVE_DONE_"),
    );
    assert!(
        ok,
        "massive output was not ingested; grid rows = {}",
        state.grid.len()
    );
    assert!(
        state.scrollback_len() > 0,
        "expected scrollback to accumulate"
    );
    session.terminate();
}

/// The mandatory Phase 0.5 backpressure test: the child produces output much
/// faster than the (deliberately slow) consumer drains it. Requirements:
/// the reader thread keeps running, memory stays bounded, and the state
/// eventually catches up — ingestion must not stall permanently.
#[test]
fn pty_backpressure_no_stall() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    // 500K lines (~3 MB) — larger than the channel can hold. Marker is
    // composed so the command echo cannot match it.
    session.write(b"seq 1 500000; echo _B$(echo P)_DONE_\n");

    // Deliberately slow consumer: 2 ms between drains (simulating a busy UI).
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut saw_done = false;
    while Instant::now() < deadline {
        session.drain(&mut state);
        if grid_text(&state).contains("_BP_DONE_") {
            saw_done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        saw_done,
        "PTY ingestion stalled under backpressure; grid rows = {}",
        state.grid.len()
    );
    // The reader thread must still be alive (child not yet reaped).
    assert!(
        !session.has_exited(),
        "reader/child died during backpressure"
    );
    // State caught up: the channel is fully drained.
    session.drain(&mut state);
    assert!(
        !session.has_pending(),
        "events still queued after catch-up drain"
    );
    session.terminate();
}

#[test]
fn resize_propagates_to_child() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    session.resize(120, 40);
    state.resize(120, 40);
    // `stty size` prints "<rows> <cols>"; the child sees the kernel winsize.
    // Marker composed to avoid matching the command echo.
    session.write(b"stty size; echo _R$(echo ESIZE)_DONE_\n");
    let ok = drain_until(
        &session,
        &mut state,
        Instant::now() + Duration::from_secs(15),
        |s| grid_text(s).contains("_RESIZE_DONE_"),
    );
    assert!(ok, "resize marker missing:\n{}", grid_text(&state));
    let text = grid_text(&state);
    // Rows are NUL-padded to the full width; trim NULs + whitespace.
    let stty_line = text
        .lines()
        .map(|l| {
            l.trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string()
        })
        .find(|l| {
            l.chars().all(|c| c.is_ascii_digit() || c == ' ') && l.split_whitespace().count() == 2
        })
        .unwrap_or_default();
    assert!(
        stty_line.contains("40") && stty_line.contains("120"),
        "child did not observe the resized size; got: {:?}\n--- grid ---\n{}",
        stty_line,
        text
    );
    session.terminate();
}

/// TUI smoke: an alternate-screen + cursor-hide sequence (what vim/less emit)
/// must round-trip through the PTY without corrupting state.
#[test]
fn alt_screen_smoke_via_pty() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    session.write(
        b"printf '\\033[?1049h\\033[?25l\\033[H\\033[2Jvi\\033[?25h\\033[?1049l'; echo ALT_DONE\n",
    );
    let ok = drain_until(
        &session,
        &mut state,
        Instant::now() + Duration::from_secs(15),
        |s| grid_text(s).contains("ALT_DONE"),
    );
    assert!(ok, "alt-screen smoke failed:\n{}", grid_text(&state));
    // After leaving the alt screen we must be back on the normal screen.
    assert!(!state.modes.alt_screen, "state stuck on alternate screen");
    assert!(state.modes.cursor_visible, "cursor left hidden");
    session.terminate();
}

#[test]
fn rapid_input_does_not_lose_bytes() {
    let Some((session, mut state)) = spawn_session() else {
        eprintln!("skipped: no shell or PTY available");
        return;
    };
    // Rapid typing: 400 chars in one burst (no newline — they stay on the
    // command line and are echoed back).
    let burst: Vec<u8> = (0..400).map(|i| b'a' + (i % 26) as u8).collect();
    session.write(&burst);
    session.write(b"\n");
    let ok = drain_until(
        &session,
        &mut state,
        Instant::now() + Duration::from_secs(15),
        |s| grid_text(s).contains("abcdefghijklmnopqrstuvwxyzabcdef"),
    );
    assert!(ok, "rapid input bytes were lost");
    session.terminate();
}
