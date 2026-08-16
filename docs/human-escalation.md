# Human Escalation (Phase 3E)

When the system cannot safely determine what to do, it stops and asks a
human — never hiding uncertainty (3e.md §33).

## When escalation fires

- The workflow reached its replan limit (`max_replans`) and another replan
  was requested (§31–§32).
- The evaluator cannot safely continue (e.g. budget exceeded with no
  recovery path).
- Any situation where the deterministic engine has no safe next step.

## What a human sees

A `HumanEscalation` record shows:

```text
What happened      ← why automation stopped
What was attempted ← what the engine already tried
What evidence      ← signals/artifacts that led here
What options       ← the available next steps (increase limit, manual
                     intervention, …)
```

This is also published as the `HumanEscalation` application event and is
listed by `terminal workflow interventions`.

## Policy

- Escalations are **records**, not silent background events — the workflow
  does not continue automatically after one.
- Escalation records are persisted (§42) and appear in the intervention
  history.
- The replan limit is deterministic configuration: the planner cannot raise
  it (§38), so an escalation is always the result of engine-owned policy,
  never planner fiat.

See also `docs/replanning.md` (loop protection) and
`docs/adaptive-orchestration.md`.
