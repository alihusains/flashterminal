# Review Findings & Consensus (Phase 3D)

Phase 3D introduces multi-agent review: independent reviewers examine a
completed task's result and file findings, and a deterministic policy
aggregates their verdicts into a single consensus the user can trust.

## Findings

A `ReviewFinding` (3d.md §18) is a first-class artifact:

- `id` — structured `finding:…` id
- `severity` — `Info` / `Low` / `Medium` / `High` / `Critical` (§21)
- `file` / `line` — optional location
- `finding` — the human-readable issue
- `evidence` — optional reference to the artifact/evidence it is based on
- `created_by_task` — the reviewer that filed it
- `created_at_ms` — engine timestamp

Findings are registered as artifacts (kind `Document`, metadata
`finding` + `severity`), so they show up in the artifact explorer and
survive restart.

## Reports

A `ReviewReport` is one reviewer's verdict on a task: `Pass`, `Warning`,
or `Fail`, plus its findings and a concise structured `reason`. Reports are
**independent**: reviewers are stored per-task, and no report can mutate
another (§19).

## Consensus

`ReviewAggregator::aggregate(reports, policy)` is a pure, deterministic
function — the LLM never votes secretly (§20). The default
`ReviewPolicy` ladder (3d.md §21):

| Condition | Overall |
|-----------|---------|
| Any `Critical` finding | `Critical` |
| Any `Fail` verdict | `NeedsReview` |
| `High` findings ≥ threshold | `NeedsReview` |
| Findings at `Medium` or below | `Warning` |
| Otherwise (all pass, no significant findings) | `ApprovedCandidate` |

Every rule that fired is recorded in `explanations`, so the user can
always answer "why did this land in NeedsReview?" (§30).

Example (3d.md §55): A=`PASS`, B=`WARNING`, C=`FAIL` → `NeedsReview`.
Replace C with `PASS` (and no blocking findings) → `ApprovedCandidate`.

## Review gate

The workflow may require a minimum number of reviews before a result is
promoted (§22). Consensus is *candidate* status: `ApprovedCandidate`
means the deterministic policy sees no blockers — the human review
boundary still applies (§29, see `worktrees.md` review lifecycle).
