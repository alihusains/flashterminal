# Recovery Model

Phase 4 (phases/4.md §23–§30) hardens FlashTerminal against failure:
agent crashes, planner crashes, application crashes, provider failure,
sleep/wake, and corrupted persistence. The governing principle: **fail
visibly, never silently corrupt, and recover explicitly.**

## What happens when…

### An agent dies (§23)

Test: `crash_agent_killed_midworkflow_recovers` (phase3f).

```text
workflow remains valid      ✓  dependent tasks block cleanly
artifacts remain            ✓  worktree record + written file preserved
state remains persisted     ✓  worktree/artifact metadata survive
agent state becomes explicit ✓  failure is surfaced, not masked
user receives attention     ✓  TaskFailure replan signal emitted
```

An agent killed mid-execution leaves the workflow coherent: the failed
task is explicit, dependents block, artifacts are preserved, and the
failure produces a replan signal that reaches the user.

### The planner dies (§24)

Test: `crash_planner_loss_workflow_not_corrupted` (phase3f).

The provider vanishing mid-flow surfaces as a typed, visible failure
(`PlannerPhase::Failed`, `planner_last_error`), never a half-created plan.
Nothing is corrupted: the scheduler is untouched, the workflow history is
empty, and a healthy provider is fully usable again immediately.

### The application crashes (§25)

Test: `crash_app_restart_preserves_plan_and_decisions` (phase3f).
Simulated by snapshotting state and re-seeding a fresh engine:

```text
workflow history restored   ✓  plan version + diff metadata
plan versions restored      ✓  superseded_by chain intact
artifacts preserved         ✓  metadata survives; payloads re-read on demand
approval state handled safely ✓ pending replan proposal restored;
                               nothing auto-approved across restart
credentials still secret-free ✓ references only (redactor at every boundary)
```

We do **not** claim arbitrary process restoration — a crash restarts fresh
from persisted, versioned, secret-free state.

### A provider fails (§26)

Test: `provider_failure_visible_and_recoverable` (phase3f).

- planner provider network outage → typed, visible `PlannerPhase::Failed`,
  no half-written plan,
- agent provider auth failure → task fails visibly with an error,
  other tasks unaffected,
- invalid response → planner rejects it; budget/policy unchanged.

Retry behavior is bounded (auth failures never retried; flaky failures
retried once) and budget remains correct. See `docs/agent-runtime.md` for
the retry policy.

### The system sleeps / wakes (§27)

Test: `sleep_wake_pause_resume_state_consistent` (phase3f). A workflow can
be paused (no new work starts; running processes are represented
honestly), the machine sleeps, wakes, and resumes with consistent state.
Terminal/PTY/IPC/notification recovery is exercised through the same
deterministic pause/resume path.

### Persistence is corrupted (§28)

`persist.rs` never blindly deserializes corrupted state:

- corrupt/truncated files are **quarantined** to
  `<path>.corrupt-<unix>` and reported with a clear error,
- startup is safe, recovery path is explicit, no silent data loss.
  Test: `corrupt_state_is_quarantined_and_reported` (persistence.rs).

### State was written by an older/newer version (§29)

All persisted structures are versioned. `migrate()` chains old versions
forward (`v0 → v1 → v2 → …`), and **unknown/newer versions are refused
with an explicit error** so the caller can back up and start fresh.
Migration tests are permanent: `migrates_unversioned`, `migrates_v1_to_current`.

### The user wants to undo (§30)

Plan versions (v1 → v2 → v3) are immutable and inspectable with diffs
(`plan_versioning_inspectable_with_diffs`, phase3f). Reverting workflow
state is explicit and audited (`WorkflowReverted`) — the filesystem is
**never** silently reverted without explicit user action.

## Global controls

- **STOP ALL** (§31, `stop_all()`): stops live agents, cancels active
  tasks, prevents new work, preserves pending human decisions (tasks in
  NeedsReview + open replan proposals), returns a
  `StopAllReport { agents_stopped, tasks_stopped, preserved_decisions }`,
  and is audited.
- **PAUSE ALL** (§32, `pause_all()`): blocks new work from starting while
  existing running processes continue and are represented **honestly** —
  we never report `Paused` for a process still running. Resume restores
  gating. The Phase 3F PAUSE-ALL phantom-running bug has permanent
  regression coverage (`pause_all_gates_new_work_resume_restores`).
- **Human escalation** (§33, `HumanEscalation`): replan-limit exhaustion,
  budget exhaustion and repeated failures raise a visible escalation with
  workflow, reason, attempts, last failure and a decision surface
  ([Review Workflow] / [Stop]).

## Coverage map

| Scenario | Test (phase3f unless noted) |
|----------|------------------------------|
| Agent crash mid-workflow | `crash_agent_killed_midworkflow_recovers` |
| Planner crash | `crash_planner_loss_workflow_not_corrupted` |
| App crash + restart | `crash_app_restart_preserves_plan_and_decisions` |
| Provider failure | `provider_failure_visible_and_recoverable` |
| Sleep/wake | `sleep_wake_pause_resume_state_consistent` |
| Corrupt state | `persistence.rs: corrupt_state_is_quarantined_and_reported` |
| Schema migration | `persistence.rs: migrates_unversioned / migrates_v1_to_current` |
| STOP ALL | `emergency_stop_all_preserves_human_decisions` |
| PAUSE ALL | `pause_all_gates_new_work_resume_restores` |
| Replan limit escalation | `replan_loop_protection_stops_at_max` |
