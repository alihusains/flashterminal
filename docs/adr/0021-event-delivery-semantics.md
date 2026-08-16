# ADR 0021: Event Delivery Semantics

## Problem

`EventBus` (`crates/terminal-workspace/src/events.rs`) coalesced
`AgentEvent::Output` — the per-execution "keep only the latest, deliver on
next flush" behavior documented in the original module comment — and
classified it as droppable under backpressure. This was a real, confirmed
correctness bug: a single engine drain frame can call `session.drain()`
many times for a focused pane (`VISIBLE_DRAIN_CAP` / `usize::MAX`), each
call able to enqueue its own `AgentEvent::Output` from the agent pump
(`crates/terminal-session/src/agent.rs::process_chunk`, one per ~25ms PTY
read). All of those published within the same frame collapsed into a
single subscriber message at `flush()` time — real agent output a
subscriber (desktop UI, IPC client, CLI watcher) never saw, silently.

This surfaced first as CI flakiness two orchestration tests
(`terminal-workspace/tests/phase3d/main.rs::access_control_denies_unrelated_tasks`
and `::cross_worktree_consumption`) that assert specific text appears in an
agent's collected output stream — see `docs/ci-forensics.md` "Root cause,
part 4" for the forensic trail that led here.

## Goal

Guarantee semantically meaningful agent events reach every subscriber,
individually, in order, without silently discarding any — while keeping
the existing bounded-memory, never-block-the-engine architecture that
Phase 2B.1 built specifically to survive a slow or wedged consumer.

## Options Considered

1. **Disable all coalescing bus-wide.** Rejected as unnecessary and
   mischaracterizing the actual bug: auditing every event type
   (`delivery_semantics()`) showed `Output` was the *only* event `EventBus`
   ever coalesced — nothing else needed to change. Removing a general
   mechanism that mostly doesn't exist isn't a real option; the fix is a
   reclassification of one event type, not an architecture removal.
2. **Add a genuine two-lane (lossless queue + coalesced state store)
   design**, as a more general framework for future event types. Rejected
   as more than this bug requires right now: `EventBus` already has two
   lanes in substance — "never dropped, may disconnect a stalled
   subscriber" (`Lossless`) vs. "may be dropped individually, no
   disconnect" (`Coalescible`) — driven by one classification function.
   Building a second physical data structure for a distinction that
   already exists behaviorally would be speculative generality.
3. **Reclassify `Output` from `Coalescible`/droppable to `Lossless`, and
   deliver every event immediately and individually in `publish()`
   instead of batching non-critical output until `flush()`.** Selected.
   Smallest change that removes the actual defect; every other event
   type's classification is unchanged.

## Decision

### `DeliverySemantics`

```rust
enum DeliverySemantics {
    /// Delivered to every subscriber individually, in publish order.
    /// Never merged, never silently dropped. A subscriber that cannot
    /// keep up is disconnected — never blocked, never fed a
    /// corrupted/incomplete stream.
    Lossless,
    /// May be dropped individually under backpressure — a subsequent
    /// event of the same kind already supersedes an older one, so the
    /// loss is harmless. Counted, but never disconnects a subscriber
    /// on its own.
    Coalescible,
}
```

One function, `delivery_semantics(event: &ApplicationEvent) ->
DeliverySemantics`, is the single place this decision is made — nothing
else in `EventBus` special-cases an event kind. See `docs/agent-events.md`
for the full audited table.

`AgentEvent::Output` moved from `Coalescible` to `Lossless`. Every other
classification is unchanged from before this fix.

### Delivery mechanism

`publish()` now calls `try_enqueue` for every event unconditionally — no
per-execution coalescing map, no deferred batching. `flush()` no longer
moves any events (nothing is buffered outside the subscriber's own bounded
channel to move); it only evaluates and applies the slow-client policy
(disconnect on saturation or stall).

### Ordering

Per-execution ordering is a direct consequence of the mechanism: events
for one execution are published from one pump thread, in the order the
pump observed them, and enqueued to each subscriber's channel (FIFO) in
that same call order. No reordering step exists. Cross-execution ordering
is explicitly not guaranteed (matches the spec's requirement) — different
executions' pump threads publish independently.

### Backpressure / overflow

Consistent with the pre-existing design's core invariant ("a slow consumer
can never block the engine" — `publish`/`flush` are never allowed to
block), the chosen policy is **disconnect, not backpressure or spooling**:

- A `Lossless` event that fails to enqueue (`TrySendError::Full`)
  increments `critical_drops`; `MAX_CRITICAL_DROPS` (8) consecutive
  failures disconnects the subscriber.
- A `Coalescible` event that fails to enqueue increments
  `dropped_non_critical`; `MAX_DROPPED_OUTPUT_BATCHES` (64) disconnects the
  subscriber (this threshold and its meaning are unchanged by this fix —
  it already existed for other droppable event types).
- A subscriber whose queue depth holds steady across a flush *and*
  nothing new was even enqueued to it in that window (`sent_since_flush ==
  0`) is stalled; `MAX_STALLED_FLUSHES` (400) consecutive stalled flushes
  disconnects it. This is the backstop for a receiver that isn't reading
  at all, even below capacity (a wedged IPC writer, for instance) — see
  the code comment in `EventBus::flush` for why immediate delivery changed
  what "stalled" has to mean (queue depth alone stopped being a reliable
  signal once delivery is no longer deferred to flush time).

**Honest limit**: up to `MAX_CRITICAL_DROPS` (8) individual `Lossless`
events can still be lost in the narrow window right at the disconnect
boundary, for a subscriber that's genuinely saturated. This is a bounded,
counted, deterministic loss — not the unbounded, silent, per-burst loss
the coalescing bug caused — and it only ever affects a subscriber about to
be disconnected anyway (a healthy subscriber that drains normally never
loses anything; verified by `healthy_subscriber_survives_sustained_high_volume`
in `tests/eventbus.rs`). Actual zero-loss-under-any-circumstance delivery
would require true backpressure (blocking the engine — rejected, breaks
the "never block" invariant) or a spool to disk (rejected as
disproportionate to the bug; nothing in this codebase persists event
history today). Chose the option consistent with the existing
architecture, per the repair spec's own instruction.

### Performance

Removing coalescing is a real, measured, proportional cost: 50,000 output
events × 10 subscribers took 29.6ms coalesced vs. 40.5ms lossless on
development hardware (~1.7M vs. ~1.2M events/sec) — both orders of
magnitude above any realistic agent output rate. `docs/agent-events.md`
carries the full comparison; `tests/eventbus.rs`'s
`high_volume_multi_subscriber_throughput_stays_bounded` guards against a
throughput regression beyond that. This change does not touch
`terminal-core`/`terminal-parser`/`terminal-renderer` at all, so the
Phase 4 tracked benchmarks (input latency, render prep, parse throughput —
`benchmarks/baseline.json`) are unaffected.

## Consequences

- Every subscriber of `EventBus` — desktop UI, IPC clients, the `terminal
  agent watch` CLI — now receives every `Output`/`Error`/state/lifecycle
  event exactly once, in order, for as long as it keeps draining its
  queue.
- IPC and the CLI watcher needed no code changes: both are pure
  `ApplicationEvent` pass-throughs (`crates/terminal-workspace/src/ipc.rs`,
  `apps/cli/src/main.rs`), so the fix at the bus is sufficient.
- Secret redaction is unaffected: it happens upstream, at the pump
  (`Redactor::redact` in `process_chunk`), before text ever becomes an
  `AgentEvent::Output` — `EventBus` never touches payload content.
- `work.rs::replay_into` (deterministic fixture replay for `AgentWork`) is
  a separate mechanism, untouched by this change.
- A subscriber that is merely slow (not wedged) may now see somewhat
  larger, more frequent messages instead of fewer, larger coalesced ones —
  no behavioral contract depended on the old batching shape.
