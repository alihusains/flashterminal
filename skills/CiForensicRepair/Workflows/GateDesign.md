# GateDesign — is a flaky performance/benchmark gate noise, a bad measurement, or a real regression?

## Ideal state

A completed gate-design pass produces an evidence-based answer to exactly one question — *is this gate's variance product, runner, OS/scheduler, or harness* — and one of exactly three outcomes, chosen only after the evidence supports it:

- **Keep the gate as-is**, because the measured distribution is consistently under threshold.
- **Redesign the CI gate** (not the engineering target) — e.g. compare against a baseline distribution's variance, or require N consecutive breaches instead of one sample — because the metric is real but environment-sensitive.
- **Investigate a genuine regression**, because the *median*, not just the tail, moved and no environment/harness explanation accounts for it.

The engineering budget (what the product is supposed to achieve) and the CI regression-detection threshold (what fails a build) are named as two separate numbers, and only one of them may need to change.

## Constraints

- Get the distribution before forming an opinion: repeat the benchmark enough times (dozens, not a handful) to compute p50/p90/p95/p99/mean/stddev — a p95 from a handful of samples is not a p95.
- Compare environments with facts, not assumption: OS/version, CPU architecture, runner type, load average, process count, available memory, local vs CI — actually gathered, not guessed.
- If instrumenting the pipeline stage-by-stage to find where the variance lives, use cheap, in-memory, monotonic-clock timestamps — instrumentation that itself perturbs the workload invalidates the measurement it's trying to take.
- Classify every outlier (product latency, OS scheduling, runner contention, harness artifact, instrumentation overhead) — a discarded sample with no stated reason is a thumb on the scale.
- Prove any new gate design actually still catches a regression: inject a deliberate, temporary performance regression, confirm the gate fails, then remove it and confirm the gate passes. A gate that's only ever been made more permissive hasn't been proven to still work.
- State plainly what the metric's name promises vs. what it actually measures (e.g. does "input latency" really mean input-to-state-apply, or does it silently include something later in the pipeline?) — a mislabeled metric is itself a measurement flaw, independent of the variance question.

## Tools

- The project's benchmark binary/harness, run repeatedly in a real release build, locally and (via reruns) on the actual CI runner
- Whatever the platform provides for environment introspection (OS/kernel info, CPU count, load average) on both sides of the comparison
- The CI's artifact-upload mechanism, so a failing run's raw samples/environment metadata survive for later diagnosis instead of requiring a blind rerun

## Output-format contract

The final recommendation must be exactly one of: `KEEP GATE`, `REDESIGN CI GATE`, or `INVESTIGATE PRODUCT REGRESSION` — stated plainly, backed by the distribution and environment comparison, with the engineering budget and the CI threshold called out as distinct numbers even if they end up equal.
