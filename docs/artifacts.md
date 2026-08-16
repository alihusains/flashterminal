# Artifacts (Phase 3D)

Artifacts are the unit of handoff between tasks in FlashTerminal's
multi-agent workflows. This document describes what an artifact is, how it
is created, how it is referenced, and how access is controlled. See also
`agent-collaboration.md` for the end-to-end collaboration flow.

## What is an artifact?

An artifact is a piece of work produced by a task: a changed file, a
report, a design document, a review finding. Every artifact carries
**engine-stamped provenance** (3d.md §4) — never agent-controlled:

| Field | Meaning |
|-------|---------|
| `id` | Structured `artifact://…` identifier (3d.md §10) |
| `kind` | `CodeChange`, `Document`, `Url`, `Report` … |
| `path` | Relative path in the producer's worktree (absent for `Url`) |
| `description` | Short human-readable summary |
| `created_by_task` | The task that produced it |
| `created_by_agent` | The agent definition that ran that task |
| `workspace_id` | The workspace it was produced in |
| `worktree` | The producer's worktree path |
| `revision` | The commit the artifact was produced at |
| `created_at_ms` | Engine timestamp |
| `metadata` | Key/value facts (e.g. `finding`, `severity`) |

## Creation

When a task completes, the engine runs `register_task_artifacts`
(3d.md §3):

1. Repository artifacts are derived from the **deterministic git diff**
   (`files_changed` / `files_created` / `files_deleted`), never from the
   agent's own claims.
2. Payloads are read from the producer worktree, **bounded** to the store
   limit and **redacted** through the global `Redactor` (§38) — artifacts
   must never become a secret-leak path.
3. A metadata-only `ArtifactCreated` event is published (§27); payloads
   never travel on the event bus.

## References

Task inputs are declared as `artifact://` references in
`input_artifacts` (3d.md §8). The engine resolves them with
`resolve_artifact_ref`; raw paths are only honored when explicitly
workspace-relative. The `ArtifactReference` type parses/validates
references so a malformed id cannot slip through.

## Lineage

`ArtifactLineage` (see ADR 0014) provides `producers`, `consumers`, and
`task_outputs` views. It is a pure function of the authoritative store +
task graph, so it can never be forged.

## Selection & access control

`ArtifactSelector` filters the store by task, referenced-by, kind, and
workspace. `ArtifactAccessPolicy::can_access` (3d.md §39–§40) enforces
that a task may only consume artifacts it explicitly declared as inputs —
an unrelated task sees nothing. The selector applies the same policy, so
"what can C see?" and "what did C read?" agree.

## Storage & retention

`ArtifactStore` keeps metadata + bounded redacted payloads with a
configurable retention policy (default `Keep` — never delete work results,
§37). Payloads are bounded per artifact; oversized files are registered
metadata-only.

## Cross-worktree consumption

`ArtifactMaterializer` (3d.md §11) writes an artifact's payload into the
consumer's own worktree at the artifact's relative path. It never assumes
a shared filesystem between producer and consumer. The engine materializes
all granted inputs into the task's execution environment **before** the
agent spawns, and publishes `ArtifactConsumed` when a grant is satisfied.

## Persistence

Persisted `ArtifactRecord`s (metadata + redacted payload) survive restart
via `PersistedState.artifacts` (§35–§36); lineage and review reports are
rebuilt from the same records on restore.
