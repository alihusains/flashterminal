# ADR 0020: Recovery Model

## Problem
Agents, planners, providers and the application itself can die at any
moment — mid-plan, mid-execution, during approval, during a replan. Before
Phase 4 the system had crash handling in places (persistence round-trips,
interrupted-task restore) but no unified model for *failure visibility and
explicit recovery*: what happens when the agent dies, the planner dies,
the provider fails, the app crashes, the machine sleeps, or the state file
is corrupted?

## Goal
Fail visibly, never silently corrupt, and recover explicitly — across
agent, planner, application, provider, sleep/wake, and persistence
failures (§23–§30).

## Options Considered
1. **Assume processes survive**: Rejected — crashes are a fact of life;
   the plan explicitly forbids assuming arbitrary process restoration.
2. **Auto-restart everything invisibly**: Rejected — the plan forbids
   silently resuming or masking failures; a crash must become explicit
   state, not a lie.
3. **Fail-visible + explicit recovery**: Selected. Every failure class is
   surfaced as typed, audited state; recovery is an explicit action
   (restart fresh from versioned, secret-free persisted state; pause/
   resume; quarantine + backup for corrupt files).

## Decision
- **Agent crash (§23)**: killed mid-workflow → task failure is explicit,
   dependents block cleanly, worktree + artifacts preserved, TaskFailure
   replan signal raised. No orphan claims of success.
- **Planner loss (§24)**: provider dies mid-flow → typed
   `PlannerPhase::Failed` + `planner_last_error`, nothing half-created,
   scheduler untouched; a healthy provider is usable immediately.
- **Application crash (§25)**: restart = fresh engine from persisted,
   versioned, secret-free state. Plan versions + diff metadata, artifact
   metadata (payloads re-read on demand), pending approval/replan
   proposals and the task graph all survive; **nothing auto-approves**
   across restart. No claim of arbitrary process restoration.
- **Provider failure (§26)**: timeout / rate limit / invalid credential /
   network loss / invalid response are all visible and typed; retry is
   bounded (auth failures never retried, flaky retried once); healthy
   tasks are not corrupted by a sibling's failure.
- **Sleep/wake (§27)**: pause/resume path is deterministic and consistent;
   running processes are represented honestly.
- **Persistence corruption (§28)**: corrupt/truncated state is quarantined
   to `<path>.corrupt-<unix>` with a clear error; never blindly
   deserialized; no silent data loss.
- **Schema migrations (§29)**: persisted structures are versioned; old
   state migrates forward, unknown/newer versions are refused with an
   explicit error. Migration tests are permanent.
- **Undo/revert (§30)**: plan versions are immutable and inspectable with
   diffs; reverting workflow state is explicit and audited
   (`WorkflowReverted`); the filesystem is never silently reverted.
- **Global controls**: STOP ALL (§31) stops agents/tasks and preserves
   pending human decisions; PAUSE ALL (§32) gates new work while running
   processes are represented honestly (no phantom-paused state — the
   Phase 3F bug has permanent regression coverage); replan-limit and
   budget exhaustion raise a human escalation (§33).

## Consequences
- **Positive**: every failure class has a tested, explicit behavior; users
  can trust state survives a crash and nothing lies about what is running;
  corrupt data never silently degrades.
- **Negative**: recovery is explicit, never magical — a crashed app
  restarts fresh (no arbitrary process restoration), and some workflows
  require a human decision after a crash rather than resuming invisibly.
