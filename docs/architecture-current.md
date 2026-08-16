# Current Architecture (Phase 1 → Phase 3E)

This document describes the FlashTerminal architecture as implemented at the
end of Phase 1, updated with the Phase 2 (multi-agent infrastructure),
Phase 2B.1 (real-agent validation + concurrency + desktop agent UX + event
streaming) and Phase 2C (agent observability: work/activity/timeline/
dashboard/attention/usage/pricing/health/replay/intent/notifications)
additions. Phase 1 sections are marked; the agent layer is
`## AI Agents (Phase 2 / 2B.1 / 2C)` — see also `agent-observability.md` for
the Phase 2C surface.
> **Interactive diagram:** open [`docs/diagrams/flashterminal-architecture.html`](diagrams/flashterminal-architecture.html) (source: `docs/diagrams/flashterminal.architecture.json`) for a navigable map of the architecture.

## Crate Responsibilities

| Crate | Responsibility |
|-------|----------------|
| **`terminal-core`** | Owns the terminal state model: packed 16-byte `Cell`, `Row`, `Cursor`, `Color`, `Attribute`, Unicode/wide/ZWJ handling, deferred wrap, alt screen, and the dirty-tracking bitset. **Tiered hot/cold scrollback** — a bounded `VecDeque<Row>` hot tier plus a cold tier of 128-row RLE+flate2 compressed blocks (`ColdStore`, `crates/terminal-core/src/scrollback.rs`), decode-on-demand viewport (ADR-0004). Memory is **flat (~3.4 MB/pane) from 10 k to 1 M history rows**. Exposes `RenderSnapshot` — an immutable, cell-reader view of the grid. |
| **`terminal-parser`** | Stateless byte→`TerminalEvent` transducer built on `vte`. No state of its own. |
| **`pty`** | Wraps `portable-pty` 0.8: spawns sessions, blocking `read_available`, non-blocking writes, resize, terminate. **Amortised O(1) pending-write FIFO** — 17.3 MB/s linear throughput. |
| **`terminal-session`** | Ownership hub between PTY and UI. `Session` spawns the reader/parser thread and forwards batches over a bounded channel (cap 1024) for backpressure. `spawn_with_wake` fires an `EventLoopProxy` callback when batches arrive. **Phase 2: the agent runtime** — `AgentRuntime` (spawn/lifecycle/pump/events), `AgentRegistry` + adapters (`claude-code`, `codex`, `opencode`, `pi`, generic, fake), provider registry + model catalog, keychain `CredentialStore` (BYOK), `Redactor`, agent state machine with provenance-aware snapshots. **Phase 2C: observability models** — `AgentWork`/Activity/Timeline/Summary/Attention, `AgentUsage` + `PricingRegistry`, secret-free health, replay fixtures, intent resolution (`work.rs`). |
| **`terminal-text`** | Font discovery + glyph rasterization via `fontdue`, LRU `GlyphCache` keyed by (font, glyph, size). |
| **`terminal-renderer`** | `wgpu` renderer: shared glyph atlas, instanced text, dirty-row updates, cursor/selection, chrome (sidebar/tab strip/focus borders) through the same atlas, and **`render_multi`** — renders N pane viewports in ONE frame (§10–11, §28). Consumes immutable `RenderSnapshot`s only. |
| **`terminal-workspace`** | **Phase 1 — the multiplexer + workspace engine** (UI-agnostic). Owns `Workspace`s → `Tab`s → binary pane split trees (pure data), and the live `Session`s/`TerminalState`s keyed by `SessionId` (`Multiplexer`). Provides the layout engine, command registry, versioned persistence + restore, notification center, and the IPC protocol (Request/Response/Event over a Unix socket). **Phase 2/2B: agent pane integration** — `split_pane_agent` (redacted metadata), agent lifecycle + permission surface on `Multiplexer`, unified `ApplicationEvent` bus with bounded subscriber queues / coalescing / drop / slow-client-disconnect policies, and the `subscribe`/`unsubscribe` event-stream IPC. **Phase 2C: agent observability surface** — `agent_dashboard`/`workspace_agent_summary`/`agent_review` (bounded diffs), quiet-mode `NotificationPrefs` (persisted + redacted), `CommandRegistry::palette()`, intent-aware IPC requests (`agents`, `work`, `review`, `health`). |
| **`crates/fake-agent`** | Deterministic agent executable (Phase 2B): `startup/working/streaming/waiting/approval/completion/failure/crash/large-output/long-running` scenarios for tests and stress harnesses. |
| **`apps/desktop`** | `winit` 0.29 main binary. Owns the `Multiplexer` (behind a mutex shared with the IPC server), drains once per frame, renders all panes through the shared renderer, and draws the sidebar/tab-strip chrome. **Phase 2B: agent UX** — agent pane header (state badge, capability-gated Stop/Restart/Resume, permission Allow/Deny bar, completion/failure indicators), sidebar agent list + info panel. **Phase 2C: observability UX** — filtered sidebar agent list (dashboard filters), review/work overlay with bounded diffs, agent logs, empty state + provider setup overlays, diagnostics overlay, command palette, quiet-mode toggle; every action routes through one `run_command` dispatch shared with key bindings. |
| **`apps/cli`** | `terminal` binary: `workspace list|create|open|rename|close`, `tab create|close`, `pane split|close|focus|list`, `terminal serve` (headless control surface), **Phase 2: `agent list|spawn|spawn-pane|status|stop|restart|resume|pause|permission|watch`** (live event-stream subscription). **Phase 2C: `agents [filter]`, `agent work|timeline|review|health <id>`** (dashboard + observability over IPC). |
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

## AI Agents (Phase 2 / 2B.1 / 2C)

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
- **Event streaming (2B.1 §24–27; delivery semantics fixed by ADR 0021)**:
  `subscribe`/`unsubscribe` over the IPC Unix socket with per-channel
  filters; per-subscriber bounded queues, explicit lossless-vs-coalescible
  delivery semantics per event type (`docs/agent-events.md`),
  stall-detection disconnect, and a socket write timeout — a slow client
  can never block the engine, and a healthy one never silently loses
  agent output (previously, output was coalesced and droppable — a
  confirmed bug, see `docs/ci-forensics.md`).
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

## Task Orchestration (Phase 3A / 3A.1)

```text
task_run ──► TaskScheduler::step (pure fn of graph + runtime view)
              └─► typed SpawnTask(TaskContext)          (no raw subprocesses)
                    └─► adapter.prepare_task ──► AgentRuntime spawns the agent
runtime exit ──► engine observes in deterministic drain order
                    └─► scheduler classifies (FailureClass) ──► TaskEvent bus
```

- **`TaskScheduler`** (`terminal-session::orchestration`): insertion-ordered
  `TaskGraph`, explicit validated transitions, deterministic topological
  order, capacity arcs (`max_agents`/`max_parallel_tasks` never overshot),
  retry policy (auth never / flaky once), review boundary (`NeedsReview`
  halts the workflow), hard cost budget, `PersistedSchedulerState`
  (versioned, bounded, secret-free; restore → `Interrupted`, never silent
  resume).
- **Engine glue** (`terminal-workspace`): task API on `Multiplexer`
  (create/run/cancel/retry/get/list/set-policy/validate/resolve-review/
  attach-pane/status), `step_tasks` inside `drain_frame` (idle cost
  microseconds), `ApplicationEvent::TaskEvent` on the bus.
- **Determinism**: same graph + same view ⇒ same commands in the same
  order; per-task event order guaranteed; cross-task emission order
  deterministic since 3A.1 (sessions drain in sorted `ExecutionId` order —
  a HashMap-iteration race fixed by the 10-run determinism test).
- **Surfaces**: IPC task request/response set (13 requests) + event
  streaming; CLI `terminal task|workflow|tasks|scheduler`; desktop
  dashboard + detail panel + create form + filters + palette (7+4
  commands, Ctrl-Alt bindings).
- **Validation**: `crates/terminal-workspace/tests/phase3a` (22 tests:
  lifecycle, retry, review, budgets, persistence, determinism ×10, stress,
  idle gate, command-safety table, event flood), `orchestration_bench`
  (40-cell matrix + 80/20 fairness), `real_agent_tasks` (`#[ignore]`d,
  SKIP/record semantics). Docs: `docs/phase3a.md`,
  `docs/orchestration.md`, `docs/phase3a-manual.md`,
  `docs/phase3a-verification.md`.

### Agent observability (Phase 2C)

Backend models live in `terminal-session` (`work.rs`): `AgentWork` (one per
execution id, status/commands/files/errors/usage, idempotent `finish`),
heuristic `ActivityKind` with coalescing windows, bounded `AgentTimeline`
ring, `attention_for(state)` as the single needs-you definition,
`AgentUsage` + `PricingRegistry` (unknown ⇒ no estimate), secret-free
`health()` rows, deterministic fixture `replay_into`. The engine
(`terminal-workspace`) adds `agent_dashboard(filter)` (explicit overlapping
counts: `needs_you` can include `failed`), `workspace_agent_summary()`,
`agent_review()` (bounded diffs), the `NotificationCenter` + quiet-mode
`NotificationPrefs` (persisted, redacted), and `CommandRegistry::palette()`.
Desktop surfaces: filtered sidebar agent list, review/work overlay with
bounded diffs, empty state + provider setup overlays, diagnostics overlay,
palette dispatch, quiet-mode toggle, and dashboard-filter key bindings —
all routed through one `run_command` dispatch. CLI adds
`terminal agents [filter]` and `terminal agent work|timeline|review|health
<id>`. Full surface: `docs/agent-observability.md`; verification truth
table: `docs/phase2c-verification.md`.

## Planner + Worktree Isolation (Phase 3B / 3C)

Phase 3B (`planning.rs`) adds a planner whose only output is a *proposal*:
`classify_intent` deterministically bypasses simple commands, and every
plan passes the schema parser, `PlanValidator`, and compiler before the
scheduler (authoritative) executes it. Engine surfaces: `plan_request /
validate / approve / reject / edit / execute / resume / cancel`, planner
events on the bus (`EventFilter.planner`), IPC + CLI `terminal plan …`.
Docs: `docs/phase3b.md`.

Phase 3C (`worktrees.rs`) isolates every coding task in its own `git
worktree`:

```text
task_spawn ──► resolve_execution_environment ──► WorktreeManager
                 ├─ git worktree add -b flash/task/<id>-<slug> (base revision)
                 └─ ExecutionEnvironment ──► launch cwd = worktree path
                     (wrong-cwd guard before spawn, 3c.md §12)
completion ──► git-generated DiffSummary + TaskResult provenance
                 └─ NeedsReview → Approved/Rejected → explicit Merge
                     (merge-tree conflicts surfaced, never auto-resolved)
```

- **`WorktreeManager`** (`terminal-session::worktrees`): deterministic
  branches/ids per (task, attempt), dirty-workspace policy
  (`RequireClean` refuses rather than discard user work), retry worktree
  policy (fresh/reuse), budget, `merge()` from `Approved` only, policy
  cleanup, restart `scan()` with ownership reconnect, orphans surfaced
  (never deleted).
- **Engine glue** (`terminal-workspace`): `resolve_execution_environment`
  in the spawn path, completion diff/review capture, review state sync,
  worktree API (list/inspect/diff/merge/discard/cleanup/orphans/budget),
  `TaskEnvironmentPreview`, `PersistedState.worktrees` (versioned,
  secret-free) with restart reconnection.
- **Surfaces**: IPC worktree request/response set + CLI
  `terminal worktree list|inspect|diff|merge|discard|cleanup|orphans|
  budget` and `terminal task environment <task-id>`.
- **Validation**: `tests/phase3c` (11 tests: parallel isolation ×5,
  cross-contamination, review gate, merge, conflict, cancellation, retry,
  persistence/orphans, traversal, secret safety) + IPC socket test.
  Docs: `docs/phase3c.md`, `docs/worktrees.md`,
  `docs/execution-environments.md`, `docs/review-and-merge.md`, ADRs
  0012/0013.

## Artifact Collaboration (Phase 3D)

Phase 3D (`artifacts.rs` + `collaboration.rs`) turns the isolated task set
into an artifact-mediated workflow — no direct agent messaging:

```text
task ──► agent ──► artifact (engine-stamped, redacted, bounded)
                      │
   dependent task ────┘ declares input_artifacts (readiness-gated)
      │  materialized grant into its own worktree before spawn (§11)
      ▼
   independent reviewers ──► deterministic consensus (§20–§21)
      ▼
   synthesis over explicitly selected results + artifacts (§14)
      ▼
   human approval / replan signal ──► human replan (§44–§45)
```

- **`ArtifactStore`** (`terminal-session::artifacts`): `artifact://` ids,
  provenance-stamped `Artifact` (task/agent/workspace/worktree/revision),
  bounded + redacted payloads, `ArtifactSelector`, `ArtifactLineage`
  (producers/consumers/task_outputs), `ArtifactAccessPolicy`, retention
  (default Keep), `ArtifactMaterializer` for cross-worktree handoffs.
- **`collaboration.rs`**: `ReviewFinding` (severity ladder Info→Critical,
  first-class artifact), `ReviewReport`, `ReviewPolicy` + deterministic
  `ReviewAggregator` (NeedsReview/Critical/ApprovedCandidate), and
  `ResultSynthesizer` → `SynthesisResult` + `SynthesisProvenance` that
  rejects hallucinated artifact ids (§54).
- **Orchestration**: structured `TaskResult` (metrics, warnings, errors,
  recommendations), `TaskErrorKind::ArtifactMissing` for the readiness
  gate (missing grants → `Blocked`, never silent), `input_artifacts`.
- **Engine glue** (`terminal-workspace`): `register_task_artifacts` from
  the deterministic diff at completion, materialization-before-spawn,
  review reports/consensus, `synthesize`, `signal_replan` /
  `replan_workflow` (new plan → `NeedsApproval`), `PersistedState`
  artifacts/review_reports/replan_signals, new `ApplicationEvent`s
  (ArtifactCreated/Consumed, ReviewFindingCreated, ReplanSignaled).
- **Surfaces**: IPC artifact/review/synthesis/replan request set + CLI
  `terminal artifact list|get|select|lineage`, `review …`, `synthesize …`,
  `replan …`, `task …` output-artifact support.
- **Validation**: `tests/phase3d` (12 tests: creation/metadata/selection,
  lineage, cross-worktree consumption, readiness block, access control,
  structured results + consensus, synthesis + hallucination rejection,
  redaction, restart recovery, replan signal + human replan) + IPC test.
  Docs: `docs/phase3d.md`, `docs/artifacts.md`,
  `docs/review-findings.md`, `docs/result-synthesis.md`,
  `docs/agent-collaboration.md`, ADRs 0014/0015.

## Adaptive Orchestration (Phase 3E)

Phase 3E (`adaptive.rs`) adds controlled replanning — the planner may
*propose* a revised plan; it never mutates or executes the workflow (§2):

```text
evidence (failures, tests, findings, conflicts, budget)
   ↓
ReplanSignal (§4) ── deterministic WorkflowEvaluator (§6–§7)
   ↓
PlannerRequest (mode=Replan, bounded ReplanContext §10–§11)
   ↓
ProposedReplan + PlanDiff (§12, §14) ── immutable PlanVersion history v1→v2→v3 (§13)
   ↓
PlanValidator (§21) ── Human approve / edit (revalidate) / reject (§15–§17)
   ↓
TaskGraph migration (completed work preserved §18; explicit, approved
invalidations §19–§20)
```

- **`adaptive.rs`** (`terminal-session`): `ReplanTrigger` taxonomy (§3),
  `ReplanSignal` (severity/evidence/persisted, §4–§5), `WorkflowEvaluator`
  (deterministic rules: test failure, critical finding, missing artifact,
  merge conflict, budget, repeated retry, §7), dedup key + cooldown
  (§8–§9), `ProposedReplan`/`PlanDiff`/`PlanVersion` (§12–§14),
  `TaskInvalidation`/`ArtifactInvalidation` (§19–§20, old artifacts
  preserved), `AutonomyPolicy` Manual/Assisted/Automatic (Automatic
  disabled, §34–§37), `ReplanLimits` (`max_replans`=5, cooldown=30s,
  §9/§31), `HumanEscalation` (§33), `ReplanMetrics` +
  `PlannerQualityMetrics` (§24–§25).
- **Engine glue** (`terminal-workspace`): `AdaptiveState`, per-frame
  evaluator snapshot (task/review/conflict/budget), `record_replan_signal`
  (dedup + cooldown + `ReplanRequested` event), `replan_workflow`
  (proposal + version + diff), `replan_approve/reject/edit` (revalidate on
  edit), `invalidate_task` (human-gated) / `invalidate_artifact`,
  `workflow_history` / `workflow_interventions`, loop protection + human
  escalation, merge-conflict → signal hook, `PersistedState.adaptive`
  (§42), new `ApplicationEvent`s (ReplanProposed/Approved/Rejected/Edited,
  PlanSuperseded, TaskInvalidated, ArtifactInvalidated, BudgetRisk,
  HumanEscalation, §39).
- **Surfaces**: IPC `replan list|inspect|approve|reject|edit`,
  `workflow history|interventions`, `invalidate-task|artifact`,
  `adaptive status` + CLI `terminal replan list|show|approve|reject`,
  `terminal workflow history|interventions`, `terminal adaptive`,
  `terminal invalidate-task|invalidate-artifact` (§40–§41).
- **Validation**: `tests/phase3e` (11 tests: failing-tests adaptive flow,
  critical-finding, rejection leaves workflow intact, edit revalidation,
  task/artifact invalidation, replan-limit loop protection + escalation,
  budget risk, merge-conflict trigger, persistence, malicious planner
  rejection, signal coalescing) + `adaptive.rs` unit tests.
  Docs: `docs/phase3e.md`, `docs/adaptive-orchestration.md`,
  `docs/replanning.md`, `docs/workflow-history.md`,
  `docs/human-escalation.md`, ADR 0016.

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
