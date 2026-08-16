# FlashTerminal EventBus Output-Loss Fix

## Objective

Fix the confirmed orchestration bug where:

```text
AgentEvent::Output
```

can be silently overwritten by EventBus coalescing before subscribers receive it.

This is a correctness bug.

Do NOT begin Phase 5.

Do NOT add new product functionality.

Do NOT redesign the orchestration architecture broadly.

Make the smallest principled architectural change required to guarantee that semantic agent output is not silently lost.

---

# 1. Confirm the Existing Failure

Before modifying code:

Inspect:

```text
terminal-workspace/tests/phase3d/main.rs
EventBus
ApplicationEvent
AgentEvent
event coalescing implementation
subscriber queues
flush logic
```

Reproduce the reported failure deterministically.

Create a test that emits:

```text
Output("line 1")
Output("line 2")
Output("line 3")
Output("line 4")
```

faster than the current flush interval.

Verify that the subscriber currently receives fewer than four outputs.

This test must fail against the current implementation.

---

# 2. Establish Event Semantics

Before coding, explicitly classify events into two categories.

## Category A: Lossless semantic events

These MUST NOT be silently dropped or overwritten.

Examples:

```text
AgentEvent::Output
AgentEvent::Error
PermissionRequested
ArtifactCreated
TaskCompleted
TaskFailed
ApprovalRequested
WorkflowCompleted
WorkflowFailed
```

The exact list should be derived from the actual event model.

## Category B: Coalescible state updates

These MAY be replaced by a newer state.

Examples:

```text
ActivityChanged
ProgressUpdated
StateChanged
Heartbeat
StatusSnapshot
```

The principle is:

> A newer state may replace an older state. A historical event may not be silently deleted.

Document this distinction.

Create or update an ADR:

```text
docs/adr/0021-event-delivery-semantics.md
```

---

# 3. Do NOT Simply Disable Coalescing

Do NOT solve the problem by removing all EventBus coalescing.

That could create:

```text
10 agents
+
thousands of events/sec
=
unbounded UI/event pressure
```

The existing performance architecture is valuable.

Preserve coalescing for state-like updates.

Make semantic output lossless.

---

# 4. Recommended EventBus Architecture

Use separate treatment for:

```text
Lossless event queue
```

and:

```text
Coalesced state queue
```

Conceptually:

```text
                         EventBus
                            │
              ┌─────────────┴─────────────┐
              │                           │
              ▼                           ▼
        Lossless Queue               State Store
              │                           │
       every event preserved        latest state wins
              │                           │
              └─────────────┬─────────────┘
                            ▼
                       Subscriber
```

The implementation does not have to literally use two physical queues if another design is cleaner.

The semantic contract must remain equivalent.

---

# 5. Output Must Preserve Ordering

For a single execution:

```text
Output("A")
Output("B")
Output("C")
```

must be delivered in exactly:

```text
A
B
C
```

order.

Do not reorder output because of batching.

Across different executions:

```text
Agent A
Agent B
```

global ordering may remain event-loop dependent.

The important invariant is:

> Per execution, semantic output order is preserved.

---

# 6. Sequence Numbers

Consider adding a monotonically increasing sequence number to lossless agent events.

Conceptually:

```rust
AgentEventEnvelope {
    execution_id,
    sequence,
    event,
    timestamp,
}
```

For each execution:

```text
1
2
3
4
5
```

This enables:

* ordering verification
* gap detection
* diagnostics
* replay
* subscriber recovery

Do not add sequence numbers if the existing architecture already has an equivalent mechanism.

---

# 7. Subscriber Backpressure

The existing EventBus uses bounded subscriber queues.

Preserve bounded memory.

Define what happens when a subscriber is too slow.

For LOSSLESS events, do not silently drop.

Possible behavior:

```text
subscriber queue full
        ↓
temporary producer backpressure
```

or:

```text
subscriber queue full
        ↓
persist/spool lossless events
        ↓
subscriber catches up
```

or:

```text
subscriber queue full
        ↓
disconnect subscriber
        ↓
emit explicit overflow/error
```

Choose the simplest architecture consistent with the current system.

The critical rule is:

> Never silently discard semantic output.

---

# 8. Separate UI Coalescing From EventBus Semantics

The terminal UI does NOT necessarily need to rerender on every output event.

This is important.

The system should allow:

```text
1000 Output events
        ↓
EventBus preserves all 1000
        ↓
UI creates one or a few render updates
```

So:

```text
event delivery
```

and:

```text
render scheduling
```

must remain separate concerns.

Do not solve UI performance by deleting events.

---

# 9. Add Lossless Output Test

Create a permanent test:

```text
output_events_are_lossless
```

Emit:

```text
10
100
1,000
10,000
```

output events.

Verify:

```text
received == emitted
```

for every execution.

---

# 10. Ordering Test

Create:

```text
output_order_is_preserved_per_execution
```

Emit:

```text
1..10000
```

as output payloads.

Verify the subscriber receives:

```text
1..10000
```

with no:

```text
missing
duplicate
reordered
```

events.

---

# 11. Multi-Agent Test

Run:

```text
Agent A:
A1 A2 A3 ... A1000

Agent B:
B1 B2 B3 ... B1000

Agent C:
C1 C2 C3 ... C1000
```

Verify:

```text
A sequence is lossless
B sequence is lossless
C sequence is lossless
```

Do not require a deterministic global ordering across agents.

---

# 12. Slow Subscriber Test

Create a deliberately slow subscriber.

For example:

```text
subscriber sleeps 100ms between reads
```

while:

```text
agent emits output rapidly
```

Verify the system follows the chosen backpressure policy.

It must NOT:

```text
silently drop output
```

The resulting behavior must be observable and documented.

---

# 13. Fast Subscriber Test

Confirm that normal subscribers do not suffer unnecessary latency because of slow subscribers.

One slow subscriber must not block:

```text
desktop UI
CLI watcher
other subscribers
workflow engine
```

unless the architecture explicitly chooses global backpressure.

Prefer subscriber isolation.

---

# 14. Event Stream Replay

The existing replay functionality must continue to work.

Record:

```text
Output 1
Output 2
...
Output N
```

Replay them.

Verify the same sequence is observed.

---

# 15. IPC Event Streaming

The existing IPC event streaming must preserve the same semantics.

Verify:

```text
AgentRuntime
 ↓
EventBus
 ↓
IPC subscriber
```

does not lose Output events.

Add an IPC integration test emitting:

```text
1000 output events
```

and verify the client receives all 1000.

---

# 16. CLI Watcher

Verify:

```bash
terminal agent watch
```

receives all lossless events.

A CLI watcher that misses output silently is unacceptable.

---

# 17. Coalesced Activity Test

Make sure state coalescing still works.

Example:

```text
Working
Working
Working
Reading
Reading
Testing
Testing
```

may become:

```text
Working
Reading
Testing
```

provided the semantics permit it.

This test should demonstrate that state coalescing survived the fix.

---

# 18. Performance Test

Benchmark:

```text
100 events/sec
1,000 events/sec
10,000 events/sec
50,000 events/sec
```

with:

```text
1 subscriber
5 subscribers
10 subscribers
```

Record:

```text
event throughput
queue depth
CPU
RSS
latency
```

Ensure the lossless design does not introduce uncontrolled memory growth.

---

# 19. Memory Safety

Run:

```text
10 agents
1000 events/sec each
```

for at least:

```text
5 minutes
```

Then:

```text
30 minutes
```

where feasible.

Observe:

```text
RSS
queue size
subscriber queue size
timeline size
event storage
```

Memory must remain bounded according to the documented backpressure policy.

---

# 20. Overflow Semantics

If a lossless queue reaches its limit, make the behavior explicit.

Possible outcomes:

```text
BLOCK
SPOOL
DISCONNECT
FAIL WORKFLOW
```

Select one per subscriber/event category.

Do not leave this implicit.

Document:

```text
docs/agent-events.md
```

---

# 21. Event Classification API

Create a clear mechanism to classify event delivery semantics.

Conceptually:

```rust
enum DeliverySemantics {
    Lossless,
    Coalescible,
}
```

or an equivalent architecture.

Do not scatter:

```text
if event == Output
```

throughout the EventBus.

The classification should live near the event definition.

---

# 22. Audit Existing Event Types

Review every current:

```text
AgentEvent
ApplicationEvent
WorkflowEvent
ReplanEvent
ArtifactEvent
NotificationEvent
```

Assign each an explicit delivery semantic.

Produce a table:

| Event               | Semantics   | Can Drop? | Ordering      |
| ------------------- | ----------- | --------- | ------------- |
| Output              | Lossless    | No        | Per execution |
| Error               | Lossless    | No        | Per execution |
| PermissionRequested | Lossless    | No        | Ordered       |
| Completed           | Lossless    | No        | Ordered       |
| StateChanged        | Coalescible | Yes       | Latest        |
| ActivityChanged     | Coalescible | Yes       | Latest        |
| Progress            | Coalescible | Yes       | Latest        |

Adjust based on actual semantics.

---

# 23. Security

Ensure the new event delivery path does not bypass:

```text
secret redaction
```

All output must continue through the existing redaction boundary.

Add a regression test where:

```text
Output("SENTINEL_SECRET")
```

is emitted.

Verify subscribers receive only the redacted representation where required.

---

# 24. Recovery

If a subscriber disconnects and reconnects, determine whether it:

```text
resumes from sequence N
```

or:

```text
starts from a new snapshot
```

The behavior must be explicit.

You do NOT need to build durable event replay unless necessary.

Document the chosen semantics.

---

# 25. Do Not Modify Planner Logic

This task is specifically about:

```text
EventBus
AgentEvent
ApplicationEvent
IPC event delivery
```

Do NOT change:

```text
planning
adaptive orchestration
task scheduling
agent selection
worktrees
policy engine
autonomy
```

unless required to integrate the event semantics.

---

# 26. Backward Compatibility

The following must continue to work:

```text
terminal agent watch
workflow history
agent dashboard
timeline
notifications
audit trail
IPC streaming
event replay
```

---

# 27. Regression Suite

Run:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --release --workspace
```

All existing Phase 0–4 tests must remain green.

---

# 28. Create a Dedicated Phase Fix Suite

Add:

```text
tests/eventbus/
```

or an equivalent dedicated test target.

Minimum tests:

```text
output_events_are_lossless
output_order_preserved
multi_agent_output_lossless
slow_subscriber_behavior
fast_subscriber_isolation
ipc_output_lossless
cli_watch_output_lossless
coalesced_state_still_coalesces
secret_redaction_preserved
event_replay_preserved
overflow_behavior
```

---

# 29. Performance Gate

The fix must not materially regress:

```text
terminal input p95
terminal render latency
agent throughput
multiplexer throughput
```

Compare against the Phase 4 baseline.

---

# 30. Documentation

Update:

```text
docs/agent-events.md
docs/agent-runtime.md
docs/architecture-current.md
docs/ci-forensics.md
```

Create:

```text
docs/adr/0021-event-delivery-semantics.md
```

Document:

* lossless events
* coalescible events
* ordering
* backpressure
* subscriber overflow
* IPC semantics
* replay behavior

---

# 31. Definition of Done

This bug fix is complete when:

```text
✓ confirmed reproduction exists
✓ failing regression test exists
✓ root cause documented
✓ lossless semantic events implemented
✓ output ordering preserved
✓ multi-agent output preserved
✓ slow subscriber behavior explicit
✓ fast subscribers isolated
✓ IPC preserves output
✓ CLI watcher preserves output
✓ state coalescing still works
✓ memory remains bounded
✓ secret redaction preserved
✓ replay preserved
✓ overflow semantics documented
✓ all workspace tests pass
✓ clippy clean
✓ fmt clean
✓ release build clean
✓ performance regression checked
```

---

# 32. Final Report

Return:

## Root cause

Exactly why Output was being overwritten.

## Design

Explain lossless vs coalescible semantics.

## Backpressure

Explain what happens when subscribers are slow.

## Tests

Report:

```text
events emitted
events received
dropped events
duplicates
ordering errors
```

## Performance

Compare before and after.

## Compatibility

Confirm:

```text
desktop
CLI
IPC
workflow
timeline
notifications
replay
```

## Decision

Return exactly:

```text
EVENTBUS OUTPUT DELIVERY FIX VERIFIED
```

Do not begin Phase 5 automatically.

This is a correctness fix, not a new feature.
