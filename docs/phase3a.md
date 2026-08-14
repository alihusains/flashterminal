# Phase 3A — Deterministic Multi-Agent Task Orchestration

Architecture and design notes for the orchestration layer
(`crates/terminal-session/src/orchestration.rs` + engine glue in
`terminal-workspace`). Companion to `docs/orchestration.md` (operator
guide) and `docs/phase3a-verification.md` (evidence).

## Design intent

The scheduler is a **pure function of its inputs**: the same task graph +
the same runtime view produce the same commands in the same order
(`§10`). Wall-clock truth never enters scheduling decisions — determinism
is a correctness property here, not a test convenience.

## Components

| Component | Responsibility |
|-----------|----------------|
| `Task` | One work item: id, title/description, status, dependencies, explicit `assigned_agent`, attempt count, duration, error, result. No LLM-generated summaries — everything is observed/bounded data. |
| `TaskGraph` | Insertion-ordered directed graph; cycle detection (colored DFS returning the offending path); validation (unknown task/agent, cycles, duplicate edges); deterministic topological order. |
| `TaskScheduler` | `step()` = observe running → classify pending → capacity arc. States via `can_transition` (typed errors, never silent no-ops). |
| `TaskPolicy` | Workspace-level: `max_agents`, `max_parallel_tasks`, dependency-failure policy (block downstream / skip downstream), `review_required`, `RetryPolicy`, `max_cost_cents`. Hard caps — never exceeded. |
| `TaskEvent` | created/ready/started/completed/failed/blocked/waiting/needs_review/retrying/cancelled/interrupted; emitted in scheduler order; per-task ordering is guaranteed. |
| `PersistedSchedulerState` | Versioned, bounded, **secret-free** (launches never stored); restore marks Running/Waiting → `Interrupted` (never resumes silently). |
| `FailureClass` | Typed failure taxonomy (`classify`); `RetryPolicy::may_retry` gates retries: auth-failure never retried, flaky once, transient/network/crash retried. |

## Execution path (no raw subprocesses)

```text
task_run ──► TaskScheduler::step
              └─ SpawnTask(TaskContext) ──► adapter.prepare_task (free fn)
                                              └─ TaskLaunchConfig (args/env, task wins)
                                                 └─ AgentRuntime (owns all spawning, §54)
runtime exit ──► engine observes (deterministic drain order) ──► scheduler classifies
```

The orchestration crate contains **zero** `std::process`/`Command::new`
calls — verified by grep in the 3A.1 security pass.

## Determinism guarantees + one known race (fixed 3A.1)

- Scheduler decisions: deterministic (same graph + view).
- Event emission per task: ordered (created → ready → started → … → terminal).
- **Cross-task emission order**: `drain_frame` iterated the session map
  (a `HashMap`); two agents exiting in the same frame could emit
  `failed`/`completed` in either order under load. Fixed 3A.1: sessions
  drain in sorted `ExecutionId` order. Verified with 5 consecutive
  full-suite determinism runs.

## Retry arc (bug found + fixed in 3A)

The `Failed → Ready` transition was missing from the table: retries were
queued but never popped. Added the arc plus a defensive `fail_task` so a
failed task can never wedge the workflow.

## Human review boundary

`review_required` tasks halt at `NeedsReview`; the workflow does not
progress past them until `resolve_task_review` (approve → continue;
reject → Failed + downstream blocked/skipped per policy).

## Fairness + budgets

- No starvation within a step: independent tasks share capacity fairly.
- Cost budget: once the scheduler knows the remaining budget is
  exhausted it blocks further starts (`BudgetExceeded` typed error).

## Fake-agent fixtures

`crates/fake-agent` scenarios: `completion`, `failure`, `flaky`,
`auth-failure`, `long-running` (`--duration`), `large-output` (100k
lines), `streaming`, `waiting`, `approval`. Scenario selected via
`FAKE_AGENT_SCENARIO` env (task `environment` or launch env);
`FAKE_AGENT_ATTEMPT` drives flaky behavior.

## Surfaces

- **Engine API**: `task_create/run/cancel/retry/get/list`, env/args
  knobs, `set_task_policy`, `workflow_validate`, `resolve_task_review`,
  `attach_task_agent_pane`, `scheduler_status`, `scheduler_trace`.
- **IPC**: `TaskCreate/List/Status/Run/Cancel/Retry/Policy/SetPolicy/
  ResolveReview/AttachPane/WorkflowValidate/SchedulerStatus` +
  `ApplicationEvent::TaskEvent` on the event bus.
- **CLI**: `terminal task …`, `terminal workflow …`, `terminal tasks`,
  `terminal scheduler`.
- **Desktop**: dashboard overlay + detail panel + create form + filters
  (see `docs/phase3a-manual.md`).

## Known limitations (3A.1, honest)

- Real-agent task *completion* not demonstrated here: claude-code hangs
  headless, codex routes to a dead local proxy, opencode/pi lack
  credentials in this environment (all recorded, never faked).
- `Pause` semantics are explicitly not built; cancellation is the
  supported stop.
- No cross-app-restart session store for agent sessions (`resume_id`
  user-supplied).
