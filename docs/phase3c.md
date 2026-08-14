# Phase 3C — Worktree Isolation + Safe Multi-Agent Execution + Artifact Review

**Spec:** `phases/3c.md` · **Status:** implemented (see §G tests, §J decision)

Phase 3C isolates every coding task in its own `git worktree` on its own
feature branch, captures deterministic diffs at completion, and routes
every completed task through a human review → approval → merge lifecycle.
The fundamental rule (3c.md §2, §54): **completed ≠ approved ≠ merged**.
No merge is ever automatic.

```text
TaskScheduler
   ↓ requests an environment (3c.md §44)
WorktreeManager        ← the only git caller for worktrees
   ↓
ExecutionEnvironment   ← repository, base revision, branch, working dir
   ↓
AgentRuntime
   ↓
AgentAdapter
   ↓
Agent (runs in the worktree on its own branch)
   ↓ completion
TaskResult + DiffSummary (git-generated, never the agent's summary)
   ↓
NeedsReview → Approved/Rejected → explicit Merge → Merged
```

## A. Worktree architecture

Everything lives in `crates/terminal-session/src/worktrees.rs`
(`WorktreeManager`, `ExecutionEnvironment`, `WorktreeRecord`,
`DiffSummary`, `MergeConflict`, policies) and is exposed through the
engine (`crates/terminal-workspace/src/engine.rs`), the IPC surface
(`ipc.rs`), and the CLI (`terminal worktree …`, `terminal task environment
…`). Worktrees are created **before** the adapter builds the launch; the
adapter never decides where an agent runs (3c.md §11). Each worktree gets
a deterministic branch `flash/task/<id>-<slug>[-a<attempt>]`, records its
base revision, and runs under `.git/flashterminal/worktrees/` — never
committed, never in `git status`, never the main working tree.

## B. Isolation

The mandatory parallel-isolation test (3c.md §37) runs **5 tasks
concurrently** (`tests/phase3c::parallel_isolation_five_tasks`), each
writing its own file. Every task ends in `NeedsReview` with a unique
worktree, unique branch, unique agent session, and a diff touching exactly
its own file — nothing from the other four, and the base repo is untouched
by all five. The cross-contamination test (3c.md §38) has two tasks write
the *same filename* in separate worktrees: each sees only its own content.

## C. Review

An isolated task completes → `NeedsReview` (the engine forces
`review_required` for isolated coding tasks). The engine captures
provenance into `TaskResult` — `base_revision`, `result_revision`,
`branch`, `worktree`, and a deterministic `diff_summary` generated from
git, never the agent's summary (3c.md §17–§18). The worktree record moves
to `NeedsReview`. `terminal task review approve|reject` resolves the gate;
approval marks the worktree `Approved` (merge stays separate), rejection
marks it `Rejected` with the worktree preserved for rework (§24).

## D. Conflict handling

`merge_conflict_surfaces_without_data_loss` (3c.md §40) runs two tasks
editing the same line of the same file. Both complete and are approved;
the first merge is clean, the second returns
`MergeConflict { files, branches, base, ours, theirs }` via classic
`git merge-tree` — no partial merge, no data loss, no auto-resolution
(§23). `main` still holds the first task's version.

## E. Persistence

`PersistedState` now carries versioned, secret-free `worktrees` records
(§49). `persistence_restart_reconnect_and_orphan_detection` saves → restores
into a fresh engine: the worktree reconnects to its task through the
restored scheduler's worktree ids (§50) and is **not** orphaned; a record
whose owner is dangling is surfaced as an orphan — never deleted (§31).

## F. Real-agent validation

Not run in standard CI (no real agent credentials). The 5-task parallel
isolation, cross-contamination, merge, conflict, cancellation, retry, and
recovery tests all run the deterministic `fake-agent` (which now commits
its worktree changes like a real coding agent) against real disposable git
repositories — never the real FlashTerminal repo (3c.md §56). See
`tests/phase3c/`.

## G. Tests

- `tests/phase3c/` (11 tests): creation + branch naming + base revision,
  dirty-workspace `RequireClean` refusal (typed error, user work
  untouched), **5-task parallel isolation**, cross-contamination, review
  gate, merge (main gains A, not B, then B), merge conflict, cancellation
  preservation, fresh-worktree retry, persistence + restart + orphan
  detection, path-traversal-safe branches, and secret-free
  metadata/diff/persistence.
- IPC: `socket_worktree_surface` round-trips the whole worktree
  request/response surface.
- Full workspace suite: **all green** (see gates).

## H. Security

- Hostile branch/worktree names are sanitized; agent-controlled strings
  can never escape the repository directory (§34).
- Worktree records, `TaskResult` provenance, diffs, and persisted state
  never contain file content or credentials — verified by the
  secret-safety test (a secret sentinel lives in the worktree file but in
  nothing the engine persists or surfaces, §35).
- The wrong-cwd guard (`assert_cwd`) hard-verifies the launch cwd matches
  the worktree before spawn (§12); `RequireClean` refuses to isolate on a
  dirty repo rather than silently discard user work (§14–§15).

## I. Known limitations

- No container/cloud sandboxing (explicitly deferred, §62); git worktree
  isolation is a safety *layer*, not a security boundary.
- Merge conflicts are surfaced, never auto-resolved (per spec).
- Worktree git operations are synchronous engine calls (documented;
  the desktop may move them to background tasks in a later phase, §59).
- Real-agent validation (§56–§57) requires credentials and a disposable
  repo; not part of standard CI.

## J. Decision

```text
READY FOR PHASE 3D
```
