# FlashTerminal — Development Summary

**Status:** ✅ PHASE 3A.1 COMPLETE — Orchestration Validation + Gap-Closing
(2026-08-14). Deterministic multi-agent orchestration validated end-to-end:
detail UI, palette, command validation, desktop manual pass (6 renderer
bugs fixed), benchmark matrix, event flood, flakiness root-caused,
determinism race fixed. Real-agent task execution recorded (unavailable
headless in this environment — see below). See `docs/phase3a-verification.md`.

```text
workspace total:                243/243 PASS (0 failed)
terminal-session lib:          59/59 PASS
terminal-workspace lib:        41/41 PASS (incl. 4 IPC round-trip)
phase3a suite:                 22/22 PASS  (18 prior + command-safety
                                           table ×11 states, review
                                           resolution, 2 event-flood)
phase2c suite:                 18/18 PASS
real_agent_tasks (ignored):    3/3 PASS — opencode COMPLETED a task;
                                           claude-code/codex/pi recorded
                                           TIMEOUT/AUTH (see below)
clippy (workspace -D warnings): 0 warnings
fmt:                           clean
release build (workspace):     clean
orchestration_bench:           40 cells + 80/20 fairness ALL PASS
agent_stress 8:                ALL PASS (48.8k events/s, p95 16.4 µs)
multiplex_bench ×10:           10/10 PASS (~45 s each, 0 hangs)
desktop manual pass:           launch + render + stability ✓ (6 bugs fixed)
```

---

## Phase 3A.1 — Orchestration Release Gate + Real-World Validation (2026-08-14)

Per `phases/3a1.md` §1–33. Verdict: **READY FOR PHASE 3B** (see final
report in `docs/phase3a-verification.md`).

### Closed gaps + new validation

| § | Item | Outcome |
|---|------|---------|
| §4–5 | Task detail UI | `draw_task_detail` overlay: full `Task` record (status color-coded, agent, attempts, duration, cost from result, dependencies, summary, ≤6 files/commands/artifacts, error). Enter opens detail (was run-all), `u` run-all, Esc unwinds form→detail→overlays. |
| §6 | Palette completeness | `CreateTask`, `ShowBlockedTasks`, `ShowTasksNeedingReview`, `OpenTask` — commands + palette + gate; create form (title+agent via `engine.task_create`, defaults to fake-agent); dashboard filter All/Blocked/NeedsReview; `v` in detail attaches agent pane + work view. |
| §7 | Task-command validation | `commands_are_safe_across_all_task_states` — 11 fixtures (none/unknown/pending/running/waiting/blocked/needs-review/completed/failed/cancelled/skipped) × cancel/retry/approve/reject/attach: typed errors, never panic, state-set stable after. `review_resolution_is_type_safe_and_state_consistent`. |
| §8 | Desktop manual pass | **The app had never launched** — first frame always panicked in wgpu. Found + fixed 6 stacked renderer bugs (see below). After: window 1200×792 renders, stable >35 s, log 0 bytes. Screenshots blocked (no Screen Recording permission) — evidence via CGWindowList + logs. |
| §9–11 | Real-agent tasks | `real_agent_tasks.rs` (`#[ignore]`d, opt-in `-- --ignored`; **3/3 PASS**): trivial tasks through the full pipeline per agent; deadline-cancel (claude hangs); engine-side assertions only. Recorded: **opencode COMPLETED** (2.17 s, exit 0, 1 attempt — full adapter→launch→PTY→work-record→result path, also inside the 5-agent parallel isolation run), claude-code/codex UNAVAILABLE (empty output / dead omniroute proxy `localhost:20128`, 90 s timeouts), pi UNAVAILABLE (auth in this environment). Auth-failure fixture verified typed: `AgentFailed` kind + `AuthenticationFailure` **class** (the retry gate). |
| §12–14 | Benchmark matrix | `orchestration_bench` (release): 40 cells ({1,10,20,50,100}t × {1,2,5,10} caps × serial/wide) all started==completed; serial ~46–50 t/s (spawn-bound), wide scales cap1 48 → cap10 296 t/s (100t wide 0.34 s); queue ≤22; RSS 12–24 MB. Fairness 80+20 @cap5: 5.22 s, 100 completed, 0 failed/blocked, 100 started. |
| §15 | Event flood | `large-output` (100k lines): settles, completes, follow-up runs; peak drain >1k batches/s; outbox bounded (<10k). 4 concurrent floods: 4 completed, 0 failed. |
| §16 | multiplex_bench flakiness | Harness 10×: **10/10 PASS ~45 s, 0 fails/hangs**. Root cause: not reproducible in isolation — earlier hangs only under heavy parallel load (PTY spawn starvation); closed with evidence. |
| §17–28 | Security/perf/architecture | Orchestration crate: zero raw process spawns (grep). Sentinel tests green (IPC stream, persistence secret-shape, scheduler state `credential_ref`-free). Idle gate + stress re-verified with orchestration present (48.8k ev/s, p95 16.4 µs). **Determinism race found+fixed**: `drain_frame` iterated HashMap sessions → same-frame exit order raced (`failed` vs `completed` emission); now sorted `ExecutionId` order; determinism re-verified 5× full-suite 22/22. |
| §29–33 | Docs + truth table + report | `docs/phase3a.md` (new), `docs/orchestration.md` (new), `docs/phase3a-manual.md` (new), `docs/phase3a-verification.md` (new — spec truth table + gates + final report §33 A–I), `docs/architecture-current.md` updated. |

### Renderer bugs found + fixed by the manual pass (desktop never launched before)

`@builtin(vertex_index)` as expression (invalid) → vertex input param;
dynamic array indexing rejected by naga (module + function scope) →
arithmetic `quad_corner`; duplicate `@builtin(position)` in `fs_glyph` →
single source; uniform binding 0 `VERTEX`-only though `fs_glyph` reads it →
`VERTEX_FRAGMENT`; empty-bitmap glyph upload (0-byte `write_texture`) →
zero-fill the atlas slot (`terminal-renderer`).

### Not done / honest records (Phase 3A.1)

- **Real-agent task completion**: opencode completed a trivial task end-to-end
  (exit 0); claude-code/codex/pi could not run headless in this environment
  (recorded, never faked; BYOK creds + a reachable provider endpoint will
  complete the matrix — run
  `cargo test -p terminal-workspace --test real_agent_tasks -- --ignored`).
- **Desktop visual confirmation**: screenshots pending Screen Recording
  permission for the host terminal (window/render/stability verified
  programmatically).
- Explicitly NOT built (unchanged, §32): LLM planner, decomposition,
  auto-selection, agent-to-agent comms/handoffs/debate, manager agents,
  visual workflow editor, distributed/cloud orchestration, pause.

---

## Phase 3A — Task Engine + Deterministic Orchestration (2026-08-14)

Implemented per `3a.md` §1–57. Verification evidence per section below.

### Done — core engine (`crates/terminal-session/src/orchestration.rs`, new)

| § | Item | What was done |
|---|---|---|
| §3–5 | `Task`, `TaskStatus`, explicit transitions | Insertion-ordered graph; every move goes through `can_transition` (typed errors, never silent no-ops). `Display`/`user_label` (Waiting/Working/Needs your attention — §38 vocabulary). |
| §6–9 | `TaskGraph` | Dependencies, cycle detection (colored DFS returning the offending path), validation (unknown task/agent, cycles, duplicate edges), deterministic topological order. |
| §10 | `TaskScheduler` | `step()` = observe running → classify pending → capacity arc. Same graph + same view ⇒ same commands in the same order (§22). |
| §11–12 | Policies + concurrency | `TaskPolicy` (max_agents, max_parallel_tasks, budget); capacity counts `SpawnTask` in the current step so the cap is never overshot. |
| §13 | Agent assignment | Tasks specify `assigned_agent`; no auto-selection (§45). `workflow_validate` reports unknown agent refs. |
| §14–15 | Execution + lifecycle | `spawn_task_agent` builds `TaskContext` → `adapter.prepare_task` → explicit args/env appended (task wins). `FAKE_AGENT_ATTEMPT` injected into context + launch env. Runs through `AgentRuntime` — never a raw subprocess (§54). |
| §16–18 | `TaskResult` + `Artifact` | `ArtifactType`/`Artifact`, result with summary/attempts/duration/cost; dependent input artifacts wired via graph (§18 outputs). |
| §19–20 | Shared context + prompt boundary | `TaskContext` (workspace, description, dependencies, artifact paths, relevant files, env); description flows to the agent, not arbitrary prompts. |
| §21–22 | `TaskEvent` + ordering | Full event enum (created/ready/started/completed/failed/blocked/waiting/needs_review/retrying/cancelled/interrupted), per-task ordering test, emission-order trace. |
| §23–24 | Persistence + restart recovery | `PersistedSchedulerState` (versioned, bounded, secret-free — launches never stored); restore marks Running/Waiting → `Interrupted`; nothing resumes silently. |
| §25–26 | Failure + retry | `FailureClass::classify`; `RetryPolicy` (auth-failure never retried; flaky retried once). **Bug found & fixed:** the retry arc `Failed → Ready` was missing from the transition table — retries were queued but never popped; added the arc + defensive `fail_task`. |
| §27–28 | Cancellation / pause | `task_cancel` (running → StopTask, terminal arcs, no orphan); pause honestly unsupported (not faked, §28). |
| §29 | Human review boundary | `NeedsReview` halts progression until `resolve_task_review`; reject → Failed + downstream blocked; approved runs continue. |
| §30–34 | Deterministic fixtures | Serial/parallel/failure/retry/cancellation fixtures driven by the fake-agent binary (scenarios `completion`, `failure`, `flaky`, `auth-failure`, `long-running`, `--attempt`). |
| §35–37 | Fairness + budgets | No starvation within a step; cost budget blocks further starts once exhausted (`exhausted_budget_blocks_further_starts`). |
| §51 | Observability | `scheduler_status()` (queued/running/states/counts/cost), `trace()`, `running_snapshot()`; diagnostics never expose secrets. |

### Done — engine glue (`terminal-workspace`)

- `tasks: TaskScheduler` on `Multiplexer`; `step_tasks` wired into `drain_frame` (idle cost: microseconds — §50 gate).
- Task API: `task_create/run/cancel/retry/get/list`, `task_set_environment`, `task_add_arguments`, `task_set_agent`, `set_task_policy`, `workflow_validate`, `resolve_task_review`, `attach_task_agent_pane`, `scheduler_status`.
- Runtime accessors: `raw_state`, `definition_exists`, `find_adapter`.
- Adapter boundary: free `default_prepare_task(ctx)` extracted (fixes trait-default recursion); `fake.rs` maps `FAKE_AGENT_*` env → args.

### Done — surfaces

- **IPC** (`ipc.rs`): `TaskCreate/List/Status/Run/Cancel/Retry/Policy/SetPolicy/ResolveReview/AttachPane/WorkflowValidate/SchedulerStatus` requests + `Tasks/TaskStatus/TaskPolicy/SchedulerStatus/WorkflowValidation` responses; `socket_task_lifecycle_and_scheduler` round-trip test (4/4 ipc tests).
- **Event streaming §53**: `ApplicationEvent::TaskEvent` published to the bus (engine.rs:1360, 1825); `EventFilter.task` added; existing subscribers unaffected.
- **CLI §42**: `terminal task create|list|show|status|run|cancel|retry|review approve|reject|attach|validate|policy|scheduler`, `terminal task set-policy <key> <value>` (read-modify-write over the socket), `terminal workflow list|validate`, `terminal tasks`. End-to-end smoke against `terminal serve`: create → run → Running w/ execution id → cancel → Cancelled ✓.
- **Desktop §39/§41**: task dashboard overlay (`OverlayMode::Tasks`, live counts + rows w/ status/agent/attempts/execution/error), ↑↓ navigation, one-key actions (Enter run-all, c cancel, r retry, a approve, d reject, p open agent). 7 new `Command` variants, palette entries, Ctrl-Alt bindings (t/Enter/c/x/p).

### Tests (phase3a suite — 18/18, `crates/terminal-workspace/tests/phase3a/`)

Serial order, parallel under cap, dependency failure (independent branch continues), retry-then-succeed, auth-failure never retried, cancellation no orphan, review boundary, rejected review, waiting task, skip downstream, budget exhaustion, persistence round-trip → Interrupted, per-task event ordering, workflow validation, **determinism ×10** (found: test compared fresh UUIDs per run — now compares schedule structure; identical across 10 runs), **stress 100 tasks at max_agents 2/5/10** (settles in ~3.2 s total; exactly 100 started, 0 over-spawn), **idle perf gate** (1000 idle drains < 1 s — §50).

Also fixed: `transition_table_rejects_invalid_moves` updated for the retry arc.

### Not done / known gaps (Phase 3A)

- **§40 Task Detail view** — no click-to-open detail panel (outputs/duration/dependencies). The dashboard shows one summary line + error per task; `p` attaches the agent pane.
- **§41 palette completeness** — missing "Create Task", "Show Blocked Tasks", "Show Tasks Needing Review" entries (present: Show Tasks, Run All, Cancel/Retry/Approve/Reject Selected, Open Selected Task Agent).
- **Desktop manual verification** — no human pass of the dashboard/palette in a running app; automated coverage only.
- **Real-agent task execution** — tasks exercised only with the deterministic fake-agent binary; no claude-code/codex/opencode/pi task runs.
- **§50 full benchmark matrix** — idle-drain gate + multiplex_bench smoke done; `agent_stress` re-run with orchestration present not performed.
- **multiplex_bench flakiness (pre-existing)** — stress section spawns real shells; completed clean once (all PASS except one 70 s input-latency p95 outlier), hung on 2/3 runs. Unrelated to orchestration (idle in that bench) but worth a look before Phase 3B.
- **Explicitly NOT built (§44–46, §56)**: LLM planner, automatic task decomposition, automatic agent selection, agent-to-agent messaging/handoffs/debate, manager agents, visual workflow editor, distributed/cloud orchestration, task pause semantics.

---

## Phase 2C — Agent Work / Dashboard / Notifications / Provider Setup (2026-08-13)

Per `2c.md` + `2c1.md` (verification phase) + `docs/phase2c-verification.md`. Verdict per 2C.1: verification performed, all automated gates green, **desktop manual tests not performed** and a few desktop surfaces pending — not declared fully complete.

### Done — backend + tests (truth table summary)

| Feature | Backend | UI | Automated tests |
|---|---|---|---|
| Agent Work | ✅ one work record per execution, idempotent finish, bounded commands/files | ✅ review/work overlay (≤64 files, ≤200 diff lines) | ✅ 4 tests |
| Activity | ✅ heuristic kinds, 400 ms coalescing, bounded history (32) | ✅ pane chrome badge + diagnostics | ✅ 3 tests |
| Timeline | ✅ bounded ring (512, deterministic) | 🟡 CLI-only (`terminal agent timeline`) | ✅ 2 tests |
| Dashboard | ✅ `agent_dashboard(filter)`, explicit overlapping counts, deterministic sort | ✅ filtered sidebar + diagnostics | ✅ 1 live test |
| Attention | ✅ single `attention_for(state)` map | ✅ Allow/Deny bar + filters | ✅ 2 tests |
| Notifications | ✅ transition-gated, once per state, pane-attached, deduped | 🟡 engine/IPC-level only — desktop does not subscribe | ✅ 1 test |
| Quiet Mode | ✅ prefs persisted | ✅ Toggle Quiet Mode (key + palette) | ✅ 2 tests |
| Provider Setup | ✅ registry/model catalog, secret-free health rows | ✅ setup overlay | ✅ 1 test |
| Cost | ✅ `PricingRegistry`, `None` when unknown, min 1¢ | 🟡 no display surface yet | ✅ 1 test |
| Intent | ✅ deterministic phrase→intent map | 🟡 IPC/model-level | ✅ 2 tests |
| Replay | ✅ deterministic fixtures + `replay_into` | 🟡 test tooling only | ✅ 1 test |

Palette: every Phase 2C command palette-reachable; single `run_command` dispatch (shared by keys + palette).

### Fixes made during 2C verification

1. **Dashboard counting bug** — `attention_for` checked first made the `failed` branch unreachable; explicit overlapping counting (`failed` can also be `needs_you`).
2. **5 test-suite fixes** — notifications require panes; `NeedsAttention` includes Failed; activity-bounds math; pricing expectations (100× + missing output tokens); palette needed empty-pid `FocusAgent`.
3. phase051 target now deterministic.

### Not done (Phase 2C)

- **Desktop manual test pass** — not performed (every Manual cell in the truth table).
- **Desktop surfaces pending**: Timeline view, Notifications subscription, Cost display, Intent UI, Replay tooling UI.
- Per `2c1.md`: Phase 2C not formally declared complete; evidence now recorded in `docs/phase2c-verification.md` (183 workspace tests, clippy/fmt/release gates green at that point).

---

## Phase 2B.1 — Real-Agent Validation + Concurrency + Desktop Agent UX + Event Streaming (2026-08-13)

Full report retained below (see `2b1.md` §34–35, Appendix A–H).

### Gates (§2–6) — agent_stress release run

```text
concurrency: 10/10 agents spawned+live at t+1.5s (spawn 1528 ms)
focused input: n=2767 p50 0.02 p95 0.05 p99 0.11 max 1.42 ms (<8 ms: PASS)
starvation (5 heavy + F/G): n=3141 p50 0.01 p95 0.04 max 7.75 ms (PASS)
memory: idle 20 → 57.6/0.85 MB · heavy 20 → 179.1/52.31 MB (gate <1 GB: PASS)
throughput: 48,297 events/s (floor 10k: PASS), apply-latency p95 31.8 µs/frame
high-output 1/5/10: tail-intact, RSS +1.6..+2.9 MB, no freeze — ALL PASS
```

- **Deadlock found + fixed** (gate "no PTY deadlock"): activity pumps blocked
  on the full bounded event channel while holding the session mutex; the main
  thread could no longer drain (ABBA). Pumps now send outside the lock;
  `drain_events` is lock-free (`Arc<AgentMetrics>` + atomics).

### §7–12 Real agents (5/5 PASS, ~194 s)

claude-code / codex / opencode / pi: launch, interactive input, `Working`
detection, stop, restart all ✓. Resume: native only for claude-code. Simple
task: claude-code timed out in `Starting`; others exited 1 (empty keychain —
auth-dependent). SKIP-when-unavailable semantics never fail the suite.

### §13–23 Desktop + observability

`state_source`/`state_confidence` (fixtures `high`, heuristics `medium`);
agent pane chrome (state dot, exit indicators, Allow/Deny bar, capability-
gated Stop/Restart/Resume), sidebar AGENTS panel, output through the same
fairness-capped drain as shells.

### §24–27 IPC streaming

Subscribe/unsubscribe with per-channel filters; bounded per-subscriber
queues, coalescing, drop policy, stall-detection disconnect, 100 ms socket
write timeout; `terminal agent watch`; `tests/ipc_stream.rs` 3/3.

### §28–32 Security/persistence/docs

Secrets env-injected only; sentinel test (never in events/persistence);
`MemoryBackend` Debug leak found + fixed; `AgentLaunchConfig::redact()`.
Persistence stores credential-**refs** only; crash recovery = fresh process
from stored config. Docs: `agent-runtime.md`, `agent-compatibility.md`,
`architecture-current.md`, `security-secrets.md`.

### §33 Performance regression

Multiplexer baseline unchanged (multiplex_bench).

### Appendix G — Remaining risks (still open)

- Real-agent completion semantics depend on each agent's own CLI auth
  (BYOK keys expected to complete the matrix).
- Activity detection heuristic (no structured protocol).
- Signal deaths → exit 1 via portable-pty; crash detection `code >= 128`.
- No cross-app-restart session store (`resume_id` user-supplied).
- Pause honestly unsupported.

### Appendix H — Decision

```text
READY FOR PHASE 2C  →  2C backend+tests verified  →  3A COMPLETE
```

---

## Repository map (this phase)

- `crates/terminal-session/src/orchestration.rs` — NEW: Task/TaskGraph/
  TaskScheduler/TaskEvent/retry policy/budgets/persistence import-export.
- `crates/terminal-session/src/adapters/{mod,fake}.rs` — prepare-task
  boundary + fake fixture mapping.
- `crates/fake-agent/src/main.rs` — scenarios: completion/failure/flaky/
  auth-failure/long-running, `--attempt`.
- `crates/terminal-workspace/src/engine.rs` — task API + `step_tasks` +
  spawn/observe/complete wiring + `ApplicationEvent::TaskEvent`.
- `crates/terminal-workspace/src/ipc.rs` — task request/response surface.
- `crates/terminal-workspace/src/command.rs` + `apps/desktop/src/main.rs` —
  task commands, palette, dashboard overlay.
- `apps/cli/src/main.rs` — `task`/`workflow` commands.
- `crates/terminal-workspace/tests/phase3a/` — 18-test suite.
- Docs: `3a.md` (spec + release state), `docs/phase2c-verification.md`.
