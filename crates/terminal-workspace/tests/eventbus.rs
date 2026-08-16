//! Dedicated regression suite for `EventBus` delivery semantics
//! (ADR 0021, `docs/agent-events.md`, `docs/ci-forensics.md` "Root cause,
//! part 4").
//!
//! `AgentEvent::Output` used to be coalesced (only the latest per execution
//! survived a flush) and treated as droppable — several output events
//! published within the same drain frame silently collapsed into one,
//! discarding real agent output a subscriber never saw. This file locks in
//! the fix: lossless events are delivered individually, in order, to every
//! subscriber, while genuinely coalescible/metadata events may still be
//! dropped under backpressure, and a subscriber that cannot keep up is
//! disconnected rather than allowed to block the bus or silently swallow
//! an unbounded amount of output.
//!
//! End-to-end IPC coverage (real fake-agent process → PTY → pump →
//! `EventBus` → Unix socket) lives in `tests/ipc_stream.rs`, which already
//! has the harness for that; this file exercises `EventBus` directly so
//! ordering/loss/backpressure assertions aren't at the mercy of PTY timing.

use std::time::Duration;

use terminal_session::execution::{AgentEvent, AgentState, ApplicationEvent, ExecutionId};
use terminal_workspace::events::{
    EventBus, EventFilter, MAX_CRITICAL_DROPS, SUBSCRIBER_QUEUE_CAPACITY,
};

fn output(eid: &ExecutionId, text: impl Into<String>) -> ApplicationEvent {
    ApplicationEvent::AgentEvent {
        execution_id: eid.clone(),
        event: AgentEvent::Output { text: text.into() },
    }
}

fn state_changed(eid: &ExecutionId) -> ApplicationEvent {
    ApplicationEvent::AgentEvent {
        execution_id: eid.clone(),
        event: AgentEvent::StateChanged {
            new_state: AgentState::Working,
            provenance: None,
        },
    }
}

fn collect_output(rx: &crossbeam_channel::Receiver<ApplicationEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ApplicationEvent::AgentEvent {
            event: AgentEvent::Output { text },
            ..
        } = ev
        {
            out.push(text);
        }
    }
    out
}

/// §9: emit N output events faster than any flush interval; every one must
/// be received. Parametrized as the brief asks (10 / 100 / 1,000 / 10,000).
#[test]
fn output_events_are_lossless() {
    for &n in &[10usize, 100, 1_000, 10_000] {
        let mut bus = EventBus::new();
        let (_, rx) = bus.subscribe(EventFilter::all());
        let eid = ExecutionId::new();
        let mut got = Vec::new();
        // Drain periodically like a real subscriber (the engine flushes and
        // consumers drain every frame) — this proves losslessness across an
        // arbitrarily long run, not just "fits in one bounded queue".
        for i in 0..n {
            bus.publish(output(&eid, format!("line {i}")));
            if i % 200 == 0 {
                bus.flush();
                got.extend(collect_output(&rx));
            }
        }
        bus.flush();
        got.extend(collect_output(&rx));
        assert_eq!(
            got.len(),
            n,
            "emitted {n}, received {} — {} lost",
            got.len(),
            n - got.len()
        );
    }
}

/// §10: per-execution output must arrive in exactly the order published —
/// no missing, duplicate, or reordered entries.
#[test]
fn output_order_is_preserved_per_execution() {
    let mut bus = EventBus::new();
    let (_, rx) = bus.subscribe(EventFilter::all());
    let eid = ExecutionId::new();
    const N: usize = 10_000;
    let mut got = Vec::new();
    for i in 1..=N {
        bus.publish(output(&eid, i.to_string()));
        if i % 200 == 0 {
            bus.flush();
            got.extend(collect_output(&rx));
        }
    }
    bus.flush();
    got.extend(collect_output(&rx));
    let got: Vec<usize> = got.into_iter().map(|s| s.parse().unwrap()).collect();
    let expected: Vec<usize> = (1..=N).collect();
    assert_eq!(got, expected, "output must arrive in exact publish order");
}

/// §11: three agents interleaving output — each execution's own sequence
/// must be complete and in order. No requirement on ordering *across*
/// executions.
#[test]
fn multi_agent_output_is_lossless_per_execution() {
    let mut bus = EventBus::new();
    let (_, rx) = bus.subscribe(EventFilter::all());
    let agents: Vec<ExecutionId> = (0..3).map(|_| ExecutionId::new()).collect();
    const N: usize = 1_000;
    let mut per_agent: std::collections::HashMap<ExecutionId, Vec<usize>> =
        agents.iter().cloned().map(|e| (e, Vec::new())).collect();
    let drain_all = |bus: &EventBus, per_agent: &mut std::collections::HashMap<_, Vec<usize>>| {
        bus.flush();
        while let Ok(ev) = rx.try_recv() {
            if let ApplicationEvent::AgentEvent {
                execution_id,
                event: AgentEvent::Output { text },
            } = ev
            {
                per_agent
                    .get_mut(&execution_id)
                    .expect("known agent")
                    .push(text.parse().unwrap());
            }
        }
    };
    // Interleave publishes across agents (A1 B1 C1 A2 B2 C2 ...) to
    // exercise the coalescing bug's exact shape: multiple executions'
    // output arriving in the same window, previously colliding in the
    // per-execution coalesce map. Drain periodically (all three agents
    // share one bounded queue) like a real per-frame consumer.
    for i in 1..=N {
        for eid in &agents {
            bus.publish(output(eid, i.to_string()));
        }
        if i % 60 == 0 {
            drain_all(&bus, &mut per_agent);
        }
    }
    drain_all(&bus, &mut per_agent);
    let expected: Vec<usize> = (1..=N).collect();
    for eid in &agents {
        assert_eq!(
            per_agent[eid], expected,
            "agent {eid:?} must receive its full, ordered sequence"
        );
    }
}

/// §12/§20: a subscriber that never drains must not silently lose events
/// invisibly — everything it does have room for is retrievable intact (no
/// gaps/corruption up to capacity), and once truly overwhelmed it is
/// disconnected rather than left to accumulate forever or corrupt state.
#[test]
fn slow_subscriber_is_never_silently_corrupted_and_is_disconnected_on_overflow() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::all());
    let eid = ExecutionId::new();
    // Publish well past queue capacity without ever draining.
    let total = SUBSCRIBER_QUEUE_CAPACITY + MAX_CRITICAL_DROPS as usize + 50;
    for i in 0..total {
        bus.publish(output(&eid, i.to_string()));
        bus.flush();
    }
    assert_eq!(
        bus.subscriber_count(),
        0,
        "a subscriber that never drains past capacity must be disconnected, not left \
         accumulating forever"
    );
    // Whatever made it into the queue before disconnect must be the exact
    // prefix of what was published — no gaps, no reordering, no duplicate
    // entries — even though the tail was necessarily dropped (documented,
    // bounded loss at the disconnect boundary; see ADR 0021).
    let got: Vec<usize> = collect_output(&rx)
        .into_iter()
        .map(|s| s.parse().unwrap())
        .collect();
    assert!(
        got.len() <= SUBSCRIBER_QUEUE_CAPACITY,
        "queue is bounded by capacity"
    );
    let expected_prefix: Vec<usize> = (0..got.len()).collect();
    assert_eq!(
        got, expected_prefix,
        "whatever was delivered must be an exact, ordered, gap-free prefix"
    );
}

/// §13: one permanently-unread subscriber must not slow down or block a
/// second, healthy subscriber on the same bus.
#[test]
fn fast_subscriber_is_isolated_from_a_stalled_one() {
    let mut bus = EventBus::new();
    let (_slow_id, _slow_rx) = bus.subscribe(EventFilter::all()); // never drained
    let (_fast_id, fast_rx) = bus.subscribe(EventFilter::all());
    let eid = ExecutionId::new();
    const N: usize = 2_000;
    let t0 = std::time::Instant::now();
    for i in 0..N {
        bus.publish(output(&eid, i.to_string()));
        bus.flush();
        // The fast subscriber drains every round — it must never see
        // backpressure from the other subscriber's saturation.
        while fast_rx.try_recv().is_ok() {}
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "a stalled subscriber must not slow down a healthy one (took {elapsed:?})"
    );
    // The slow subscriber's eventual disconnect must not affect the fast
    // one's continued membership.
    assert!(
        bus.subscriber_count() >= 1,
        "the fast subscriber must still be attached"
    );
}

/// §17: coalescible/metadata events (not output) may still be dropped
/// individually under backpressure without disconnecting the subscriber —
/// the fix only changes `Output`'s category, not the backpressure policy
/// for genuinely droppable state.
#[test]
fn coalescible_events_still_drop_under_backpressure_without_disconnect() {
    let mut bus = EventBus::new();
    let (_id, _rx) = bus.subscribe(EventFilter::all()); // never drained
    let eid = ExecutionId::new();
    // ArtifactCreated is Coalescible (droppable) per delivery_semantics.
    let coalescible = ApplicationEvent::ArtifactCreated {
        artifact_id: "a1".into(),
        task_id: None,
        kind: "file".into(),
        description: "d".into(),
    };
    // Fill the queue, then overflow it by fewer than MAX_DROPPED_OUTPUT_BATCHES
    // — a modest, occasional overflow must drop those events without
    // disconnecting (that threshold exists precisely to catch a sustained
    // output-flooded client, which this is not; unrelated to this fix and
    // unchanged by it).
    for _ in 0..(SUBSCRIBER_QUEUE_CAPACITY + 10) {
        bus.publish(coalescible.clone());
        bus.flush();
    }
    assert_eq!(
        bus.subscriber_count(),
        1,
        "a handful of coalescible drops must never disconnect a subscriber"
    );
    let stats = bus.subscriber_stats();
    assert!(
        stats[0].dropped_non_critical > 0,
        "excess coalescible events must be counted as dropped, not silently vanish \
         unaccounted"
    );

    // Meanwhile a lossless event queued alongside real capacity still gets
    // through once the subscriber makes room (proves the two categories
    // are tracked independently).
    let (_id2, rx2) = bus.subscribe(EventFilter::all());
    bus.publish(output(&eid, "still delivered"));
    bus.flush();
    assert_eq!(collect_output(&rx2), vec!["still delivered".to_string()]);
}

/// §20: overflow of a lossless queue must be an explicit, observable,
/// bounded event — never an unbounded silent loss. `MAX_CRITICAL_DROPS`
/// bounds how many lossless sends can fail before disconnect; verify that
/// bound is real and the disconnect is the eventual, deterministic outcome
/// documented in ADR 0021 (not a hang, not indefinite accumulation).
#[test]
fn lossless_overflow_is_bounded_and_explicit() {
    let mut bus = EventBus::new();
    let (_id, _rx) = bus.subscribe(EventFilter::all()); // never drained
    let eid = ExecutionId::new();
    // Fill the queue exactly, then start overflowing with lossless events.
    for i in 0..SUBSCRIBER_QUEUE_CAPACITY {
        bus.publish(output(&eid, i.to_string()));
    }
    bus.flush();
    assert_eq!(bus.subscriber_count(), 1, "not yet overflowed");
    for _ in 0..MAX_CRITICAL_DROPS {
        bus.publish(state_changed(&eid));
        bus.flush();
    }
    assert_eq!(
        bus.subscriber_count(),
        0,
        "after MAX_CRITICAL_DROPS failed lossless sends the subscriber must be \
         disconnected — bounded, deterministic, never an indefinite hang or silent \
         accumulation"
    );
}

/// A subscriber that keeps up must never be penalized by the stall/overflow
/// policy, however long the run — this is the "never disconnect a healthy
/// consumer" counterpart to the overflow tests above.
#[test]
fn healthy_subscriber_survives_sustained_high_volume() {
    let mut bus = EventBus::new();
    let (_id, rx) = bus.subscribe(EventFilter::all());
    let eid = ExecutionId::new();
    for batch in 0..5_000usize {
        bus.publish(output(&eid, batch.to_string()));
        bus.flush();
        while rx.try_recv().is_ok() {}
        assert_eq!(
            bus.subscriber_count(),
            1,
            "a subscriber that drains every round must never be disconnected"
        );
    }
}

/// §18/§29: coarse throughput sanity check. Delivering `Output`
/// individually instead of coalescing it is inherently more expensive per
/// subscriber (proportional to actual event volume, not the old squashed
/// batch count) — measured directly against the pre-fix implementation:
/// 50,000 events × 10 subscribers took 29.6ms coalesced vs 40.5ms lossless
/// on this machine (~1.7M vs ~1.2M events/sec) — a real, expected,
/// bounded cost of not losing data, not a runaway regression. This asserts
/// a generous ceiling well above that, as a regression guard rather than a
/// precise benchmark (see `docs/agent-events.md` for the full numbers).
#[test]
fn high_volume_multi_subscriber_throughput_stays_bounded() {
    let mut bus = EventBus::new();
    let rxs: Vec<_> = (0..10)
        .map(|_| bus.subscribe(EventFilter::all()).1)
        .collect();
    let eid = ExecutionId::new();
    const N: usize = 50_000;
    let t0 = std::time::Instant::now();
    for i in 0..N {
        bus.publish(output(&eid, i.to_string()));
        if i % 200 == 0 {
            bus.flush();
            for rx in &rxs {
                while rx.try_recv().is_ok() {}
            }
        }
    }
    bus.flush();
    for rx in &rxs {
        while rx.try_recv().is_ok() {}
    }
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_millis(500),
        "50k lossless events × 10 subscribers took {elapsed:?}, expected well under 500ms \
         (measured ~40ms on dev hardware) — possible throughput regression"
    );
}
