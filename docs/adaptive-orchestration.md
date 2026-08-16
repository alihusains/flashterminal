# Adaptive Orchestration (Phase 3E)

Phase 3E lets a workflow adapt to failures and new evidence **while the
deterministic execution engine and human approval boundary stay firmly in
control** (3e.md §1). This document describes the loop; see `replanning.md`
for the signal→proposal→approval flow, `workflow-history.md` for plan
versioning, and `human-escalation.md` for what happens when automation
cannot safely continue.

## The loop

```text
Plan → Execute → Observe → Evaluate → Replan Signal
  ↑                                              │
  └────── continue ──── human approval ← planner ←┘
```

The critical rule (§2): the planner may **propose** a new plan, but must
never mutate or execute the workflow directly. The only valid path is
`Evidence → ReplanSignal → Planner → ProposedPlan → PlanValidator → Human
Approval → TaskGraph → TaskScheduler`.

## Triggers

`ReplanTrigger` is a small, explicit taxonomy (3e.md §3):

`TaskFailure`, `CriticalReviewFinding`, `RepeatedRetryFailure`,
`ArtifactMissing`, `DependencyInvalidated`, `BudgetRisk`,
`EnvironmentFailure`, `TestsFailed`, `MergeConflict`, `ManualUserRequest`.

Each signal carries `severity` (`Info` / `Warning` / `Critical`, §5),
`reason`, `evidence_artifacts`, and is persisted (§4, §42). Severity never
auto-executes — it only drives UI/approval behavior.

## Deterministic evaluation

`WorkflowEvaluator` (3e.md §6–§7) is a **pure function** of observable
state — no LLM is asked to detect failures:

| Rule | Signal |
|------|--------|
| Test failure | Replan candidate (`TestsFailed`, Warning) |
| Critical review finding | Replan candidate (`CriticalReviewFinding`, Critical) |
| Required artifact missing | Replan (`ArtifactMissing`, Warning) |
| Merge conflict | Replan candidate (`MergeConflict`, Warning) |
| Budget exceeded / risk | Replan or Stop (`BudgetRisk`, Critical/Warning) |
| Repeated task failure | Replan candidate (`RepeatedRetryFailure`, Warning) |
| Plain task failure | Replan candidate (`TaskFailure`, Warning) |

The engine builds a `WorkflowSnapshot` from authoritative state (task
statuses, retries, review findings, worktree merge results, budget) and the
evaluator returns signals. Identical signals are **coalesced** (§8: dedupe
key = workflow + task + trigger + evidence fingerprint) and **cooldown-
gated** (§9: `replan_cooldown_seconds`, default 30) — 100 identical test
failures never become 100 replan requests.

## Replan context

`ReplanContextBuilder` (3e.md §10) gathers the current plan, completed/
running/failed/remaining tasks, artifacts, review findings, worktree state,
budget and user constraints into a **bounded** context — never unlimited
logs. The planner request carries `mode: Replan` (§11) with the current
workflow, trigger, evidence and remaining work.

## Metrics

Tracked separately (§24–§25):

- **Replan metrics**: `replan_count`, `replan_trigger_count`,
  `replan_approval_rate`, `replan_rejection_rate`, `replan_edit_rate`,
  `time_to_replan`, `additional_cost`, `workflow_recovery_rate`.
- **Planner-quality metrics**: `valid_replan_rate`, `invalid_replan_rate`,
  `human_edit_rate`, `human_rejection_rate`, `successful_replan_rate`.

See also `docs/replanning.md`, `docs/workflow-history.md`,
`docs/human-escalation.md`, and ADR 0016.
