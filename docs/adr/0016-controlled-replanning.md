# ADR 0016: Controlled Replanning

## Problem
When a running workflow diverges from its plan — a task fails, tests
regress, a review finds a critical issue, a merge conflicts, or cost
approaches the budget — the workflow must adapt. But letting an LLM
re-plan, re-write the graph, and re-execute on its own is exactly the
"agent → LLM → spawn task" failure mode Phase 3E forbids (3e.md §2): no
auditability, no human boundary, and unbounded failure loops.

## Goal
Detect divergence deterministically, surface a *proposal*, and let a human
approve, edit, or reject it — while keeping the deterministic validator and
scheduler authoritative and the history immutable.

## Options Considered
1. **Autonomous replanning**: Rejected — Phase 3E explicitly disables it
   (§37); the planner must never mutate or execute the workflow directly
   (§2).
2. **Replan on LLM judgment**: Rejected — an LLM should not decide when a
   workflow failed (§6–§7); evaluation is a deterministic function of
   observable state.
3. **Deterministic signals → planner proposal → human approval**:
   Selected. `WorkflowEvaluator` turns observed state into `ReplanSignal`s
   (§7), the planner produces a `ProposedReplan` (§12), and only human
   approval (`replan_approve`) applies it (§15–§17).

## Decision
- **Signals**: `ReplanTrigger` (small taxonomy, §3) + `ReplanSignal`
  (id/workflow/task/trigger/severity/reason/evidence/created_at, §4),
  persisted (§42), deduplicated by a coalescing key (§8) and gated by a
  configurable cooldown (§9).
- **Evaluation**: `WorkflowEvaluator` is pure and deterministic (§6–§7):
  test failure → candidate, critical review finding → candidate, missing
  artifact → replan, merge conflict → candidate, budget exceeded → replan/
  stop, repeated retry failure → candidate. Severity never auto-executes
  (§5).
- **Proposals**: the planner's output is wrapped as `ProposedReplan` +
  `PlanDiff` (§12, §14) and versioned immutably (v1 → v2 → v3, §13).
- **Gates**: approve (`replan_approve`), reject (`replan_reject` — original
  workflow intact), edit (`replan_edit` — revalidation always follows,
  §16). Completed tasks are preserved (§18); invalidation of completed
  work is explicit + human-approved (§19); artifacts are never deleted
  (§20).
- **Limits**: workflow-level `max_replans` + cooldown (§9, §31); reaching
  the limit blocks and escalates to a human (§32–§33). Autonomy policy is
  `Manual` by default; **Automatic is disabled in Phase 3E** (§34–§37).
- **Security**: the planner cannot raise its own limits/agents/budget or
  bypass approval (§38) — policies are deterministic configuration owned by
  the engine.

## Consequences
- **Positive**: predictable failure detection, auditable immutable plan
  history, a clear human decision surface, loop protection, and a policy
  abstraction ready for future autonomy levels.
- **Negative**: replanning always needs a human in the loop (by design in
  3E); the deterministic evaluator cannot catch failures that leave no
  observable trace.
