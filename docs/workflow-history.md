# Workflow History (Phase 3E)

Every plan version is immutable and linked: v1 → superseded by v2 →
superseded by v3 (3e.md §13). History is required for auditability.

## Plan versions

`PlanVersion` records:

- `version` — v1, v2, v3 …
- `plan_id` — `plan-vN`
- `goal` + full `ProposedPlan` snapshot
- `superseded_by` — the version that replaced it (`None` = current)
- `created_at`
- `diff_from_previous` — a `PlanDiff` vs the prior version (None for v1)
- `approved` / `approved_at` — the human decision

## Plan diffs (3e.md §14)

`PlanDiff::between(prev, next)` shows:

```text
Added tasks            [+ Investigate failures]
Removed tasks          [- Tests]
Modified tasks         [~ Research (new dependency)]
Changed agents         [(step, old, new)]
Changed dependencies   [(step, old deps, new deps)]
Changed budget         [(old estimate, new estimate)]
```

Example (§14):

```text
Original                  Revised
Implement              →  Investigate failures
   ↓                        ↓
Tests                      Fix database layer
                            ↓
                           Implement
                            ↓
                           Tests
```

## Example timeline (3e.md §30)

```text
Plan v1 approved
Task A completed
Tests failed
Replan requested
Plan v2 proposed
Plan v2 edited
Plan v2 approved
```

Access it with `terminal workflow history` (CLI) / `WorkflowHistory` (IPC),
and view per-replan diffs with `terminal replan show <id>`.

## Invalidation (3e.md §19–§20)

- `TaskInvalidation` — a completed task is explicitly invalidated with
  reason + evidence; normally requires human approval. The task is reverted
  (status + error record the invalidation) so it can be re-run — its result
  is no longer trusted.
- `ArtifactInvalidation` — an artifact is marked invalid (superseded,
  source changed, bad assumption) but its record is **preserved for
  lineage** — never deleted.

Both are visible via `terminal workflow interventions` and survive
restarts (§42).

## Audit trail (3e.md §43)

Every replan event records workflow_id, plan_version, trigger, evidence,
planner/model, proposed changes, approval, approver and timestamp — never
private reasoning.
