//! Notification abstraction (§33).
//!
//! Phase 1 implements terminal/process notifications only. The type is
//! future-proofed for agent/remote events without implementing them.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::model::PaneId;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationKind {
    /// A pane's child process exited.
    ProcessExited { pane_id: PaneId, code: Option<i32> },
    /// A PTY/session error occurred (e.g. read error, spawn failure).
    SessionError { pane_id: PaneId, message: String },
    /// The application failed to persist/restore a workspace.
    PersistenceError { message: String },
    // Phase 2C §21: meaningful agent notifications only (never per-output
    // or per-state-transition noise).
    AgentCompleted { agent: String, pane_id: PaneId },
    AgentFailed { agent: String, code: Option<i32>, pane_id: PaneId },
    AgentNeedsApproval { agent: String, pane_id: PaneId },
    AgentNeedsInput { agent: String, pane_id: PaneId },
    AgentProviderFailure { agent: String, message: String, pane_id: PaneId },
    /// The application requires the user's attention somewhere.
    AttentionSummary { needs_you: u32, failed: u32, pane_id: PaneId },
}

/// Quiet-mode preferences (2c.md §22): workspace-level notification policy.
/// Default: notify on "needs me" + failures; not on every start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPrefs {
    pub on_needs_me: bool,
    pub on_failure: bool,
    pub on_completion: bool,
    pub on_start: bool,
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            on_needs_me: true,
            on_failure: true,
            on_completion: false,
            on_start: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub kind: NotificationKind,
    /// Millisecond timestamp (monotonic-ish wall clock).
    pub at_ms: u64,
}

impl Notification {
    pub fn new(kind: NotificationKind) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self { kind, at_ms }
    }
}

pub type NotificationSink = Arc<dyn Fn(Notification) + Send + Sync>;

/// Simple broadcast center. The desktop/CLI subscribe; the engine emits.
#[derive(Default, Clone)]
pub struct NotificationCenter {
    sinks: Vec<NotificationSink>,
}

impl NotificationCenter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&mut self, sink: NotificationSink) {
        self.sinks.push(sink);
    }

    pub fn emit(&self, n: Notification) {
        for s in &self.sinks {
            s(n.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn center_broadcasts() {
        let mut c = NotificationCenter::new();
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::clone(&count);
        c.subscribe(Arc::new(move |_| {
            c2.fetch_add(1, Ordering::SeqCst);
        }));
        c.emit(Notification::new(NotificationKind::PersistenceError {
            message: "x".into(),
        }));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
