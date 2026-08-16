# Phase 3E Report — Adaptive Orchestration + Controlled Replanning

## A. Replanning architecture

```text
Evidence (task failures, test results, findings, conflicts, budget)
   ↓
ReplanSignal (§4) — WorkflowEvaluator is pure + deterministic (§6–§7)
   ↓
Planner (mode=Replan, bounded ReplanContext §10–§11)
   ↓
ProposedReplan (§12) — reason, changes, new/removed/modified tasks,
                       deps, agent recs, estimated cost, warnings
   ↓
PlanValidator — the same gates as an initial plan (§21)
   ↓
Human Approval (§15–§17) — approve / edit / reject
   ↓
TaskGraph migration — completed work preserved (§18), invalidations
   explicit + approved (§19–§20)
```

Implemented across `crates/terminal-session/src/adaptive.rs`
(triggers/signals/evaluator/proposals/diff/versions/invalidations/
escalation/policy/metrics), `planning.rs` (`PlannerRequestMode`),
`execution.rs` (Phase 3E events), the engine (`AdaptiveState`, signal
pipeline, approve/reject/edit, invalidation, history, persistence), IPC +
CLI, and `tests/phase3e`.

## B. Trigger matrix

| Trigger | Severity | Where observed |
|---------|----------|----------------|
| `TaskFailure` | Warning | Task status Failed |
| `CriticalReviewFinding` | Critical | Review findings ≥ Critical |
| `RepeatedRetryFailure` | Warning | attempt_count ≥ 3 + Failed |
| `ArtifactMissing` | Warning | Blocked with artifact error |
| `BudgetRisk` | Critical (exceeded) / Warning (risk) | spent/projected vs budget |
| `EnvironmentFailure` | Warning | adapter/spawn failures |
| `TestsFailed` | Warning | failed task whose work ran tests |
| `MergeConflict` | Warning | `worktree_merge` returns Conflict |
| `ManualUserRequest` | Info | explicit `signal_replan` |

Severity never auto-executes (§5); it drives UI/approval behavior.

## C. Plan versioning

`PlanVersion` records v1 → v2 → v3 with `superseded_by` links and a
`PlanDiff` from the previous version (§13–§14). The original plan is never
mutated — history is immutable and survives restarts (§42). Diffs show
added/removed/modified tasks, changed agents, changed dependencies, changed
budget (test: `failing_tests_emit_signal_and_proposed_replan` + unit
`plan_diff_detects_added_removed_modified_and_agents`).

## D. Human intervention

When automation cannot safely continue — e.g. the replan limit is reached —
the engine records a `HumanEscalation` (what happened / attempted /
evidence / options, §33) and publishes the `HumanEscalation` event. The
workflow does **not** continue automatically. Surfaces: `workflow
interventions` (CLI/IPC) and the escalation records in the adaptive
persisted state. Verified by `replan_limit_blocks_and_escalates`.

## E. Loop protection

`ReplanLimits { max_replans: 5, replan_cooldown_seconds: 30 }` (§9, §31).
Equivalent signals coalesce on a dedupe key (§8); reaching the limit blocks
further replans with `NotAllowed("replan limit reached …")` and escalates
(§32). Repeated failing frames never produce 100 signals (test:
`repeated_failure_without_replan_escalates_once` — coalesced to ≤ 2).
`AutonomyPolicy` defaults to `Manual`; **Automatic is disabled in Phase
3E** (§34–§37).

## F. Real-agent adaptive workflow

`failing_tests_emit_signal_and_proposed_replan` runs the full loop with the
deterministic `fake-agent` binary (same PTY runtime as real agents):
Research → Implementation → Tests; Tests fails (`tests-failed` scenario →
`$ cargo test` + FAILED output + exit 1); the evaluator emits
`TestsFailed`; the mock planner proposes Investigate → Fix → Re-run; the
proposal lands in `NeedsApproval` with a versioned `PlanDiff`. The
critical-finding fixture (§27) runs Implementation → Security Review where
a Critical finding produces a Critical `CriticalReviewFinding` signal.

## G. Performance

| Metric | Phase 3E |
|--------|----------|
| Replan latency | One planner round-trip + validation; proposal/diff/version computed in-process (sub-ms) |
| Evaluator latency | Pure in-memory pass over task/finding/budget state per frame — negligible |
| 20-task / 10-agent | Not re-benchmarked this phase; the evaluator is O(tasks + findings), the scheduler unchanged |
| Memory | `AdaptiveState` holds bounded signal/version/proposal vectors |
| Event throughput | New events are metadata-only and coalesced by the existing bus policy |

Replanning never blocks terminal interaction — the engine's drain loop is
unchanged.

## H. Security

- The planner cannot raise its own autonomy, `max_replans`, `max_agents`,
  or budget (§38) — those are engine-owned config.
- `planner_cannot_raise_its_own_limits_or_bypass_approval` feeds a mock
  plan with `estimated_cost_cents` above the configured budget; the normal
  pipeline rejects it with `BudgetExceeded` — no approval bypass.
- `replan_edit_requires_revalidation` shows an edited plan pointing at an
  unavailable agent fails validation (§16, §29).
- Task invalidation is explicit + human-gated; artifacts are never deleted
  (§19–§20).

## I. Known limitations

1. **Automatic replanning is disabled** by design in Phase 3E (§37).
2. **Deterministic evaluator only**: failures that leave no observable
   trace (status/artifacts/findings/budget) cannot trigger a replan.
3. **Test-failure attribution is heuristic**: `TestsFailed` fires when a
   failed task's observed commands include a test run; it cannot parse
   every test runner's output.
4. **Budget risk uses estimated costs**, which are `None` when no pricing
   is configured — the trigger then only fires on explicit budget
   observations.
5. **Replan proposals reuse the planner's single pending-plan slot**:
   a new replan supersedes the previous proposal's planner phase (history
   still records both).
6. **`process_exit_is_tracked` / phase3a determinism are occasionally
   load-flaky** in parallel CI (pre-existing engine tests, unrelated to
   Phase 3E); they pass deterministically in isolation.

## J. Decision

```text
READY FOR PHASE 4
```
