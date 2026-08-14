# ADR 0012: Execution Isolation

## Problem
Concurrent coding agents running directly in the workspace working tree
overwrite each other's changes, corrupt user work, and make artifacts
unattributable. FlashTerminal needs a way to run each task in its own
sandboxed copy of the repository without reimplementing containers or
distributed execution.

## Goal
Give every coding task a private, attributable execution environment —
unique directory, unique branch, unique base revision — so parallel agents
cannot cross-contaminate and every change can be traced to one task. The
isolation layer must never discard user work.

## Options Considered
1. **Containers / cloud workspaces**: Rejected (Phase 3C §62 explicitly
   defers container orchestration until safe local isolation is proven).
2. **Single working tree + file locking**: Rejected — locks serialize
   agents, do not prevent conflicting edits, and give no branch-level
   provenance.
3. **`git worktree` per task on its own feature branch**: Selected.
   Git worktrees are cheap, native, offline, and give per-task branches,
   base-revision tracking, and a mergeable artifact for free.

## Decision
- Introduce `ExecutionEnvironment` as the explicit contract between the
  scheduler and `AgentRuntime`: repository, base revision/branch, working
  directory, isolation mode (3c.md §2–§3).
- Introduce `IsolationMode`: `GitWorktree` (default for coding tasks),
  `SharedWorkspace` (explicit policy only, §28), `TemporaryDirectory`.
- `WorktreeManager` is the **only** git caller for worktree operations
  (§44). Neither the planner nor the adapter ever touches git.
- The scheduler resolves the environment *before* the adapter builds the
  launch; the launch cwd is the environment's working directory (§11,
  §44). A hard wrong-cwd guard verifies the launch cwd matches the
  worktree (§12).
- Non-git workspaces degrade gracefully to the shared root with a warning
  — isolation requires a real repository (§13).

## Consequences
- **Positive**: deterministic branch naming (`flash/task/<id>-<slug>`),
  per-task diff vs a recorded base revision, full parallel isolation,
  clean merge/conflict story, offline-first.
- **Negative**: worktrees consume disk; creation latency is a git call
  (kept out of the UI thread, §58–§59); shared-workspace tasks are
  explicitly policy-gated.
- **Safety**: dirty-workspace policy (`AllowDirty`/`RequireClean`) never
  silently discards uncommitted user changes (§14–§15); hostile names are
  sanitized so agent-controlled strings can never escape the repository
  (§34).
