# Replanning (Phase 3E)

How a replan moves from signal to approved, executed change — and how it
can never happen without a human.

## Signal → proposal → approval

```text
ReplanSignal (§4)            ← WorkflowEvaluator or ManualUserRequest
      │
ReplanContext (§10)          ← bounded workflow state for the planner
      │
PlannerRequest (mode=Replan) ← §11
      │
ProposedReplan (§12)         ← reason, changes, new/removed/modified tasks,
                               dependencies, agent recommendations,
                               estimated cost, warnings
      │
PlanValidator                ← the SAME gates as an initial plan (§21)
      │
Human decision (§15)
   ├─ Approve  → replan_approve  → graph migration (§17), task preservation (§18)
   ├─ Edit     → replan_edit     → revalidate (§16), then approve
   └─ Reject   → replan_reject   → original workflow intact (§28)
```

## The review surface (3e.md §15)

```text
Workflow needs replanning
Reason: 14 tests failed
Evidence: test-report #123
Proposed changes:
  + Investigate database layer
  + Fix migration handling
  ~ Re-run implementation
  ~ Re-run tests
Estimated additional cost: $0.81
[Approve] [Edit] [Reject]
```

Nothing executes on proposal — the deterministic engine stays
authoritative until a human approves.

## Replan edits (3e.md §16, §29)

The user can add/remove tasks, change agents, change dependencies. After
any edit, `PlanValidator` **must** run again — an edited plan is never
executed without re-validation (test: `replan_edit_requires_revalidation`
rejects an edit pointing at an unavailable agent).

## Replan execution (3e.md §17–§18)

Approval applies the proposal through the normal `compile_for_execution`
path. **Completed historical work is preserved**: completed tasks remain
completed unless explicitly invalidated (§19) — a new plan retains
`Research ✅` without re-running it. Only affected portions of the graph
change.

## Rejection (3e.md §28)

Rejecting a replan leaves the original workflow, tasks, worktrees and
artifacts exactly as they were — verified by
`rejected_replan_leaves_workflow_intact`.

## Loop protection (3e.md §31–§32)

`max_replans` (default 5) caps replans per workflow. Reaching the limit
blocks further replans, records a human escalation, and surfaces
"Workflow requires human intervention" — no `failure → replan → failure →
replan` loop runs unattended. Verified by `replan_limit_blocks_and_escalates`.

## Autonomy policy (3e.md §34–§37)

`Manual` (every replan requires approval — the default), `Assisted` (the
system prepares a replan and highlights it), and `Automatic` (**future
capability — disabled in Phase 3E**). The policy is deterministic
configuration owned by the engine; the planner cannot raise its own
autonomy, limits, agents, or budget (§38).
