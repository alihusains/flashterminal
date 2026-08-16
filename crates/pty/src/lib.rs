//! PTY Management
//!
//! Abstraction over the OS PTY layer via `portable-pty`.
//!
//! Locking discipline (Phase 0.5 audit fix):
//!
//! * No lock is held across a blocking `read`. `read_available` locks only
//!   long enough to look the session up (`Arc`), then the reader mutex is
//!   taken briefly per batch. Concurrent writes/resizes to *other* sessions
//!   are never blocked by a slow reader.
//! * The child handle is retained so exits are reaped (no zombies), and
//!   EOF on the master is reported as [`ReadResult::Eof`] to the caller so
//!   reader threads can stop instead of busy-looping.
//! * The master writer is taken exactly once at spawn (portable-pty forbids
//!   calling `take_writer` more than once) and stored in the session.

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use tracing::info;

/// Result of a non-blocking-ish read from the PTY master.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadResult {
    /// Bytes were read into the buffer.
    Data(usize),
    /// EOF: the child closed its side / the session ended.
    Eof,
}

/// FIFO byte buffer with O(1)-amortized front removal.
///
/// Phase 0.5.2 fix: `Vec::drain(..n)` on a large pending backlog is O(n)
/// per flush — with a multi-MB paste the flush path degenerated to O(n²)
/// and throughput collapsed to ~0.1 MB/s. This keeps a `Vec` (fast slice
/// writes) with a head offset, compacting only when the consumed prefix is
/// a large fraction of the buffer.
struct PendingWrite {
    buf: Vec<u8>,
    head: usize,
}

impl PendingWrite {
    fn new() -> Self {
        Self {
            buf: Vec::new(),
            head: 0,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.head >= self.buf.len()
    }

    #[inline]
    fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    #[inline]
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// First `n` (or fewer) bytes, as a slice.
    #[inline]
    fn front(&self, n: usize) -> &[u8] {
        let n = n.min(self.len());
        &self.buf[self.head..self.head + n]
    }

    #[inline]
    fn consume(&mut self, n: usize) {
        self.head += n;
        // Amortized compaction: only once the consumed prefix is large
        // relative to the buffer, so total cost stays linear.
        if self.head >= 64 * 1024 && self.head * 2 >= self.buf.len() {
            self.buf.drain(..self.head);
            self.head = 0;
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }
}

struct PtySession {
    master: Mutex<Box<dyn MasterPty + Send>>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// Bytes the UI thread wrote that the kernel couldn't accept yet.
    /// The reader loop flushes this as the child consumes input, so the
    /// UI thread never blocks on the PTY writer (Phase 0.5.1 fix).
    pending_write: Mutex<PendingWrite>,
    child: Mutex<Option<Box<dyn Child + Send + Sync>>>,
}

pub struct PtyManager {
    /// `native_pty_system()` returns `Box<dyn PtySystem + Send>` (no `Sync`
    /// bound in portable-pty 0.8), so it lives behind a mutex to keep the
    /// manager `Send + Sync` for `Arc` sharing across threads.
    pty_system: Mutex<Box<dyn portable_pty::PtySystem + Send>>,
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
    /// `terminate()` removes a session from `sessions` (to free its fds
    /// promptly) *before* any caller has a chance to `try_wait()` it — that
    /// call would otherwise always see "session not found" and never learn
    /// the exit code it just reaped. This holds the code `terminate()`
    /// captured so a subsequent `try_wait()` on the same id still resolves
    /// it instead of retrying against a session that no longer exists.
    terminated_codes: Mutex<HashMap<String, Option<i32>>>,
}

impl PtyManager {
    pub fn new() -> Result<Self> {
        let pty_system = native_pty_system();
        Ok(Self {
            pty_system: Mutex::new(pty_system),
            sessions: Mutex::new(HashMap::new()),
            terminated_codes: Mutex::new(HashMap::new()),
        })
    }

    /// Spawns `shell` in `cwd` with the given terminal size.
    /// Returns `(session_id, child_pid)` where pid is `-1` if unknown.
    pub fn spawn(&self, shell: &str, cwd: &str, cols: u16, rows: u16) -> Result<(String, i64)> {
        self.spawn_with_env(shell, &[], cwd, &[], cols, rows)
    }

    /// Like [`PtyManager::spawn`], but with explicit arguments and extra
    /// environment entries. `env` entries are *added* on top of the
    /// inherited process environment (never a full replacement), so callers
    /// only pass what they need (e.g. injected credentials for agent
    /// processes). The environment is passed straight to the child — it is
    /// never logged or persisted by this crate.
    pub fn spawn_with_env(
        &self,
        command: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
    ) -> Result<(String, i64)> {
        let mut cmd = CommandBuilder::new(command);
        for a in args {
            cmd.arg(a);
        }
        cmd.cwd(cwd);
        for (k, v) in env {
            cmd.env(k, v);
        }

        let pair = self.pty_system.lock().unwrap().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        // Phase 0.5.1 fix: the master fd must be non-blocking for writes.
        // `PtyManager::write` then buffers whatever the kernel won't accept
        // (instead of blocking the caller, which on the desktop is the same
        // thread that drains the event channel — a blocking write there
        // deadlocks the pipeline once the channel is full). The reader loop
        // flushes the buffer as the child consumes input. The reader path
        // already handles `WouldBlock` (`ReadResult::Data(0)`), so making
        // the shared fd non-blocking is safe for reads too.
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags >= 0 {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }

        let child = pair.slave.spawn_command(cmd)?;
        let pid = child.process_id().map(|p| p as i64).unwrap_or(-1);
        // The child must be reaped, not leaked.

        // The reader is a clone of the master fd; the writer is taken once
        // (portable-pty: "It is invalid to take the writer more than once").
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let id = uuid::Uuid::new_v4().to_string();

        let session = PtySession {
            master: Mutex::new(pair.master),
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(Some(writer)),
            pending_write: Mutex::new(PendingWrite::new()),
            child: Mutex::new(Some(child)),
        };

        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), Arc::new(session));
        info!(
            "Spawned PTY session {} with command {} in {}",
            id, command, cwd
        );

        Ok((id, pid))
    }

    /// Writes up to one chunk of the session's pending buffer to the kernel
    /// (non-blocking). Leaves the remainder for the next call.
    ///
    /// Pacing matters: dumping the whole buffer in one flush lets the tty
    /// line discipline overflow its canonical-mode input queue and *silently
    /// discard* bytes (macOS rings the bell and drops overflow). The old
    /// blocking `write_all` was naturally paced by the queue filling; this
    /// chunked flush reproduces that pacing without ever blocking the
    /// caller. The reader loop re-invokes this every iteration, so a large
    /// paste still drains at full speed.
    fn flush_pending(inner: &PtySession, pending: &mut PendingWrite) {
        if pending.is_empty() {
            return;
        }
        let mut guard = inner.writer.lock().unwrap();
        let Some(writer) = guard.as_mut() else {
            return;
        };
        let chunk = pending.len().min(1024);
        match writer.write(pending.front(chunk)) {
            Ok(0) => {}
            Ok(n) => {
                pending.consume(n);
            }
            // Slave input queue full: the child will consume later, at which
            // point the reader loop flushes again.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            // EIO / EBADF etc: the child is gone; drop the stale bytes.
            Err(_) => {
                pending.clear();
            }
        }
    }

    /// Reads available bytes into `buf`. Never blocks on other sessions;
    /// the reader mutex is held only for the duration of one `read`.
    pub fn read_available(&self, session_id: &str, buf: &mut [u8]) -> Result<ReadResult> {
        let inner = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .context("session not found")?;

        // Phase 0.5.1 fix: the reader loop is the natural flush point for
        // buffered input — it runs whenever the child makes progress, so
        // pending writes are delivered promptly without the writer ever
        // blocking the UI thread.
        {
            let mut pending = inner.pending_write.lock().unwrap();
            Self::flush_pending(&inner, &mut pending);
        }

        let mut guard = inner.reader.lock().unwrap();
        match guard.as_mut() {
            Some(reader) => match reader.read(buf) {
                Ok(0) => Ok(ReadResult::Eof),
                Ok(n) => Ok(ReadResult::Data(n)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(ReadResult::Data(0)),
                Err(e) => Err(e.into()),
            },
            None => Ok(ReadResult::Eof),
        }
    }

    /// Waits for the child process to exit (non-blocking check).
    pub fn try_wait(&self, session_id: &str) -> Result<Option<i32>> {
        let Some(inner) = self.sessions.lock().unwrap().get(session_id).cloned() else {
            // Already terminated: `terminate()` removes the session from
            // `sessions` but records what it reaped, so callers that raced
            // it (the agent pump polling for the exit code right after a
            // user-initiated stop) still get the real code instead of a
            // hard error that looks like "no such session ever existed".
            return match self.terminated_codes.lock().unwrap().get(session_id) {
                Some(code) => Ok(*code),
                None => Err(anyhow::anyhow!("session not found")),
            };
        };
        let mut child = inner.child.lock().unwrap();
        if let Some(child) = child.as_mut() {
            Ok(child.try_wait()?.map(|status| status.exit_code() as i32))
        } else {
            Ok(None)
        }
    }

    /// Writes to the PTY master. **Never blocks** (Phase 0.5.1 fix): the
    /// master fd is non-blocking, so anything the kernel won't accept is
    /// buffered per-session and flushed by the reader loop. This is what
    /// makes the burst/paste deadlock impossible — the caller (the desktop
    /// event loop, which also drains the channel) can't be wedged on a full
    /// slave input queue.
    pub fn write(&self, session_id: &str, data: &[u8]) -> Result<()> {
        let inner = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .context("session not found")?;
        let mut pending = inner.pending_write.lock().unwrap();
        pending.push(data);
        Self::flush_pending(&inner, &mut pending);
        Ok(())
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<()> {
        let inner = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .context("session not found")?;
        let master = inner.master.lock().unwrap();
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }

    /// Terminates the session: kills the child *and its process group*,
    /// reaps it, closes the reader/writer fds (which wakes the reader thread
    /// with EOF), and removes the session from the map.
    ///
    /// Phase 0.5.1 fix: killing only the direct child leaves descendants
    /// (e.g. `yes` launched by the shell) writing to the PTY forever, which
    /// keeps the reader thread holding the reader mutex almost continuously
    /// — `terminate` would livelock on `reader.lock()`. SIGKILL to the whole
    /// group closes the master's writers, so the blocking read returns EOF
    /// promptly and the reader mutex frees. The reader lock below is also
    /// time-bounded so a stuck reader can never hang `terminate`.
    pub fn terminate(&self, session_id: &str) -> Result<()> {
        let inner = self
            .sessions
            .lock()
            .unwrap()
            .remove(session_id)
            .context("session not found")?;

        // Kill the whole process group first (the child is its session
        // leader on unix), so no descendant can keep the PTY alive.
        #[cfg(unix)]
        if let Some(child) = inner.child.lock().unwrap().as_ref() {
            if let Some(pid) = child.process_id() {
                // Negative pid = process group. Harmless if already gone.
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }

        // Then kill + reap the direct child, capturing the exit code so a
        // caller's `try_wait()` after this returns can still observe it.
        let mut reaped_code = None;
        if let Some(mut child) = inner.child.lock().unwrap().take() {
            let _ = child.kill();
            for _ in 0..50 {
                if let Some(status) = child.try_wait()? {
                    reaped_code = Some(status.exit_code() as i32);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        self.terminated_codes
            .lock()
            .unwrap()
            .insert(session_id.to_string(), reaped_code);

        // Close the fds so any blocked reader thread wakes up with EOF.
        // Bounded: if the reader is stuck (e.g. blocked on a full channel
        // send), give up after 2 s instead of hanging the caller.
        let reader_taken: Option<Box<dyn Read + Send>> = {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match inner.reader.try_lock() {
                    Ok(mut g) => break g.take(),
                    Err(_) if std::time::Instant::now() > deadline => break None,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        };
        drop(reader_taken);
        drop(inner.writer.lock().unwrap().take());
        inner.pending_write.lock().unwrap().clear();
        // Dropping the master closes the fd → EOF on the child's slave.
        drop(inner);
        info!("Terminated PTY session {}", session_id);
        Ok(())
    }

    /// Number of live sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }
}
