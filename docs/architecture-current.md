# Current Architecture (Phase 1 + Phase 2 / 2B.1)

This document describes the FlashTerminal architecture as implemented at the
end of Phase 1, updated with the Phase 2 (multi-agent infrastructure) and
Phase 2B.1 (real-agent validation + concurrency + desktop agent UX + event
streaming) additions. Phase 1 sections are marked; the agent layer is
`## AI Agents (Phase 2 / 2B.1)`.
> **Interactive diagram:** open [`docs/diagrams/flashterminal-architecture.html`](diagrams/flashterminal-architecture.html) (source: `docs/diagrams/flashterminal.architecture.json`) for a navigable map of the architecture.

## Crate Responsibilities

| Crate | Responsibility |
|-------|----------------|
| **`terminal-core`** | Owns the terminal state model: packed 16-byte `Cell`, `Row`, `Cursor`, `Color`, `Attribute`, Unicode/wide/ZWJ handling, deferred wrap, alt screen, and the dirty-tracking bitset. **Tiered hot/cold scrollback** — a bounded `VecDeque<Row>` hot tier plus a cold tier of 128-row RLE+flate2 compressed blocks (`ColdStore`, `crates/terminal-core/src/scrollback.rs`), decode-on-demand viewport (ADR-0004). Memory is **flat (~3.4 MB/pane) from 10 k to 1 M history rows**. Exposes `RenderSnapshot` — an immutable, cell-reader view of the grid. |
| **`terminal-parser`** | Stateless byte→`TerminalEvent` transducer built on `vte`. No state of its own. |
| **`pty`** | Wraps `portable-pty` 0.8: spawns sessions, blocking `read_available`, non-blocking writes, resize, terminate. **Amortised O(1) pending-write FIFO** — 17.3 MB/s linear throughput. |
| **`terminal-session`** | Ownership hub between PTY and UI. `Session` spawns the reader/parser thread and forwards batches over a bounded channel (cap 1024) for backpressure. `spawn_with_wake` fires an `EventLoopProxy` callback when batches arrive. **Phase 2: the agent runtime** — `AgentRuntime` (spawn/lifecycle/pump/events), `AgentRegistry` + adapters (`claude-code`, `codex`, `opencode`, `pi`, generic, fake), provider registry + model catalog, keychain `CredentialStore` (BYOK), `Redactor`, agent state machine with provenance-aware snapshots. |
| **`terminal-text`** | Font discovery + glyph rasterization via `fontdue`, LRU `GlyphCache` keyed by (font, glyph, size). |
| **`terminal-renderer`** | `wgpu` renderer: shared glyph atlas, instanced text, dirty-row updates, cursor/selection, chrome (sidebar/tab strip/focus borders) through the same atlas, and **`render_multi`** — renders N pane viewports in ONE frame (§10–11, §28). Consumes immutable `RenderSnapshot`s only. |
| **`terminal-workspace`** | **Phase 1 — the multiplexer + workspace engine** (UI-agnostic). Owns `Workspace`s → `Tab`s → binary pane split trees (pure data), and the live `Session`s/`TerminalState`s keyed by `SessionId` (`Multiplexer`). Provides the layout engine, command registry, versioned persistence + restore, notification center, and the IPC protocol (Request/Response/Event over a Unix socket). **Phase 2/2B: agent pane integration** — `split_pane_agent` (redacted metadata), agent lifecycle + permission surface on `Multiplexer`, unified `ApplicationEvent` bus with bounded subscriber queues / coalescing / drop / slow-client-disconnect policies, and the `subscribe`/`unsubscribe` event-stream IPC. |
| **`crates/fake-agent`** | Deterministic agent executable (Phase 2B): `startup/working/streaming/waiting/approval/completion/failure/crash/large-output/long-running` scenarios for tests and stress harnesses. |
| **`apps/desktop`** | `winit` 0.29 main binary. Owns the `Multiplexer` (behind a mutex shared with the IPC server), drains once per frame, renders all panes through the shared renderer, and draws the sidebar/tab-strip chrome. **Phase 2B: agent UX** — agent pane header (state badge, capability-gated Stop/Restart/Resume, permission Allow/Deny bar, completion/failure indicators), sidebar agent list + info panel. |
| **`apps/cli`** | `terminal` binary: `workspace list|create|open|rename|close`, `tab create|close`, `pane split|close|focus|list`, `terminal serve` (headless control surface), **Phase 2: `agent list|spawn|spawn-pane|status|stop|restart|resume|pause|permission|watch`** (live event-stream subscription). |
| **`benchmarks`** | Validation & benchmark suite: `validate`, `scrollback_bench`, `raw_throughput`, `paste_bench`, `plateau`, `soak`, `alloc_profiler`, and Phase 1 `multiplex_bench` (creation latencies, 1–50 pane scaling, 20-pane stress with focused-pane input latency, state-batching metrics). **Phase 2B: `agent_stress`** (10-agent concurrency + interactive panes, 5-heavy starvation, memory scaling 1/5/10/20 × 4 workloads, event throughput, high-output 1/5/10 stability + tail integrity). |

## Thread Ownership & Data Flow

Message-passing with a single owner (ADR-0002/0003):

```text
per session:  shell ──► PTY master ──► [reader + parser thread] ──► bounded channel (1024)
                                                                              │
UI thread (owns Multiplexer):                                         drain_frame()
  ├─ fairness caps: focused uncapped, visible 4096/frame, background 512/frame
  ├─ batch-apply events per session → one dirty region set per frame
  ├─ LayoutEngine → pane rectangles (single pass, zoom-aware)
  └─ Multiplexer::pane_frames → Vec<PaneFrame> (snapshot + consumed dirty + origin)
                                                                    │
                                                                    ▼
Renderer.render_multi(&viewports)  ── ONE GPU frame, shared atlas, one present

IPC server thread(s):  Unix socket → lock engine → handle(Request) → respond
CLI:                   terminal … → roundtrip over the socket
```

1. **Reader/parser threads** (one per session): block on `read_available`,
   parse into `TerminalEvent` batches, send over the bounded channel. If the
   UI falls behind, the reader blocks — backpressure, bounded memory.
2. **UI thread**: owns the authoritative `Multiplexer`. Per frame:
   `drain_frame()` (fairness + batch application), layout, resize pane
   grids via fast ioctls, `pane_frames()` (snapshot + consume dirty in one
   borrow), then `render_multi`.
3. **Renderer**: consumes `PaneFrame`s only; never touches engine state.
   The `FrameCtx` (ascent/cursor-style/blink) is per-frame shared state.
4. **Input routing** (§9): window → active workspace → active tab → focused
   pane → `Session::write` → PTY. App commands (split/focus/resize/tab/
   workspace) resolve through the `CommandRegistry` first.

### The snapshot boundary

`RenderSnapshot` is the contract between state and GPU: a cheap borrow-based
view (`visible_cell(row, col)`). Phase 1 extends the boundary with
`PaneFrame` (snapshot + consumed `DirtyTracker` + origin) so the desktop can
render all panes without re-borrowing engine state. The renderer can read
snapshots freely but can never mutate state.

## Workspace domain model (Phase 1)

```text
PersistedState { version, workspaces[], active_workspace }
Workspace     { id, name, project_root, tabs[], active_tab, metadata }
Tab           { id, workspace_id, root: PaneNode, title, active_pane, metadata }
PaneNode      = Leaf(Pane) | Split { direction, ratio, [child; 2] }   (binary tree)
Pane          { id, pane_type: Terminal, session_id, title, cwd, metadata }
SessionId     → live Session (PTY + parser + state) owned by the Multiplexer
```

Rules (§3): workspaces own tabs, tabs own pane trees, panes *reference*
sessions, the multiplexer owns live sessions, the renderer renders snapshots.
The pane tree is pure data and fully serializable.

## AI Agents (Phase 2 / 2B.1)

```text
agent process ──► PTY master ──► [reader + parser thread]  (same pipeline as shells)
                                        │ raw-output tap
                                        ▼
                          activity pump (per agent)
                                        │ redacted semantic events (Started /
                                        │ StateChanged+provenance / Output /
                                        │ PermissionRequested / Exited)
                                        ▼
                        AgentRuntime.event_tx (bounded) ──► ApplicationEvent bus
                                        │                      │ bounded subscriber
                                        │                      │ queues + slow-client
                                        ▼                      ▼ policy
                       terminal_states (same drain path)   desktop chrome, IPC,
                                                            notifications
```

- **Identity**: `ExecutionId` / `ExecutionKind` unify terminals and agents;
  agent panes reference an execution, never a session type. `Pane.metadata`
  carries a redacted `agent` launch record (definition, provider, model,
  credential **reference**, cwd, args) for persistence + restore.
- **`AgentRuntime`** (`terminal-session`) owns spawn/lifecycle/stop/restart/
  resume/permission-response, the per-session activity pump, and the bounded
  semantic-event channel. Adapters (`claude-code`, `codex`, `opencode`,
  `pi`, generic-CLI, fake) declare honest `AgentCapabilities`; the desktop
  only surfaces capability-gated controls.
- **Fairness**: agent output is drained through the same per-frame caps
  (focused uncapped / 4096 / 512) as terminal sessions — agent floods cannot
  starve interactive panes (2B.1 starvation test: focused-input p95 < 8 ms
  under 5 heavy agents).
- **Event streaming (2B.1 §24–27)**: `subscribe`/`unsubscribe` over the IPC
  Unix socket with per-channel filters; per-subscriber bounded queues,
  coalescing, structured drops, stall-detection disconnect, and a socket
  write timeout — a slow client can never block the engine.
- **Security boundary**: keys live only in the OS keychain; everything else
  holds `keychain://flashterminal/<provider>` references; `Redactor` masks
  registered secrets + known shapes at output/errors/IPC/persistence;
  `AgentLaunchConfig::redact()` runs before any storage point. Verified by
  sentinel + persistence tests (see `docs/security-secrets.md`).
- **Desktop agent UX (2B.1 §15–23)**: agent pane chrome (state badge,
  capability-gated Stop/Restart/Resume, permission Allow/Deny bar,
  completion/failure indicators), sidebar agent list + info panel; raw
  output is always the pane itself.
- **Validation**: `crates/fake-agent` (deterministic scenarios),
  `benchmarks/src/bin/agent_stress.rs` (§2–6 harness),
  `real_agents` feature suite (§7–11, SKIP-when-unavailable), IPC/persistence
  integration tests. Results: `docs/agent-compatibility.md`, `docs/phase2b.md`.

## Verified Budgets (Phase 0.5 + Phase 1)

Measured by the validation harness, scrollback suite, and `multiplex_bench`
(see `docs/performance.md`, `docs/performance-phase-0.5.1.md`,
`docs/scrollback.md`, `docs/phase1-multiplexer.md`):

| Metric | Budget | Measured | Status |
|--------|--------|----------|--------|
| Idle RAM (1 live session) | < 40 MB | ~9 MB app (19.8 MB tree w/ shell) | ✅ |
| 10 panes RAM | < 80 MB | 50.5 MB tree | ✅ |
| 20 panes RAM | < 120 MB | 85.0 MB tree | ✅ |
| Input latency p95 (focused, 20-pane stress) | < 8 ms | **0.71 ms** (p99 1.41, max 7.7) | ✅ |
| New workspace | < 100 ms | 2.6 ms | ✅ |
| Pane split | < 30 ms | 2.8 ms | ✅ |
| Focus switch | < 10 ms | 0.6 µs | ✅ |
| Layout (50 panes) | < 5 ms | 1.8 µs | ✅ |
| Scrollback state memory | plateau | 3.43 MB flat, 10 k→1 M rows | ✅ |
| Raw PTY throughput | — | 17.3 MB/s (100 MB, linear) | ✅ |
| State batching (20-pane stress) | — | 1195 events/s, apply-latency p95 509 µs | ✅ |
| Pane scaling 1→50 | linear | 19.8 → 187.1 MB tree (2.02 MB state) | ✅ |

## Benchmark Flow

- **Report generator** (`benchmarks/src/main.rs`): full pipeline + RAM with
  real sessions; writes `docs/performance-report.md` + `baseline.json`.
- **Validation harness** (`validate.rs`): PTY backpressure, end-to-end
  latency, memory breakdown, stress A–E, input priority, coalescing, atlas.
- **Scrollback suite**: `scrollback_bench`, `raw_throughput`, `paste_bench`,
  `plateau`, `soak`, `alloc_profiler`.
- **Phase 1 multiplexer suite** (`multiplex_bench.rs`): creation latencies,
  1/5/10/20/50-pane scaling, 20-pane mixed stress (5 idle/5 moderate/5 heavy/
  5 interactive) with focused-pane input latency and fairness (§25–27).
- **Criterion microbenches** (`benches/`): isolated operations.
- **Integration tests** (`crates/terminal-session/tests/`): PTY→parser→state
  round-trip, massive-output backpressure, resize propagation, Phase 0.5.1
  manual-gate coverage.
