# Agent Collaboration (Phase 3D)

Phase 3D turns a set of isolated tasks into a collaborating workflow:
artifacts flow between tasks, reviewers validate results, a synthesizer
summarizes the outcome, and the user stays in control of every promotion
and replan.

## Collaboration model

FlashTerminal deliberately does **not** implement direct agent messaging,
agent debate, or agent-to-agent sockets (3d.md §2, §60). Collaboration is
**artifact-mediated** and **user-gated**:

1. A task completes → its output artifacts are registered with
   engine-stamped provenance (see `artifacts.md`).
2. A dependent task **declares** the artifacts it needs (`input_artifacts`,
   §8). The engine materializes granted artifacts into its own worktree
   **before** the agent spawns (§11) and gates readiness on them (§9).
3. Reviewers file findings and verdicts; the deterministic policy produces
   a consensus (see `review-findings.md`).
4. The user approves/rejects at the review boundary (§29), and can trigger
   a **human replan** from the signals a failing workflow leaves behind
   (§44–§45).

## Readiness gating

A task that declares a missing artifact is **Blocked** with a typed error
(`TaskErrorKind::ArtifactMissing`), never silently continued. The
scheduler's readiness pass consults the artifact store before allowing a
task to start.

## Replanning signal

A task failure produces a structured replan signal (`cause`, `detail`,
`timestamp`) — never an autonomous replan loop (§60). The signal feeds the
human replan path:

- `signal_replan(cause, detail)` records the signal.
- `replan_workflow(goal)` builds a fresh `PlannerRequest` from the current
  graph/results/artifacts, runs it through the normal planner pipeline, and
  lands in `NeedsApproval` — the new plan requires the same validate +
  approval gates as any other plan (§45). Signals addressed by a replan are
  cleared.

## Collaboration surface

- **Artifact explorer** (§47): `terminal artifact list|get|select|lineage`
  over IPC.
- **Review surface**: `terminal review record|consensus|reports <task>`.
- **Synthesis**: `terminal synthesize <task-ids> [artifact-ids]`.
- **Replan**: `terminal replan <goal>`.
- **Timeline** (§48): artifact creation/consumption, findings, and replan
  signals all flow as `ApplicationEvent`s on the unified event bus
  (`ArtifactCreated`, `ArtifactConsumed`, `ReviewFindingCreated`,
  `ReplanSignaled`, …).

## Security

- Access control: a task may only read artifacts it declared (§39–§40);
  the selector enforces the same policy.
- Secrets never reach artifact payloads or persisted records: payloads are
  bounded + redacted at registration (§38), and `PersistedState` keeps only
  secret-free metadata.
- Agent memory boundary (§41): each agent sees only its own worktree +
  granted artifacts.

## See also

- `artifacts.md` — creation, lineage, selection, storage
- `review-findings.md` — findings, reports, consensus policy
- `result-synthesis.md` — deterministic synthesis + provenance
- `worktrees.md` — the isolation layer artifacts are produced in
