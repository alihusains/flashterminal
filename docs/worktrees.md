# Worktrees

`WorktreeManager` (`crates/terminal-session/src/worktrees.rs`) is the only
git caller for worktree operations in FlashTerminal. The planner never
touches git; the scheduler never creates worktrees (3c.md §44–§45).

## Creating a worktree

`WorktreeManager::create` (through `environment_for_task`):

1. Enforces the worktree budget (default 32, `policy.max_worktrees`).
2. Validates the repository and the base revision (§13) and applies the
   dirty-workspace policy (`RequireClean` refuses to isolate on a dirty
   repo — never discards user work, §14–§15).
3. Builds a deterministic, collision-resistant branch:
   `flash/task/<sanitized-task-id>-<slug>[-a<attempt>]` (sanitized so
   hostile strings cannot escape, §7, §34).
4. Runs `git worktree add -b <branch> <path> [base]` inside the
   repository's own git dir
   (`.git/flashterminal/worktrees/<worktree-id>`), so worktrees are never
   committed and never appear in `git status`.
5. Records versioned, secret-free metadata: id, owner task, path,
   repository, branch, base revision/branch, timestamps, state (§9, §49).

Worktree ids are stable per (task, attempt): `wt-<hash>` on attempt 1,
`wt-<hash>-a2` on retries — so retries can create a fresh worktree while
reusing the same task identity (§10, §43).

## States

```
Created → Active → Completed → NeedsReview → Approved/Rejected → Merged
```

- `NeedsReview` — the agent finished; a human must review (3c.md §19, §54).
- `Approved` — the human accepted the artifact; merge is still a separate,
  explicit step (§21).
- `Rejected` — the worktree stays available for reopen/retry/discard (§24).
- `Merged` — integrated into the target branch.
- Failure/cancellation moves the worktree to `Completed` (preserved, with
  changes intact, §30, §42).

Only valid actions are surfaced per state (`allowed_actions`, §52).

## Diffs

`WorktreeManager::diff` produces a deterministic `DiffSummary` between the
recorded base revision and the worktree's current state (numstat +
name-status + untracked). It is generated from git — never from the
agent's own summary (§18). File *names* only; never content, so secrets
cannot leak through diffs (§35).

## Merge

`WorktreeManager::merge(id, target)`:

1. Only from `Approved` (typed error otherwise, §22).
2. Runs classic `git merge-tree <base> <ours> <theirs>` — deterministic
   conflict detection without touching the working tree.
3. Conflict → `MergeConflict { files, branches, base, ours, theirs }` with
   no data loss and no auto-resolution (§23, §40).
4. Clean → `git merge --no-ff <feature>` into the target branch; the
   record moves to `Merged`.

Never automatic: `Completed` ≠ `Approved` ≠ `Merged` (§54).

## Cleanup, orphans, recovery

- `remove(id)` — explicit discard (user action), removes the worktree,
  its branch, and prunes (§30).
- `cleanup()` — policy sweep (`Keep` default; then after merge/reject/
  cancel). Never touches unowned work.
- `scan(ownership)` — restart-time reconnection. `ownership` maps
  worktree-id → task-id from the restored scheduler; worktrees with no
  live owner are flagged orphaned and surfaced for review, never deleted
  (§31, §50).
- Persisted records (id, path, branch, base revision, task, state,
  timestamps) round-trip through `PersistedState.worktrees` (§49).

## Security

- Branch names and worktree ids are sanitized; agent-controlled strings
  can never escape the repository directory (§34).
- Records, `TaskResult` provenance, diffs, events and persistence never
  contain credentials or file content (§35).
