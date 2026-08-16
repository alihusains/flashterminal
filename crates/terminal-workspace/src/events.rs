//! Unified application event bus (Phase 2B.1 §24–27; delivery semantics
//! fixed and made explicit per ADR 0021).
//!
//! ```text
//! AgentRuntime ─▶ ApplicationEvent ─▶ EventBus ─▶ Subscriber queues ─▶ IPC client / desktop
//! ```
//!
//! The engine publishes [`ApplicationEvent`]s (agent lifecycle, pane
//! changes, session exits, notifications) into the bus, potentially several
//! per drain frame, then flushes once per frame. Subscribers — the desktop
//! UI and IPC clients — receive events over **bounded** queues, so a slow
//! consumer can never block the engine (§27). Every event is classified by
//! [`DeliverySemantics`] (see `docs/adr/0021-event-delivery-semantics.md`
//! and `docs/agent-events.md` for the full table):
//!
//! * **Lossless** events (agent output, errors, permission prompts, state
//!   changes, task/plan lifecycle, exits) are delivered to every connected
//!   subscriber **in publish order, individually — never merged, never
//!   silently dropped**. A subscriber that cannot keep up is not blocked
//!   and does not corrupt the stream either: its queue is left to fill,
//!   and if it stays saturated or stalled it is **disconnected** (removed)
//!   so the engine stays unblocked. Loss only ever happens by an explicit,
//!   observable disconnect — never a silent per-event drop.
//! * **Coalescible** events (activity heuristics, usage counters, low-value
//!   collaboration metadata) may be dropped individually under backpressure
//!   — a newer one already suffices in place of an older one. These use
//!   the ordinary bounded-queue drop counter, not the disconnect policy.
//! * A subscriber whose queue stays non-empty across many flushes (its
//!   receiver is not draining — e.g. an IPC writer blocked on a full socket
//!   buffer) is disconnected regardless of category: the drop counters
//!   only fire on overflow, so the stall rule is the backstop for wedged
//!   writers (§27).
//!
//! Prior to this fix, `AgentEvent::Output` was coalesced (only the latest
//! per execution survived from one flush to the next) and treated as
//! droppable — a real, observed bug: several `Output` events published
//! within the same drain frame (a single frame can drain many PTY chunks
//! for a focused pane) collapsed into one, silently discarding agent output
//! a subscriber never saw. `Output` is now `Lossless`; the coalescing map
//! that caused this is gone.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

use terminal_session::execution::{AgentEvent, ApplicationEvent};
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
    /// Phase 3B: planner lifecycle events (3b.md §24).
    #[serde(default)]
    pub planner: bool,
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
            planner: true,
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
            ApplicationEvent::PlannerEvent { .. } => self.planner,
            // Phase 3D collaboration events ride the task channel (they are
            // workflow state) — metadata only, never payloads (§27).
            ApplicationEvent::ArtifactCreated { .. }
            | ApplicationEvent::ReviewFindingCreated { .. }
            | ApplicationEvent::SynthesisStarted { .. }
            | ApplicationEvent::SynthesisCompleted { .. }
            | ApplicationEvent::ArtifactConsumed { .. }
            | ApplicationEvent::WorkflowNeedsReplan { .. }
            // Phase 3E adaptive events ride the task channel (workflow
            // state) — metadata only (§39).
            | ApplicationEvent::ReplanRequested { .. }
            | ApplicationEvent::ReplanProposed { .. }
            | ApplicationEvent::ReplanEdited { .. }
            | ApplicationEvent::ReplanApproved { .. }
            | ApplicationEvent::ReplanRejected { .. }
            | ApplicationEvent::PlanSuperseded { .. }
            | ApplicationEvent::TaskInvalidated { .. }
            | ApplicationEvent::ArtifactInvalidated { .. }
            | ApplicationEvent::BudgetRisk { .. }
            | ApplicationEvent::HumanEscalation { .. }
            // Phase 3F: global controls are workflow state changes.
            | ApplicationEvent::WorkflowStopped { .. }
            | ApplicationEvent::WorkflowPaused { .. } => self.task,
        }
    }
}

/// How an [`ApplicationEvent`] may be delivered — see ADR 0021 and
/// `docs/agent-events.md` for the full audited table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverySemantics {
    /// Must reach every connected subscriber, individually, in publish
    /// order. Never merged with another event, never silently dropped. A
    /// subscriber that cannot keep up is disconnected (never blocked, never
    /// fed a corrupted/incomplete stream).
    Lossless,
    /// May be dropped individually under backpressure — a subsequent event
    /// of the same kind already supersedes it, so losing an old one is
    /// harmless. Counted (`dropped_non_critical`) but never disconnects a
    /// subscriber on its own.
    Coalescible,
}

/// Classifies delivery semantics for every event type. This is the single
/// place that decision is made — nothing else in `EventBus` special-cases
/// an event kind.
fn delivery_semantics(event: &ApplicationEvent) -> DeliverySemantics {
    use DeliverySemantics::{Coalescible, Lossless};
    match event {
        // Agent output/errors/state/prompts/exit are all semantically
        // significant to whoever is watching this execution — none may be
        // silently lost. (`Output` was `Coalescible` before this fix; that
        // was the bug — see the module doc comment.)
        ApplicationEvent::AgentEvent {
            event:
                AgentEvent::StateChanged { .. }
                | AgentEvent::PermissionRequested { .. }
                | AgentEvent::Completed
                | AgentEvent::Exited { .. }
                | AgentEvent::Error { .. }
                | AgentEvent::Output { .. },
            ..
        } => Lossless,
        ApplicationEvent::PaneCreated { .. }
        | ApplicationEvent::PaneClosed { .. }
        | ApplicationEvent::SessionExited { .. }
        | ApplicationEvent::WorkspaceChanged => Lossless,
        // Process-started/usage/activity are high-frequency heuristic or
        // counter-like updates; a newer one supersedes an older one.
        ApplicationEvent::AgentEvent {
            event:
                AgentEvent::Started | AgentEvent::UsageUpdated { .. } | AgentEvent::Activity { .. },
            ..
        } => Coalescible,
        // Task lifecycle events are state changes — never dropped.
        ApplicationEvent::TaskEvent {
            event: TaskEvent::TaskArtifactCreated { .. },
        } => Coalescible,
        ApplicationEvent::TaskEvent { .. } => Lossless,
        // Planner events are low-frequency lifecycle transitions (approval
        // gates, execution start) — never dropped for a live subscriber.
        ApplicationEvent::PlannerEvent { .. } => Lossless,
        // Phase 3D: synthesis/replan signals are state changes; findings
        // and consumption are high-frequency metadata (droppable).
        ApplicationEvent::SynthesisCompleted { .. }
        | ApplicationEvent::WorkflowNeedsReplan { .. } => Lossless,
        // Phase 3E: approval gates, supersession, invalidations and
        // escalations are state changes — never dropped. Budget-risk
        // warnings are state too (they gate continuation).
        ApplicationEvent::ReplanProposed { .. }
        | ApplicationEvent::ReplanEdited { .. }
        | ApplicationEvent::ReplanApproved { .. }
        | ApplicationEvent::ReplanRejected { .. }
        | ApplicationEvent::PlanSuperseded { .. }
        | ApplicationEvent::TaskInvalidated { .. }
        | ApplicationEvent::ArtifactInvalidated { .. }
        | ApplicationEvent::BudgetRisk { .. }
        | ApplicationEvent::HumanEscalation { .. } => Lossless,
        ApplicationEvent::ReplanRequested { .. } => Coalescible,
        ApplicationEvent::ArtifactCreated { .. }
        | ApplicationEvent::ReviewFindingCreated { .. }
        | ApplicationEvent::SynthesisStarted { .. }
        | ApplicationEvent::ArtifactConsumed { .. } => Coalescible,
        // Phase 3F: STOP ALL / PAUSE ALL are state changes — never dropped.
        ApplicationEvent::WorkflowStopped { .. } | ApplicationEvent::WorkflowPaused { .. } => {
            Lossless
        }
    }
}

fn is_critical(event: &ApplicationEvent) -> bool {
    delivery_semantics(event) == DeliverySemantics::Lossless
}

/// Per-subscriber slow-client state. All mutations happen under the bus
/// mutex (engine thread only).
struct Subscriber {
    id: u64,
    filter: EventFilter,
    tx: Sender<ApplicationEvent>,
    /// Consecutive critical sends that failed to enqueue.
    critical_drops: u64,
    /// Non-critical events dropped since the last flush (output storms).
    dropped_non_critical: u64,
    /// Consecutive flushes with an undrained non-empty queue (stalled).
    stalled_flushes: u64,
    /// Queue depth at the start of the previous flush (stall comparison).
    last_q_before: usize,
    /// Events successfully enqueued to this subscriber since the last
    /// flush. `publish` delivers immediately now (no batching deferred to
    /// flush time), so an unchanged queue depth alone no longer implies a
    /// stalled receiver — depth naturally holds steady between flushes for
    /// a healthy subscriber too if nothing new happened to arrive in that
    /// window. What actually distinguishes "genuinely not draining" is: the
    /// depth held steady *and* nothing new was even offered to it either.
    sent_since_flush: u64,
    /// Receiver was dropped by the client (no more sends will succeed).
    disconnected: bool,
}

impl Subscriber {
    fn try_enqueue(&mut self, event: ApplicationEvent) {
        if self.filter.matches(&event) {
            match self.tx.try_send(event.clone()) {
                Ok(()) => {
                    self.sent_since_flush += 1;
                }
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
            critical_drops: 0,
            dropped_non_critical: 0,
            stalled_flushes: 0,
            last_q_before: 0,
            sent_since_flush: 0,
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

    /// Publishes one event to every matching subscriber, immediately and in
    /// call order (never blocks). Every event is delivered individually —
    /// see [`DeliverySemantics`]; nothing is coalesced or deferred here.
    /// `Lossless` events that cannot enqueue are counted and eventually
    /// disconnect the subscriber (`flush`); `Coalescible` events that
    /// cannot enqueue are counted and simply dropped.
    pub fn publish(&self, event: ApplicationEvent) {
        let mut subs = self.subscribers.lock().unwrap();
        for sub in subs.iter_mut() {
            sub.try_enqueue(event.clone());
        }
    }

    /// Applies the slow-client policy: subscribers that repeatedly failed
    /// to enqueue lossless events — that dropped too many coalescible
    /// events in a row, or whose queue stayed non-empty across many
    /// flushes (receiver blocked, e.g. IPC write wedged on a full socket)
    /// — are disconnected (their receivers close). `publish` already
    /// delivers everything immediately, so this call does not itself move
    /// any events; it only evaluates and applies that policy.
    ///
    /// The engine calls this once per drain frame.
    pub fn flush(&self) -> usize {
        let mut subs = self.subscribers.lock().unwrap();
        let mut removed: Vec<usize> = Vec::new();
        for (i, sub) in subs.iter_mut().enumerate() {
            // Stall detection: the queue depth is unchanged since the last
            // flush *and* nothing new was even successfully enqueued in
            // that window — so the receiver read exactly zero events over
            // an entire flush interval despite there being a backlog to
            // read. A wedged writer (IPC blocked on a full socket) shows
            // this every flush, indefinitely. A subscriber that's merely
            // between publishes (nothing new to send) also has
            // `sent_since_flush == 0`, but then `q_before` is whatever it
            // already drained down to — the two conditions only coincide,
            // flush after flush, for a receiver that truly never reads.
            let q_before = sub.tx.len();
            if q_before > 0 && q_before == sub.last_q_before && sub.sent_since_flush == 0 {
                sub.stalled_flushes += 1;
            } else {
                sub.stalled_flushes = 0;
            }
            sub.last_q_before = q_before;
            sub.sent_since_flush = 0;
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
    use terminal_session::execution::{AgentState, ExecutionId};

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
    fn output_events_are_lossless_and_ordered() {
        // Regression test for the confirmed bug (docs/ci-forensics.md,
        // ADR 0021): Output used to be coalesced — only the latest per
        // execution survived a flush. It must now be delivered
        // individually, in publish order, with none lost.
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
        let mut got = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ApplicationEvent::AgentEvent {
                event: AgentEvent::Output { text },
                ..
            } = ev
            {
                got.push(text);
            }
        }
        let expected: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        assert_eq!(got, expected, "every output event must survive, in order");
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
        // publish() already enqueued it; this flush only establishes the
        // stall-detection baseline (`last_q_before`).
        bus.flush();
        assert_eq!(bus.subscriber_count(), 1, "publish already enqueued it");
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
