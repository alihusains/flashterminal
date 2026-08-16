# Agent Events

How `ApplicationEvent`s move from the engine to subscribers (desktop UI,
IPC clients, the `terminal agent watch` CLI), and what each event type is
guaranteed to do when a subscriber can't keep up. See ADR 0021 for why
this fix exists and what was tried.

```text
AgentRuntime ─▶ ApplicationEvent ─▶ EventBus ─▶ Subscriber queues ─▶ IPC client / desktop / CLI
```

## Delivery semantics

Every event is classified `Lossless` or `Coalescible` by
`delivery_semantics()` in `crates/terminal-workspace/src/events.rs` — the
single place this decision is made.

| Event | Semantics | Can drop? | Ordering |
|---|---|---|---|
| `AgentEvent::Output` | Lossless | No | Per execution, publish order |
| `AgentEvent::Error` | Lossless | No | Per execution |
| `AgentEvent::StateChanged` | Lossless | No | Per execution |
| `AgentEvent::PermissionRequested` | Lossless | No | Per execution |
| `AgentEvent::Completed` | Lossless | No | Per execution |
| `AgentEvent::Exited` | Lossless | No | Per execution |
| `AgentEvent::Started` | Coalescible | Yes | Latest |
| `AgentEvent::UsageUpdated` | Coalescible | Yes | Latest |
| `AgentEvent::Activity` | Coalescible | Yes | Latest (already throttled at the source — see below) |
| `PaneCreated` / `PaneClosed` | Lossless | No | Publish order |
| `SessionExited` | Lossless | No | Publish order |
| `WorkspaceChanged` | Lossless | No | Publish order |
| `TaskEvent` (general) | Lossless | No | Publish order |
| `TaskEvent::TaskArtifactCreated` | Coalescible | Yes | Latest |
| `PlannerEvent` | Lossless | No | Publish order |
| `SynthesisCompleted`, `WorkflowNeedsReplan` | Lossless | No | Publish order |
| `SynthesisStarted`, `ArtifactCreated`, `ReviewFindingCreated`, `ArtifactConsumed` | Coalescible | Yes | Latest |
| `ReplanProposed/Edited/Approved/Rejected`, `PlanSuperseded`, `TaskInvalidated`, `ArtifactInvalidated`, `BudgetRisk`, `HumanEscalation` | Lossless | No | Publish order |
| `ReplanRequested` | Coalescible | Yes | Latest |
| `WorkflowStopped`, `WorkflowPaused` | Lossless | No | Publish order |

`AgentEvent::Output` was the one event type this fix reclassified — from
`Coalescible` (coalesced + droppable) to `Lossless`. Every other row is
unchanged from before ADR 0021.

## Ordering guarantee

Per execution, events are delivered in exactly the order the engine
published them — no reordering, no batching-induced merges. Across
different executions, no ordering is guaranteed (each agent's pump thread
publishes independently); this matches what a consumer can actually rely
on regardless of implementation, since separate agents genuinely run
concurrently.

## Backpressure and overflow

`EventBus`/`publish`/`flush` never block the engine thread — this is a
hard invariant carried over from Phase 2B.1 and preserved by this fix. A
subscriber that cannot keep up is disconnected, not backpressured or
spooled:

- **Lossless** events that fail to enqueue (queue full) count toward
  `critical_drops`; 8 consecutive failures disconnects the subscriber.
- **Coalescible** events that fail to enqueue count toward
  `dropped_non_critical`; 64 disconnects the subscriber.
- A subscriber whose queue depth holds steady across a flush *and*
  received nothing new in that window is "stalled"; 400 consecutive
  stalled flushes disconnects it (the backstop for a receiver that never
  reads at all, even below capacity — e.g. an IPC writer blocked on a full
  socket).

**What this does not guarantee**: a subscriber right at the disconnect
boundary can still lose up to 8 individual lossless events in the failed
attempts before disconnect fires. This is bounded, counted (via
`EventBus::subscriber_stats()`), and deterministic — not the unbounded,
silent, per-frame loss the coalescing bug caused. True zero-loss delivery
under all conditions would require blocking the engine (rejected — the
whole point of this architecture is that a slow client can't do that) or
spooling to disk (rejected as disproportionate; see ADR 0021).

`AgentEvent::Activity` is throttled *at the source*, not by `EventBus`:
`crates/terminal-session/src/agent.rs::process_chunk` emits at most one
`Activity` event per `work::ACTIVITY_COALESCE_MS` window per execution.
This is unaffected by this fix — it was never part of `EventBus`'s own
coalescing (which only ever applied to `Output`).

## Redaction boundary

`EventBus` never touches event payload content. Text is redacted upstream,
at the pump (`Redactor::redact` in `process_chunk`), before it ever
becomes an `AgentEvent::Output`. This fix doesn't move or weaken that
boundary — verified by `tests/ipc_stream.rs::sentinel_secret_never_reaches_events_or_persistence`
and the redaction check inside `agent_events_stream_live_to_subscriber`.

## IPC and the CLI watcher

Both are pure `ApplicationEvent` pass-throughs:

- `crates/terminal-workspace/src/ipc.rs`'s `Event::Application` boxes the
  raw `ApplicationEvent` straight onto the wire.
- `apps/cli/src/main.rs`'s `agent_watch` renders whatever
  `ApplicationEvent`s arrive over that same IPC stream.

Neither needed a code change for this fix — fixing `EventBus` fixes both
transparently. `tests/ipc_stream.rs::agent_output_stream_is_lossless_end_to_end`
is the end-to-end proof: a real fake-agent process emitting 1,000
distinguishable lines through the actual PTY → pump → EventBus → Unix
socket path, asserting every line reaches the client in order.

## Event replay

`crates/terminal-session/src/work.rs::replay_into` (deterministic replay
of `AgentEvent` fixtures into `AgentWork`/timeline state) is a separate
mechanism from `EventBus` and is untouched by this fix.

## Performance

Removing `Output` coalescing is a measured, proportional cost — each
distinct output chunk is now an individual send per subscriber instead of
being squashed into the latest one. On development hardware, 50,000
output events across 10 subscribers:

| | Coalesced (before) | Lossless (after) |
|---|---|---|
| Elapsed | 29.6ms | 40.5ms |
| Throughput | ~1.7M events/sec | ~1.2M events/sec |

Both are far beyond any realistic agent output rate (tens to low hundreds
of lines/sec). `tests/eventbus.rs::high_volume_multi_subscriber_throughput_stays_bounded`
guards against a throughput regression beyond a generous ceiling. This
change doesn't touch `terminal-core`/`terminal-parser`/`terminal-renderer`,
so the Phase 4 tracked benchmarks in `benchmarks/baseline.json` (input
latency, render prep, parse throughput) are unaffected.

## Test coverage

- `crates/terminal-workspace/src/events.rs` unit tests: filtering,
  lossless-and-ordered output, slow/stalled-subscriber disconnect,
  drained-subscriber-never-disconnected, unsubscribe.
- `crates/terminal-workspace/tests/eventbus.rs`: the dedicated regression
  suite for this fix — losslessness at 10/100/1,000/10,000 events, strict
  per-execution ordering, multi-agent interleaving, slow/fast subscriber
  isolation, coalescible-still-drops, bounded overflow, sustained-volume
  throughput.
- `crates/terminal-workspace/tests/ipc_stream.rs`: real end-to-end IPC
  coverage (`agent_output_stream_is_lossless_end_to_end`,
  `sentinel_secret_never_reaches_events_or_persistence`,
  `slow_ipc_client_is_disconnected_and_engine_keeps_running`).
