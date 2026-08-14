# Review and Merge

Phase 3C keeps a human in the loop at every permanent step. A completed
coding task **never** merges automatically (3c.md §19, §54).

```text
Running
   ↓ agent finishes
NeedsReview        ← the review gate (scheduler `TaskStatus::NeedsReview`)
   ↓ human: approve | reject
Approved / Rejected
   ↓ (approved) explicit merge
Merged
```

## The review gate

- An isolated coding task always requires review — the engine sets
  `review_required` when it resolves a `GitWorktree` environment, so the
  scheduler transitions the task to `NeedsReview` on completion (§19,
  §54).
- At completion the engine captures deterministic provenance into the
  scheduler's `TaskResult` (§17–§18): `base_revision`, `result_revision`,
  `branch`, `worktree` path, and the git-generated `diff_summary` (never
  the agent's summary). The worktree record moves to `NeedsReview`.
- `terminal task review <approve|reject> <task-id>` (IPC
  `TaskResolveReview`) resolves the gate:
  - **Approve** → task `Completed`, worktree `Approved`.
  - **Reject** → task `Failed`, worktree `Rejected` — the worktree and its
    changes stay available for reopen/retry/discard (§24–§25).

## Merge (separate, explicit)

`WorktreeManager::merge(id, target)` (CLI: `terminal worktree merge
<worktree-id> [target-branch]`):

- Requires the worktree to be `Approved` — a typed error otherwise (§22).
- Conflict detection is deterministic and side-effect free (classic
  `git merge-tree`); a conflict returns `MergeConflict { files, branches,
  base, ours, theirs }` with **no data loss and no auto-resolution**
  (§23, §40).
- A clean merge runs `git merge --no-ff` and marks the record `Merged`.

## Merge conflict example

Task A and Task B both edit `auth.ts` in separate worktrees. Both complete
and are approved. Merging A is clean; merging B afterwards returns:

```text
merge conflict — no changes were made:
  ! auth.ts
```

`main` still holds A's version untouched — nothing was partially merged
and no agent was asked to resolve the conflict automatically (§40).

## Cancellation and failure

- Cancelling a running isolated task stops the agent and marks the task
  `Cancelled`; the worktree and its changes are preserved (§42).
- A failed task keeps its worktree at `Completed` — the user can reopen,
  retry (fresh worktree per policy), or discard (§30, §43).

## Lineage

Every artifact is traceable: **task → worktree → commit → artifact →
review → merge** (§26). The worktree record carries the owner task id, the
`TaskResult` carries the branch/worktree/base, and persisted state
round-trips both so lineage survives restarts (§49–§50).
