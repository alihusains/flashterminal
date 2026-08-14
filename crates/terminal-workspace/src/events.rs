//! Unified application event bus (Phase 2B.1 §24–27).
//!
//! ```text
//! AgentRuntime ─▶ ApplicationEvent ─▶ EventBus ─▶ Subscriber queues ─▶ IPC client / desktop
//! ```
//!
//! The engine publishes [`ApplicationEvent`]s (agent lifecycle, pane
//! changes, session exits, notifications) into the bus once per drain
//! frame. Subscribers — the desktop UI and IPC clients — receive events
//! over **bounded** queues, so a slow consumer can never block the engine
//! (§27):
//!
//! * Output events are **coalesced** (latest-wins per execution) and then
//!   **dropped** if the subscriber's queue is full.
//! * State/control events (state changes, permission prompts, exits) are
//!   never dropped; instead a subscriber that cannot keep up has its
//!   remaining queue drained and is **disconnected** (removed) so the
//!   engine stays unblocked.
//! * A subscriber whose queue stays non-empty across many flushes (its
//!   receiver is not draining — e.g. an IPC writer blocked on a full socket
//!   buffer) is likewise **disconnected**: the drop counters only fire on
//!   overflow, so the stall rule is the backstop for wedged writers (§27).

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

use terminal_session::execution::{AgentEvent, ApplicationEvent, ExecutionId};
use terminal_session::orchestration::TaskEvent;

/// Maximum events buffered per subscriber before the slow-client policy
/// kicks in (output coalescing + drops + disconnect).
pub const SUBSCRIBER_QUEUE_CAPACITY: usize = 1024;
/// Consecutive critical drops before a subscriber is disconnected.
pub const MAX_CRITICAL_DROPS: u64 = 8;
/// Coalesced output batches dropped while the queue stayed full before the
/// subscriber is disconnected (output-flooded clients trip this even when
/// no critical event ever collides).
pub const MAX_DROPPED_OUTPUT_BATCHES: u64 = 64;
/// Consecutive flushes with a non-empty queue before a subscriber is
/// disconnected. A receiver that never drains its queue (e.g. an IPC writer
/// blocked on a full socket buffer because the client never reads) leaves a
/// permanent backlog; drop/coalesce counters never trip because the queue
/// never overflows, so the stall rule is the disconnect backstop (§27).
pub const MAX_STALLED_FLUSHES: u64 = 400;

/// Which event channels a subscriber wants (Phase 2B.1 §25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EventFilter {
    #[serde(default)]
    pub workspace: bool,
    #[serde(default)]
    pub pane: bool,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub agent: bool,
    #[serde(default)]
    pub notification: bool,
    #[serde(default)]
    pub task: bool,
}

impl EventFilter {
    pub fn all() -> Self {
        Self {
            workspace: true,
            pane: true,
            terminal: true,
            agent: true,
            notification: true,
            task: true,
        }
    }

    pub fn agent_only() -> Self {
        Self {
            agent: true,
            ..Self::default()
        }
    }

    fn matches(&self, event: &ApplicationEvent) -> bool {
        match event {
            ApplicationEvent::WorkspaceChanged => self.workspace,
            ApplicationEvent::PaneCreated { .. } | ApplicationEvent::PaneClosed { .. } => self.pane,
            ApplicationEvent::SessionExited { .. } => self.terminal || self.agent,
            ApplicationEvent::AgentEvent { .. } => self.agent,
            ApplicationEvent::TaskEvent { .. } => self.task,
        }
    }
}

/// Events that must never be dropped for a live subscriber: losing one of
/// these would hide a user-visible state change or a security prompt.
fn is_critical(event: &ApplicationEvent) -> bool {
    match event {
        ApplicationEvent::AgentEvent {
            event:
                AgentEvent::StateChanged { .. }
                | AgentEvent::PermissionRequested { .. }
                | AgentEvent::Completed
                | AgentEvent::Exited { .. }
                | AgentEvent::Error { .. },
            ..
        } => true,
        ApplicationEvent::PaneCreated { .. }
        | ApplicationEvent::PaneClosed { .. }
        | ApplicationEvent::SessionExited { .. }
        | ApplicationEvent::WorkspaceChanged => true,
        ApplicationEvent::AgentEvent {
            event:
                AgentEvent::Output { .. }
                | AgentEvent::Started
                | AgentEvent::UsageUpdated { .. }
                | AgentEvent::Activity { .. },
            ..
        } => false,
        // Task lifecycle events are state changes — never dropped.
        ApplicationEvent::TaskEvent {
            event: TaskEvent::TaskArtifactCreated { .. },
        } => false,
        ApplicationEvent::TaskEvent { .. } => true,
    }
}

/// Per-subscriber slow-client state. All mutations happen under the bus
/// mutex (engine thread only).
struct Subscriber {
    id: u64,
    filter: EventFilter,
    tx: Sender<ApplicationEvent>,
    /// Latest output event per execution, delivered on flush (coalescing).
    pending_output: HashMap<ExecutionId, ApplicationEvent>,
    /// Consecutive critical sends that failed to enqueue.
    critical_drops: u64,
    /// Non-critical events dropped since the last flush (output storms).
    dropped_non_critical: u64,
    /// Consecutive flushes with an undrained non-empty queue (stalled).
    stalled_flushes: u64,
    /// Queue depth at the start of the previous flush (stall comparison).
    last_q_before: usize,
    /// Receiver was dropped by the client (no more sends will succeed).
    disconnected: bool,
}

impl Subscriber {
    fn try_enqueue(&mut self, event: ApplicationEvent) {
        if self.filter.matches(&event) {
            match self.tx.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    if is_critical(&event) {
                        self.critical_drops += 1;
                    } else {
                        self.dropped_non_critical += 1;
                    }
                }
                // Receiver dropped (client gone without unsubscribing):
                // mark for removal on the next flush.
                Err(TrySendError::Disconnected(_)) => {
                    self.disconnected = true;
                }
            }
        }
    }
}

/// Diagnostics snapshot for one subscriber (tests/telemetry, Phase 2B.1 §5).
#[derive(Debug, Clone, Copy)]
pub struct SubscriberStats {
    pub queued: usize,
    pub critical_drops: u64,
    pub dropped_non_critical: u64,
    pub stalled_flushes: u64,
}

/// The application event bus. Engine-owned; the desktop and IPC server
/// subscribe once and drain their receivers per frame / per event loop.
#[derive(Default)]
pub struct EventBus {
    subscribers: Mutex<Vec<Subscriber>>,
    next_id: u64,
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: 1,
        }
    }

    /// Subscribes with a filter. Returns `(subscription_id, receiver)`; the
    /// receiver must be drained continuously (the bus assumes it is).
    pub fn subscribe(&mut self, filter: EventFilter) -> (u64, Receiver<ApplicationEvent>) {
        let mut subs = self.subscribers.lock().unwrap();
        let id = self.next_id;
        self.next_id += 1;
        let (tx, rx) = bounded(SUBSCRIBER_QUEUE_CAPACITY);
        subs.push(Subscriber {
            id,
            filter,
            tx,
            pending_output: HashMap::new(),
            critical_drops: 0,
            dropped_non_critical: 0,
            stalled_flushes: 0,
            last_q_before: 0,
            disconnected: false,
        });
        (id, rx)
    }

    /// Removes a subscription (client gone, or slow-client disconnect).
    pub fn unsubscribe(&mut self, id: u64) -> bool {
        let mut subs = self.subscribers.lock().unwrap();
        let before = subs.len();
        subs.retain(|s| s.id != id);
        subs.len() != before
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().unwrap().len()
    }

    /// Per-subscriber queue diagnostics (Phase 2B.1 §5 instrumentation).
    pub fn subscriber_stats(&self) -> Vec<SubscriberStats> {
        self.subscribers
            .lock()
            .unwrap()
            .iter()
            .map(|s| SubscriberStats {
                queued: s.tx.len(),
                critical_drops: s.critical_drops,
                dropped_non_critical: s.dropped_non_critical,
                stalled_flushes: s.stalled_flushes,
            })
            .collect()
    }

    /// Publishes one event to every matching subscriber (never blocks).
    /// Output events are coalesced; critical events that cannot enqueue are
    /// counted and eventually disconnect the subscriber.
    pub fn publish(&self, event: ApplicationEvent) {
        let mut subs = self.subscribers.lock().unwrap();
        for sub in subs.iter_mut() {
            if !sub.filter.matches(&event) {
                continue;
            }
            // Output events: keep the latest per execution in the coalesce
            // slot and deliver on the next flush — a burst of output from
            // one agent becomes one subscriber message.
            if let ApplicationEvent::AgentEvent {
                event: AgentEvent::Output { .. },
                execution_id,
            } = &event
            {
                sub.pending_output
                    .insert(execution_id.clone(), event.clone());
                continue;
            }
            sub.try_enqueue(event.clone());
        }
    }

    /// Flushes coalesced output into subscriber queues; applies the
    /// slow-client policy: subscribers that repeatedly failed to enqueue
    /// critical events — that dropped too many coalesced output batches in
    /// a row, or whose queue stayed non-empty across many flushes (receiver
    /// blocked, e.g. IPC write wedged on a full socket) — are disconnected
    /// (their receivers close).
    ///
    /// The engine calls this once per drain frame (no per-event latency on
    /// the IPC path beyond the frame tick).
    pub fn flush(&self) -> usize {
        let mut subs = self.subscribers.lock().unwrap();
        let mut removed: Vec<usize> = Vec::new();
        for (i, sub) in subs.iter_mut().enumerate() {
            // Stall detection: the receiver's queue depth at the start of
            // the flush, before this frame's coalesced output is enqueued.
            // A wedged writer (IPC blocked on a full socket) leaves the
            // depth frozen at the same non-zero value across flushes; a
            // healthy subscriber's queue is empty (drained between frames)
            // or visibly changing, which resets the counter.
            let q_before = sub.tx.len();
            let outputs: Vec<ApplicationEvent> =
                sub.pending_output.drain().map(|(_, e)| e).collect();
            for ev in outputs {
                sub.try_enqueue(ev);
            }
            if q_before > 0 && q_before == sub.last_q_before {
                sub.stalled_flushes += 1;
            } else {
                sub.stalled_flushes = 0;
            }
            sub.last_q_before = q_before;
            if sub.critical_drops >= MAX_CRITICAL_DROPS
                || sub.dropped_non_critical >= MAX_DROPPED_OUTPUT_BATCHES
                || sub.stalled_flushes >= MAX_STALLED_FLUSHES
                || sub.disconnected
            {
                removed.push(i);
            }
        }
        // Remove disconnects (highest index first).
        let mut n = 0usize;
        for i in removed.into_iter().rev() {
            if i < subs.len() {
                subs.swap_remove(i);
                n += 1;
            }
        }
        n
    }
}

/// A live subscription handle pairing the bus id with its receiver
/// (convenience for the desktop/IPC server).
pub struct Subscription {
    pub id: u64,
    pub rx: Receiver<ApplicationEvent>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_session::execution::AgentState;

    fn state_event(eid: &ExecutionId) -> ApplicationEvent {
        ApplicationEvent::AgentEvent {
            execution_id: eid.clone(),
            event: AgentEvent::StateChanged {
                new_state: AgentState::Working,
                provenance: Some(terminal_session::execution::StateProvenance::HEURISTIC),
            },
        }
    }

    #[test]
    fn filter_gates_channels() {
        let mut bus = EventBus::new();
        let (_, rx) = bus.subscribe(EventFilter::agent_only());
        bus.publish(ApplicationEvent::WorkspaceChanged);
        bus.publish(state_event(&ExecutionId::new()));
        bus.flush();
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, 1);
    }

    #[test]
    fn output_events_coalesce_per_execution() {
        let mut bus = EventBus::new();
        let (_, rx) = bus.subscribe(EventFilter::all());
        let eid = ExecutionId::new();
        for i in 0..100 {
            bus.publish(ApplicationEvent::AgentEvent {
                execution_id: eid.clone(),
                event: AgentEvent::Output {
                    text: format!("line {i}"),
                },
            });
        }
        bus.flush();
        let mut got = 0;
        let mut last_text = String::new();
        while let Ok(ev) = rx.try_recv() {
            got += 1;
            if let ApplicationEvent::AgentEvent {
                event: AgentEvent::Output { text },
                ..
            } = ev
            {
                last_text = text;
            }
        }
        assert_eq!(got, 1, "100 output events must coalesce into one");
        assert_eq!(last_text, "line 99");
    }

    #[test]
    fn slow_client_is_disconnected_not_blocked() {
        let mut bus = EventBus::new();
        // Keep the receiver alive but never drain it — the bus must not
        // block on this subscriber and must eventually disconnect it.
        let (_id, _rx) = bus.subscribe(EventFilter::all());
        let before = bus.subscriber_count();
        assert_eq!(before, 1);
        // Saturate the subscriber's queue without ever draining it.
        let critical = state_event(&ExecutionId::new());
        let total = SUBSCRIBER_QUEUE_CAPACITY + MAX_CRITICAL_DROPS as usize + 10;
        for _ in 0..total {
            bus.publish(critical.clone());
            // publish() never blocks, and critical events eventually trip
            // the disconnect in flush().
            bus.flush();
        }
        assert_eq!(
            bus.subscriber_count(),
            0,
            "a saturated subscriber must be disconnected, never block the bus"
        );
    }

    #[test]
    fn stalled_subscriber_is_disconnected_not_blocked() {
        // A receiver that never drains leaves a permanent backlog that never
        // overflows (no drops) — the stall rule must still disconnect it.
        let mut bus = EventBus::new();
        let (_id, _rx) = bus.subscribe(EventFilter::all());
        bus.publish(ApplicationEvent::AgentEvent {
            execution_id: ExecutionId::new(),
            event: AgentEvent::Output {
                text: "line".into(),
            },
        });
        bus.flush();
        bus.flush();
        assert_eq!(bus.subscriber_count(), 1, "one flush enqueues the output");
        for _ in 0..MAX_STALLED_FLUSHES - 1 {
            bus.flush();
            assert_eq!(
                bus.subscriber_count(),
                1,
                "backlog alone must not disconnect"
            );
        }
        bus.flush();
        assert_eq!(
            bus.subscriber_count(),
            0,
            "a permanently stalled subscriber must be disconnected"
        );
    }

    #[test]
    fn drained_subscriber_is_never_disconnected() {
        let mut bus = EventBus::new();
        let (_id, rx) = bus.subscribe(EventFilter::all());
        for batch in 0..MAX_STALLED_FLUSHES * 2 {
            bus.publish(ApplicationEvent::AgentEvent {
                execution_id: ExecutionId::new(),
                event: AgentEvent::Output {
                    text: format!("line {batch}"),
                },
            });
            bus.flush();
            while rx.try_recv().is_ok() {}
            assert_eq!(
                bus.subscriber_count(),
                1,
                "a subscriber that keeps up must never be disconnected"
            );
        }
    }

    #[test]
    fn unsubscribe_removes_and_closes() {
        let mut bus = EventBus::new();
        let (id, rx) = bus.subscribe(EventFilter::all());
        bus.unsubscribe(id);
        bus.publish(state_event(&ExecutionId::new()));
        bus.flush();
        assert!(rx.try_recv().is_err(), "receiver must be closed");
    }
}
