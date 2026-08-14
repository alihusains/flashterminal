# Execution Environments

Phase 3C introduces an explicit **execution environment** layer between the
task scheduler and the agent runtime (3c.md §2–§3, §44):

```text
TaskScheduler
   ↓  requests an environment
WorktreeManager
   ↓  creates / reuses / re-creates
ExecutionEnvironment
   ↓  launch cwd + branch + base revision
AgentRuntime
   ↓
AgentAdapter
   ↓
Agent
```

The adapter never decides where an agent runs. The scheduler/environment
layer decides; the adapter only builds the vendor instruction.

## What an environment contains

`ExecutionEnvironment` (in `terminal_session::worktrees`) carries:

- `repository` — the base repository path (engine-owned, trusted).
- `base_revision` / `base_branch` — the commit/branch the task was created
  from (3c.md §8).
- `working_directory` — the directory the agent actually runs in (the
  worktree path for isolated tasks).
- `worktree_id` / `branch` — the manager's record id and the feature
  branch, when isolated.
- `isolation` — `GitWorktree` (default for coding tasks), `SharedWorkspace`
  (explicit policy only), or `TemporaryDirectory`.
- `environment_variables` — non-secret additions for the launch (never
  persisted, §35).

## Isolation modes

| Mode | Where the agent runs | Default? |
| --- | --- | --- |
| `GitWorktree` | Dedicated `git worktree` under `.git/flashterminal/worktrees/`, on its own `flash/task/<id>-<slug>` branch | Yes for coding tasks |
| `SharedWorkspace` | The repository working tree directly | No — requires `requires_shared_workspace` + a warning |
| `TemporaryDirectory` | A throwaway temp dir | No |

## How an environment is resolved

The engine's `resolve_execution_environment` (one call per task spawn):

1. Reads the task's isolation mode, slug, existing worktree id, and the
   workspace repository.
2. For `GitWorktree`: verifies git is available and the workspace is a
   real repository (graceful fallback to the shared root with a warning
   otherwise, 3c.md §13).
3. Calls `WorktreeManager::environment_for_task` — **reuses** the task's
   worktree on attempt 1, and on retries either reuses or creates a fresh
   worktree per the explicit `RetryWorktreePolicy` (default: fresh, §10,
   §43).
4. The launch cwd is set to the environment's `working_directory`, and a
   hard wrong-cwd guard (`assert_cwd`) verifies the launch before spawn
   (3c.md §12).

## Previewing before execution

`Multiplexer::task_environment_preview` (CLI: `terminal task environment
<task-id>`) renders the 3c.md §29 preview: repository, base `main @ <rev>`,
isolation, branch, agent, and working directory — so a user always knows
where an agent will work before it starts.

## Policy wiring

The authoritative `TaskPolicy` drives the worktree manager's knobs:
`policy.dirty` (dirty-workspace policy), `policy.retry_worktree`, and
`policy.max_worktrees` (budget) are synced into `WorktreeManager` on
`set_task_policy`. A plan can never raise them (3c.md §46–§47).
