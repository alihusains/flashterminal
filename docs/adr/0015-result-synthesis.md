# ADR 0015: Result Synthesis

## Problem
After a multi-agent workflow completes, the user needs one coherent answer:
what was done, what remains, and what to do next. Naively concatenating
agent outputs produces contradictions and hallucinated claims — and a
synthesizer given free rein over the whole project history can reference
artifacts that were never produced.

## Goal
Produce a deterministic, auditable synthesis over an explicitly selected
set of task results and artifacts — never the entire project history — and
guarantee it references only artifacts it was actually given (3d.md §14,
§54).

## Options Considered
1. **LLM-only synthesis over full history**: Rejected — unbounded context
   (§25), hallucination risk (§54), no audit trail.
2. **Concatenate raw agent outputs**: Rejected — no structure, no
   provenance, no "remaining work" reasoning.
3. **Deterministic `ResultSynthesizer` over explicitly selected
   `SynthesisInput`**: Selected. Pure function of the inputs; an LLM
   synthesizer can slot in behind the same trait later.

## Decision
- `SynthesisInput` carries only the explicitly selected `task_results` +
  `artifacts` (plus optional plan/workflow ids) — the engine's
  `synthesize()` API takes `task_ids` + `artifact_ids` and resolves them
  from the store (§14).
- `ResultSynthesizer::synthesize` is deterministic: it aggregates
  structured `TaskResult` fields (metrics, warnings, errors,
  recommendations, files changed), and refuses to synthesize from an empty
  input set.
- `SynthesisResult` records `artifacts` (ids actually referenced) and full
  `SynthesisProvenance` — `input_task_ids`, `input_artifact_ids`, model,
  provider, timestamp, plan/workflow ids (§34).
- Hallucinated artifact ids are **rejected with an error**, never silently
  dropped (§54) — the test `synthesis_references_all_inputs_and_rejects_unknown`
  locks this in.
- No hidden chain of thought: synthesis output is structured
  (`overall_status`, `summary`, `completed_work`, `remaining_work`,
  `warnings`, `failures`, `recommendations`) with no private reasoning
  (§16).

## Consequences
- **Positive**: deterministic, testable, auditable; provenance makes every
  claim traceable; no hallucinated ids can leak into output.
- **Negative**: the default synthesizer is mechanical (no prose flair) —
  richer synthesis requires an LLM provider behind the same contract,
  deferred to a later phase (§60).
