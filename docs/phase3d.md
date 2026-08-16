# Phase 3D Report — Multi-Agent Collaboration

## A. Collaboration architecture

Artifact-mediated, user-gated collaboration — no direct agent messaging
(3d.md §2):

```text
Task
 ↓
Agent
 ↓
Artifact            ← engine-stamped, redacted, bounded payload
 ↓
Dependent Task      ← declares input_artifacts; readiness-gated (§8–§9)
 ↓
Agent               ← materialized grant in its own worktree (§11)
 ↓
Review (independent reviewers → deterministic consensus)
 ↓
Synthesis           ← explicitly selected results + artifacts, §14
 ↓
Human approval / replan signal → human replan (§44–§45)
```

Implemented across:

- `crates/terminal-session/src/artifacts.rs` — `ArtifactStore`,
  `ArtifactSelector`, `ArtifactLineage`, `ArtifactAccessPolicy`,
  `ArtifactMaterializer`, retention, `artifact://` references.
- `crates/terminal-session/src/collaboration.rs` — `ReviewFinding`,
  `ReviewReport`, `ReviewPolicy`, `ReviewAggregator`, `ResultSynthesizer`,
  `SynthesisResult`, `SynthesisProvenance`.
- `crates/terminal-session/src/orchestration.rs` — structured
  `TaskResult`, `TaskErrorKind::ArtifactMissing`, readiness gate.
- `crates/terminal-workspace/src/engine.rs` — artifact registration,
  materialization-before-spawn, review/synthesis/replan APIs, persistence,
  new `ApplicationEvent`s.
- IPC (`ipc.rs`) + CLI (`apps/cli`): `artifact …`, `review …`,
  `synthesize …`, `replan …`, `task …` output artifact support.

## B. Artifact lineage

```text
task A (Architecture) ──produce──► artifact:… (docs/architecture.md)
                                      │
                                      └─consume─► task B (Implementation)  §11
```

`ArtifactLineage` exposes `producers` (artifact → task), `consumers`
(artifact → dependent tasks), `task_outputs` (task → artifacts), built
purely from the store + graph — never from agent claims. Locked in by
`artifact_lineage_maps_producers_and_consumers`.

## C. Access control

A task may only consume artifacts it **explicitly declared** in
`input_artifacts` (3d.md §39–§40). `ArtifactAccessPolicy::can_access`
consults the task graph (declared inputs + dependency edges); the engine
skips un-granted artifacts during materialization, and
`ArtifactSelector` applies the same policy to visibility queries. Verified
by `access_control_denies_unrelated_tasks` (C has no grant → C's agent
cannot read A's file → task fails deterministically) and
`missing_input_artifact_blocks_task` (missing grant → `Blocked`, never
silently continued).

## D. Review synthesis

Independent reviewers → deterministic consensus (3d.md §20–§21, §55):

```text
Reviewer A: PASS      ─┐
Reviewer B: WARNING   ─┤──► NeedsReview   (any Fail / High findings)
Reviewer C: FAIL      ─┘
```

Replace C with PASS (no blocking findings) → `ApprovedCandidate`.
Every fired rule is recorded in `explanations` (§30). Verified by
`structured_results_and_review_consensus` +
`review_consensus_all_pass_is_approved_candidate`.

## E. Real-agent workflow

An end-to-end artifact handoff runs the deterministic `fake-agent`
fixtures through the **same PTY runtime as real agents** (Phase 2B):
`cross_worktree_consumption` creates task A (writes `report.txt` in its
own worktree), approves A's review, then task B declares A's artifact,
runs in its **own** worktree, and its agent output proves it read the
materialized content (`CONSUMED report.txt: research findings`) while an
`ArtifactConsumed` event fires on the bus. The replan path
(`human_replan_builds_new_plan_requiring_approval`) exercises the full
planner pipeline with a mock provider behind the real `PlannerProvider`
trait.

## F. Cross-worktree validation

`ArtifactMaterializer` writes a granted artifact's payload into the
consumer's worktree at the artifact's relative path — no shared filesystem
assumed. Task B's agent runs with its own cwd/worktree, reads the
materialized file, and exits cleanly; the consumer's worktree never
touches the producer's. The access-denial test proves the inverse: without
a grant, nothing is materialized and the read fails.

## G. Security

- `artifact_payloads_are_redacted`: the agent writes a registered secret
  (`sk-3d-super-secret`) into a file; the artifact payload is redacted and
  the persisted artifact records are secret-free (§38, §35).
- `access_control_denies_unrelated_tasks`: cross-task reads without a
  grant fail deterministically (§39–§40).
- Payloads are bounded (`ArtifactStore` budget) and never travel on the
  event bus (§27) — events carry metadata only.

## H. Performance

| Metric | Value |
|--------|-------|
| Artifact count | 12-test suite registers dozens of artifacts (one per changed file per task) |
| Context size | Synthesis input = explicitly selected results + artifact **metadata** (payloads never join the event bus) |
| 20-task workflow | Not benchmarked this phase (scheduler scales linearly; see Phase 3B) |
| Event throughput | Unchanged — new events are metadata-only and coalesced by the existing bus policy |
| RSS | Unchanged — artifacts are bounded in-memory records; payloads capped per artifact |

## I. Known limitations

1. **Default synthesizer is mechanical** — deterministic aggregation, not
   prose. LLM synthesis behind the same contract is deferred (§60).
2. **Artifact payloads are optional** — files exceeding the payload budget
   register metadata-only; consumers must read them from the producer's
   worktree path when the store lacks a payload.
3. **Review consensus is verdict-driven** — it flags blocking signals but
   does not itself gate the worktree lifecycle; the human review boundary
   (`resolve_task_review`) remains the authoritative gate (§29).
4. **No global artifact search/embedding** — deliberately (§50).
5. **Replan requires the planner provider to be reachable** — with no
   provider configured, `replan_workflow` errors clearly instead of
   degrading silently.
6. **`process_exit_is_tracked` is occasionally load-flaky** in parallel
   CI runs (shell exit-timing); it passes deterministically in isolation —
   a pre-existing engine test, unrelated to Phase 3D.

## J. Decision

```text
READY FOR PHASE 3E
```
