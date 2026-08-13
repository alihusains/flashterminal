//! Terminal Session — the ownership hub between the PTY and the UI thread.
//!
//! ```text
//! shell ──► PTY master ──► [reader + parser thread] ──► bounded channel ──► UI thread
//!                                                                              │
//!                                    TerminalCommand ───────────────────────────┘
//! ```
//!
//! * The **UI thread** owns the authoritative [`TerminalState`] and the
//!   renderer. It calls [`Session::drain`] to apply pending events and sends
//!   [`Command`]s back into the session.
//! * The **reader thread** (spawned per session) performs blocking reads from
//!   the PTY master, parses bytes into events on that thread, and forwards
//!   batches over a bounded channel. The channel provides backpressure:
//!   the reader blocks when the UI thread falls behind, bounding memory.
//! * No locks are held across a blocking read; [`PtyManager`] guards only
//!   short lookups.
//!
//! `TerminalSession` is the future unit of multiplexing: nothing in this
//! crate assumes a single session.

pub mod adapters;
pub mod agent;
pub mod credential;
pub mod execution;
pub mod launch;
pub mod provider;
pub mod redact;
pub mod work;

use crossbeam_channel::{bounded, Receiver};
use pty::{PtyManager, ReadResult};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use terminal_core::{TerminalEvent, TerminalState};
use terminal_parser::Parser;

/// Events the active session pushes to the UI thread.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// A batch of terminal events, applied in order by the UI thread.
    Terminal(Vec<TerminalEvent>),
    /// EOF on the PTY master: the child process exited.
    Exited { code: Option<i32> },
}

/// Instrumentation counters exposed for validation (Phase 0.5.1).
///
/// These are cheap atomics incremented on the reader thread and read from
/// the measuring thread; they feed the PTY throughput / event-queue-depth /
/// render-coalescing tests.
#[derive(Debug, Default)]
pub struct SessionStats {
    /// Raw bytes read from the PTY master.
    pub bytes_read: AtomicU64,
    /// Parsed terminal events produced by the parser.
    pub events_read: AtomicU64,
    /// Batches (channel sends) forwarded to the UI thread.
    pub batches: AtomicU64,
}

const CHANNEL_CAPACITY: usize = 1024;

/// Wake callback: fires from reader threads when a batch is enqueued.
pub type WakeCallback = Box<dyn Fn() + Send>;
/// Raw-output tap: receives every chunk read from the PTY master (reader
/// thread — must stay fast). Used by the agent runtime's activity pump.
pub type OutputTap = Box<dyn Fn(&[u8]) + Send>;

/// A live shell session with its own reader thread and bounded channel.
pub struct Session {
    id: String,
    pty: Arc<PtyManager>,
    event_rx: Receiver<SessionEvent>,
    exited: Arc<AtomicBool>,
    stats: Arc<SessionStats>,
}

impl Session {
    /// Spawns `shell` in `cwd`, starts the reader/parser thread, and returns
    /// a session handle. The caller additionally creates a [`TerminalState`]
    /// of the same size — the state stays on the UI thread.
    pub fn spawn(
        pty: Arc<PtyManager>,
        shell: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<(Self, i64)> {
        Self::spawn_with_options(pty, shell, &[], cwd, &[], cols, rows, None, None)
    }

    /// Like [`Session::spawn`], but fires `wake` (from the reader thread)
    /// every time a batch is enqueued or the session reaches EOF. The desktop
    /// uses this to wake the winit event loop immediately when PTY output
    /// arrives, instead of waiting for a poll timer.
    ///
    /// The callback is invoked from the reader thread only, so it needs to
    /// be `Send` but not `Sync` (winit's `EventLoopProxy` is not `Sync`).
    pub fn spawn_with_wake(
        pty: Arc<PtyManager>,
        shell: &str,
        cwd: &str,
        cols: u16,
        rows: u16,
        wake: Option<Box<dyn Fn() + Send>>,
    ) -> anyhow::Result<(Self, i64)> {
        Self::spawn_with_options(pty, shell, &[], cwd, &[], cols, rows, wake, None)
    }

    /// Full spawn: arbitrary command + arguments + environment additions,
    /// an optional `wake` callback and an optional raw-output `tap`.
    ///
    /// * `env` entries are added on top of the inherited process
    ///   environment (never a full replacement) — agent adapters inject
    ///   credentials this way. Nothing here is logged or persisted.
    /// * `tap` is invoked from the reader thread with every raw chunk read
    ///   from the PTY master (before parsing). Agent sessions use it to feed
    ///   an activity detector without a second PTY implementation. It must
    ///   be fast — never block on anything long-lived.
    #[allow(clippy::too_many_arguments)] // the full spawn surface; see below
    pub fn spawn_with_options(
        pty: Arc<PtyManager>,
        command: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        wake: Option<WakeCallback>,
        tap: Option<OutputTap>,
    ) -> anyhow::Result<(Self, i64)> {
        let (id, pid) = pty.spawn_with_env(command, args, cwd, env, cols, rows)?;
        let (event_tx, event_rx) = bounded(CHANNEL_CAPACITY);
        let exited = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(SessionStats::default());

        let pty_reader = Arc::clone(&pty);
        let id_reader = id.clone();
        let event_tx_reader = event_tx.clone();
        let exited_reader = Arc::clone(&exited);
        let stats_reader = Arc::clone(&stats);

        std::thread::Builder::new()
            .name(format!("pty-io-{}", &id[..id.len().min(8)]))
            .spawn(move || {
                let mut parser = Parser::new();
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match pty_reader.read_available(&id_reader, &mut buf) {
                        Ok(ReadResult::Data(0)) => {
                            // WouldBlock on a non-blocking fd: back off
                            // briefly instead of hot-spinning the thread.
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Ok(ReadResult::Data(n)) => {
                            stats_reader
                                .bytes_read
                                .fetch_add(n as u64, Ordering::Relaxed);
                            // Raw-output tap (agent activity detection).
                            if let Some(tap) = &tap {
                                tap(&buf[..n]);
                            }
                            parser.advance_bytes(&buf[..n]);
                            let events = parser.take_events();
                            if !events.is_empty() {
                                stats_reader
                                    .events_read
                                    .fetch_add(events.len() as u64, Ordering::Relaxed);
                                // Bounded channel: backpressure on the reader.
                                if event_tx_reader
                                    .send(SessionEvent::Terminal(events))
                                    .is_err()
                                {
                                    break;
                                }
                                stats_reader.batches.fetch_add(1, Ordering::Relaxed);
                                if let Some(w) = &wake {
                                    w();
                                }
                            }
                        }
                        Ok(ReadResult::Eof) => {
                            let _ = event_tx_reader.send(SessionEvent::Exited { code: None });
                            exited_reader.store(true, Ordering::SeqCst);
                            if let Some(w) = &wake {
                                w();
                            }
                            tracing::info!("PTY session {} reached EOF", id_reader);
                            break;
                        }
                        Err(e) => {
                            tracing::error!("PTY read error on {}: {}", id_reader, e);
                            break;
                        }
                    }
                }
            })?;

        Ok((
            Self {
                id,
                pty,
                event_rx,
                exited,
                stats,
            },
            pid,
        ))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// True if the session has events queued for the UI thread (non-blocking).
    /// Lets an event-driven event loop request a redraw only when needed.
    pub fn has_pending(&self) -> bool {
        !self.event_rx.is_empty()
    }

    /// Number of event batches currently queued for the UI thread. Used by
    /// the validation harness to observe event-queue depth under load.
    pub fn pending_len(&self) -> usize {
        self.event_rx.len()
    }

    /// Instrumentation counters (bytes/events/batches read by the reader
    /// thread). See [`SessionStats`].
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Drains all pending events without blocking. Returns true if any were
    /// applied (the caller may then request a redraw).
    pub fn drain(&self, state: &mut TerminalState) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.event_rx.try_recv() {
            match ev {
                SessionEvent::Terminal(events) => {
                    for e in events {
                        state.apply_event(e);
                    }
                    changed = true;
                }
                SessionEvent::Exited { .. } => {
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn write(&self, bytes: &[u8]) {
        // Writes go straight to the PTY master (fast path for keystrokes);
        // no channel hop.
        let _ = self.pty.write(&self.id, bytes);
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.pty.resize(&self.id, cols, rows);
    }

    pub fn terminate(&self) {
        let _ = self.pty.terminate(&self.id);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.terminate();
    }
}
