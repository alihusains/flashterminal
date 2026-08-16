# Result Synthesis (Phase 3D)

After a multi-agent workflow completes, the user needs one coherent answer
about what happened. Result synthesis (3d.md §14–§15, ADR 0015) combines
an **explicitly selected** set of task results and artifacts into a single
structured `SynthesisResult`.

## Inputs

`SynthesisInput` carries only:

- `task_results` — the structured `TaskResult`s of the selected tasks
- `artifacts` — the selected artifact records
- `plan_id` / `workflow_id` — optional provenance context

The engine's `synthesize(plan_id, workflow_id, task_ids, artifact_ids)`
resolves the ids from the authoritative store. The synthesizer **never**
receives the whole project history (§25, §14).

## Output

`SynthesisResult` fields:

- `overall_status` — one-line status
- `summary` — deterministic summary of combined results
- `completed_work` / `remaining_work` — structured lists
- `warnings` / `failures` — extracted from structured task results
- `recommendations` — from task results' `recommendations`
- `artifacts` — the artifact ids this synthesis actually references
- `provenance` — full `SynthesisProvenance` (§34): `input_task_ids`,
  `input_artifact_ids`, model, provider, timestamp, plan/workflow ids

## Determinism & anti-hallucination

- The default `ResultSynthesizer` is **pure and auditable** — no LLM in
  the loop, so CI can test it deterministically.
- It references **only** artifact ids it was given. A hallucinated id is
  **rejected with an error**, never silently dropped (§54) — locked in by
  the test `synthesis_references_all_inputs_and_rejects_unknown`.
- An empty input set is an error, not an empty success.
- No hidden chain of thought (§16): output is structured and concise.

## Provenance

Every synthesis records where its facts came from — which tasks, which
artifacts, which model/provider (when an LLM synthesizer is plugged in
later), and when it ran. No raw credentials, no private reasoning.

## Future

An LLM synthesizer can implement the same contract behind the engine API;
the deterministic default remains for tests and headless use (§60 defers
richer synthesis to a later phase).
