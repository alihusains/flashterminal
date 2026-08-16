# ADR 0014: Artifact Lineage

## Problem
In a multi-agent workflow, one task's output becomes another task's input
(`docs/architecture.md` produced by an architecture agent feeds an
implementation agent). Without a record of who produced what, results are
unattributable, cross-task handoffs are ad-hoc file copies, and the user
cannot answer "where did this come from?" or "what depends on this?".

## Goal
Make every artifact attributable and traceable: who produced it, from which
task/agent/worktree/revision, and which downstream tasks consume it. The
lineage must be derived from the authoritative store and task graph — never
from agent claims (3d.md §2, §5).

## Options Considered
1. **Raw file paths as references**: Rejected — paths collide across
   worktrees, do not survive branch/merge, and carry no producer identity.
2. **Agent-reported metadata**: Rejected — an agent can claim anything;
   provenance must be engine-stamped (3d.md §4).
3. **Engine-stamped `Artifact` records + structured `artifact://` ids,
   with lineage derived from the store and graph**: Selected. The engine
   stamps `created_by_task`/`created_by_agent`/`workspace_id`/`worktree`/
   `revision`/`created_at_ms` at registration time, and `ArtifactLineage`
   is a pure function of the store + `TaskGraph`.

## Decision
- Every artifact gets an `artifact://` structured id (`new_artifact_id`),
  never a bare path (3d.md §10). Raw paths are rejected at the API
  boundary by `resolve_artifact_ref` unless explicitly treated as
  workspace-relative inputs.
- `Artifact` carries engine-stamped provenance fields (§4): kind, path,
  description, `created_by_task`, `created_by_agent`, `workspace_id`,
  `worktree`, `revision`, `created_at_ms`, and key/value `metadata`.
- `ArtifactLineage` exposes three views (§5): `producers` (artifact id →
  producing task id), `consumers` (artifact id → dependent task ids), and
  `task_outputs` (task id → its output artifact ids). It is built by
  traversing the authoritative store + graph (`ArtifactLineage::build`),
  so it can never be forged by an agent.
- Tasks declare inputs via `input_artifacts` (explicit, §8); the selector
  (`ArtifactSelector`) filters by task/referenced-by/kind using the same
  lineage so unrelated tasks cannot see each other's artifacts (§39–§40).

## Consequences
- **Positive**: full attribution for every handoff, deterministic lineage
  tests, selector + access control share one source of truth, clean
  restart recovery (lineage survives via persisted artifact records).
- **Negative**: lineage is only as complete as the registration path —
  artifacts must be registered at completion (engine does this in
  `register_task_artifacts`), and payloads are bounded/redacted (§27, §38).
