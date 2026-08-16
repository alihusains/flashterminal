# Agent Runtime Architecture

Status: mirrors the **Phase 2B** implementation (validated since 2B.1).

The agent runtime provides a provider-neutral foundation for hosting AI
agents inside FlashTerminal panes: any agent that speaks a TTY can be
hosted, observed, and controlled through the same pipeline as a shell —
without coupling the workspace engine to any specific vendor.

## Pipeline

```text
agent process ──► PTY master ──► [reader + parser thread]
                                        │  raw-output tap (activity detection)
                                        ▼
                             pump thread (per agent)
                                        │  redacted semantic events
                                        ▼
                        AgentRuntime.event_tx (bounded)
                                        │
                                        ▼
                          ApplicationEvent bus ──► desktop / IPC subscribers
                                        │
                                        ▼
                         terminal_states (same drain path as shells)
```

Every agent pane's PTY stream flows through the *same* fairness-aware
drained pipeline as terminal sessions (`VISIBLE_DRAIN_CAP` /
`BACKGROUND_DRAIN_CAP`), so agents share one batching, backpressure, and
coalescing mechanism. Agent activity can never bypass terminal fairness
(`docs/performance.md`).

### Pump discipline (deadlock-free by construction)

The activity pump threads are the only producers of the bounded
`event_tx` channel, and the UI thread is the only consumer. Two rules
keep that producer/consumer pair deadlock-free (enforced since 2B.1):

1. **No session lock is ever held across a channel send.** All
   `event_tx.send` calls in the pump run *outside* the
   `Mutex<AgentSession>` scope — the lock covers only state mutation
   (microseconds). A pump blocked on a full channel therefore holds no
   lock the drain path needs.
2. **The drain path takes no session locks.** `drain_events` updates
   per-session counters through `metrics_by_eid` (lock-free `Arc` +
   atomics), never through `sessions[..].lock()`.

Violating either rule deadlocks under flood load (pump holds the session
lock while blocked on the full channel; the main thread blocks on that
same lock and can no longer drain). Found and fixed by
`benchmarks/src/bin/agent_stress.rs` §4 (§2–6 gate: "no PTY deadlock").
`stop()` uses `try_send` under the lock only — deliberately
non-blocking.

## Core components

| Component | Responsibility |
|-----------|----------------|
| `ExecutionId` / `ExecutionKind` | Stable identity for every execution (terminal or agent); panes reference one, never the session type itself. |
| `AgentDefinition` | Declarative description (id, display name, binary, default args, install hint). No keys. |
| `AgentLaunchConfig` | Persistable launch description: definition, cwd, arguments, `provider_id`, `model_id`, `credential_ref` (URI reference only), `resume_id`, environment. `redact()` strips/masks secret-shaped values before *any* persistence boundary. |
| `AgentSession` | One live session record: state, provenance, timestamps, exit code, launch, metrics. |
| `AgentAdapter` trait | Spawn / write / resize / stop / restart / resume / permission-response contracts. Capabilities are *honest*: nothing is claimed until verified. |
| Adapters | `ClaudeCodeAdapter`, `CodexAdapter`, `OpenCodeAdapter`, `PiAdapter`, `GenericCliAdapter` (fallback for custom definitions), `FakeAgentAdapter` (deterministic test fixture). |
| `AgentRegistry` | Definition lookup + adapter resolution; unknown ids fall back to the generic CLI adapter. |
| `AgentRuntime` | Orchestrates spawn/lifecycle of sessions; owns the bounded event channel, PTY registry, and activity pump threads; on drop it terminates every running process. |
| `ProviderRegistry` | Provider definitions (endpoints, credential env var, header policy) and a model catalog. |
| `CredentialStore` | OS keychain (or in-memory backend for tests/headless). Stores *values* only in the OS keychain; everything else holds references. |
| `Redactor` | Process-wide secret registry + shape-based masking (`sk-`, `ghp_`, …). Applied at output, errors, IPC frames, and persistence boundaries. |

## Lifecycle states

`Created → Starting → Working/Waiting/NeedsApproval ⇄ … → Completed |
Failed | Crashed | Stopped`

- State transitions are emitted as `AgentEvent::StateChanged` with a
  **provenance** (`state_source` + `state_confidence`), §14 of 2B.1:
  e.g. `Working` observed from terminal heuristics carries
  `confidence = medium`, whereas deterministic fixture output carries
  `high`. The primary UI may show only the state; the confidence exists
  for later orchestration decisions.
- Activity detection today is heuristic (output-pattern refinement) with
  output remaining authoritative (`crates/terminal-session/src/agent.rs`).
  Native/structured sources are not yet consumed — see
  `docs/agent-compatibility.md` for the per-agent audit.

## Security boundary

- `AgentRuntime` is the only path that touches agent processes: permission
  decisions are normalized by the runtime and translated by the adapter —
  the desktop/CLI never write to a process directly.
- Credentials are resolved from the keychain at spawn, injected into the
  child's environment, registered with the `Redactor`, and exist in
  ephemeral `AgentLaunchContext` only — never persisted, logged, or sent
  over IPC.
- Pane metadata and session snapshots carry `credential_ref` URIs
  (`keychain://flashterminal/<provider>`), never contents; launch configs
  are redacted before storage (§28–§31, `docs/security-secrets.md`).

## Fairness & throughput

- Agent output is delivered losslessly and in order through `EventBus` —
  fixed in ADR 0021 after a confirmed bug where per-execution output
  coalescing silently dropped real output published within the same drain
  frame. State/lifecycle events remain lossless as before; only genuinely
  coalescible metadata (activity heuristics, usage counters) may still be
  dropped under backpressure. See `docs/agent-events.md` for the full
  classification table and `docs/adr/0021-event-delivery-semantics.md`.
  The event bus still uses bounded subscriber queues with a
  drop/disconnect slow-client policy — a stalled or saturated subscriber
  is disconnected, never allowed to block the engine.
- 2B.1 stress evidence (10 agents + 2 interactive panes, starvation
  p95 < 8 ms, memory scaling 1–20 agents): `benchmarks/src/bin/agent_stress.rs`.