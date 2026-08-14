# ADR 0013: Worktree Lifecycle

## Problem
A completed task's changes must not flow into `main` automatically.
FlashTerminal needs a deterministic lifecycle from "agent finished" to
"changes merged" that keeps a human in the loop at every destructive or
permanent step.

## Goal
Define and enforce the artifact lifecycle — completed → review → approval
→ merge — with worktree preservation on failure/cancellation and no
automatic deletion or merging.

## Options Considered
1. **Auto-merge on completion**: Rejected — "completed" means the agent
   finished, not that changes are approved (3c.md §54).
2. **Delete worktrees on failure/cancellation**: Rejected — silently
   destroying user-visible work is unacceptable (§30, §42).
3. **Explicit review + merge lifecycle with preserved worktrees**:
   Selected. A completed isolated task lands in `NeedsReview`; approval
   accepts the artifact (not a merge); merge is a separate explicit step;
   failure/cancellation preserves the worktree for reopen/retry/discard.

## Decision
- `WorktreeState` tracks `Created → Active → Completed → NeedsReview →
  Approved/Rejected → Merged` (§9, §19–§24).
- Isolated coding tasks always require review (`review_required`) — a
  completed task never auto-merges (§54).
- `WorktreeManager.merge()` only runs from `Approved`; conflicts surface
  as `MergeConflict` (files/branches/base/ours/theirs) with no data loss
  and no auto-resolution (§22–§23).
- Cleanup is policy-driven (`Keep` default, then after merge/reject/cancel)
  and never touches unowned work (§30).
- Records persist versioned, secret-free metadata (§49) and reconnect to
  tasks through the restored scheduler on restart; worktrees with no live
  owner are surfaced as orphans — never deleted (§31, §50).

## Consequences
- **Positive**: a trustworthy merge story (human-approved merges only),
  recoverable interruptions, auditable lineage (task → worktree → commit →
  artifact → review → merge, §26).
- **Negative**: an extra human step before integration; worktrees persist
  until explicitly discarded; merge conflicts require manual resolution.
- **Safety**: rejection keeps the worktree available for rework (§24–§25);
  cancellation preserves changes (§42); retries use a fresh worktree per
  the explicit policy (§10, §43).
