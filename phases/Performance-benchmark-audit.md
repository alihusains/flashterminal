# FlashTerminal Performance Benchmark Reliability Audit

The EventBus Output Delivery Fix is complete and verified.

Current EventBus result:

```text
537/1000 output lines lost before fix
0/1000 lost after fix
```

All EventBus-related GitHub jobs are green.

However, the GitHub Performance Check remains intermittently failing on:

```text
input_latency_apply_p95_ms
```

with the same code producing measurements such as:

```text
5.99 ms
8.44 ms
10.93 ms
```

The current CI budget is:

```text
p95 <= 8 ms
```

The code touched by the EventBus fix does not modify:

```text
terminal-core
terminal-parser
```

Therefore this task is NOT to arbitrarily change the budget.

The goal is to determine:

> Is the benchmark measuring a real product regression, benchmark noise, or a poorly isolated measurement?

Do not begin Phase 5.

Do not change the 8 ms budget until the evidence supports doing so.

---

# 1. Reproduce the Variance

Run the performance benchmark repeatedly in a controlled release build.

At minimum:

```text
30 independent runs
```

Record:

```text
p50
p90
p95
p99
max
min
mean
standard deviation
```

Generate a distribution.

Do not report only the passing run.

---

# 2. Compare Local and CI Environments

Record:

```text
OS
OS version
CPU architecture
runner type
Rust version
kernel/system information where useful
CPU load
process count
available memory
```

Compare:

```text
developer machine
GitHub runner
```

Determine whether the GitHub runner is materially noisier.

---

# 3. Determine CPU Contention

While benchmarking, record:

```text
CPU utilization
load average
available CPU
```

Determine whether:

```text
input_latency_apply_p95_ms
```

correlates with CPU contention.

Run:

```text
idle runner
normal runner
artificial background CPU load
```

and compare.

---

# 4. Determine Scheduler Variance

Investigate whether the metric is dominated by:

```text
thread scheduling
PTY scheduling
process scheduling
macOS runner virtualization
```

Do not assume.

Instrument timestamps at each stage.

---

# 5. Instrument the Pipeline

Break:

```text
input_latency_apply_p95_ms
```

into:

```text
input_event
    ↓
PTY write
    ↓
PTY read
    ↓
VT parse
    ↓
state apply
    ↓
render scheduling
    ↓
frame
```

Measure:

```text
input_to_write
write_to_read
read_to_parse
parse_to_apply
apply_to_render
render_to_frame
```

Use monotonic clocks.

Do not add expensive logging to the hot path in normal benchmark mode.

---

# 6. Avoid Measurement Pollution

The benchmark must not significantly alter the workload it is measuring.

Do not:

```text
print every sample
allocate strings per event
synchronously log every timestamp
perform filesystem writes in the hot path
```

Use buffered/in-memory measurement.

---

# 7. Check Benchmark Methodology

Inspect exactly what:

```text
input_latency_apply_p95_ms
```

means.

Document:

```text
start timestamp
end timestamp
what process emits start
what process emits end
what thread records each
```

Determine whether it actually measures:

```text
input → state application
```

or:

```text
input → something later in the pipeline
```

The metric name must accurately describe the measurement.

---

# 8. Separate Product Metrics From Harness Metrics

Create clear categories:

```text
Product performance
Benchmark harness overhead
Environment noise
```

Do not mix them.

For example:

```text
FlashTerminal:
input-to-state latency

Harness:
process coordination overhead
```

---

# 9. Statistical Analysis

Use the 30-run dataset to calculate:

```text
mean
median
p90
p95
p99
standard deviation
coefficient of variation
```

Determine:

```text
noise floor
```

and:

```text
confidence interval
```

Do not choose a CI threshold before seeing the distribution.

---

# 10. Determine Whether 8ms Is Realistic for CI

The conclusion must be evidence-based.

Possible outcomes:

### Outcome A

```text
p95 consistently <8ms
```

Then:

> Keep the 8 ms budget.

### Outcome B

```text
median <8ms
p95 occasionally >8ms
```

and the variance correlates strongly with runner noise.

Then:

> Keep 8 ms as the engineering budget, but redesign the CI gate.

### Outcome C

```text
median consistently >8ms
```

Then:

> Investigate a genuine performance regression.

Do NOT change the budget first.

---

# 11. CI Gate Design

If the benchmark is environment-sensitive, design a CI regression gate based on the baseline distribution.

Consider:

```text
baseline median
baseline p95
allowed variance
```

Possible model:

```text
fail if:
current > baseline_threshold
```

where the threshold is derived from measured variance.

Another possible approach:

```text
fail only after 2 consecutive regressions
```

or:

```text
fail if current exceeds baseline by statistically meaningful amount
```

Choose based on evidence.

---

# 12. Preserve Hard Engineering Budgets

The product budget remains:

```text
input latency p95 <= 8 ms
```

Do not change it unless measurements demonstrate that the target itself is inappropriate.

Distinguish:

```text
engineering target
```

from:

```text
CI regression detection threshold
```

They do not have to be identical.

---

# 13. Add Warmup

Determine whether benchmark runs need:

```text
process warmup
font cache warmup
GPU initialization
allocator warmup
PTY warmup
```

Measure before and after warmup.

Do not hide cold-start regressions accidentally.

Keep separate metrics for:

```text
cold
warm
steady-state
```

---

# 14. Multiple Samples Per Run

Ensure the p95 statistic is based on a sufficiently large sample size.

Report:

```text
sample count
```

along with:

```text
p95
```

A p95 based on only a handful of samples is not meaningful.

---

# 15. Outlier Classification

Do not automatically discard outliers.

Classify them:

```text
legitimate product latency
OS scheduling
runner contention
benchmark artifact
instrumentation overhead
```

Every discarded sample requires an explanation.

---

# 16. Performance Artifacts

Make CI upload:

```text
performance.json
performance.md
raw sample data
environment metadata
```

when the performance job fails.

This will make future failures diagnosable without rerunning blindly.

---

# 17. CI Reproduction

Run the performance workflow:

```text
20 independent CI runs
```

or the maximum practical number.

Determine:

```text
pass rate
failure rate
distribution
```

Do not call the workflow healthy based on one green run.

---

# 18. No Budget Changes Without Evidence

Do NOT:

```text
change 8ms → 10ms
change 8ms → 12ms
remove the performance gate
mark performance "informational"
```

unless the final analysis explicitly demonstrates why the existing gate is statistically invalid.

If the engineering budget remains 8ms, keep it documented.

---

# 19. Regression Test

The final CI design must still detect a real regression.

Create a deliberate benchmark regression, such as an injected artificial delay, and verify:

```text
CI FAILS
```

Then remove the artificial regression and verify:

```text
CI PASSES
```

This proves the gate is not merely being made more permissive.

---

# 20. Documentation

Create:

```text id="ckymxw"
docs/performance-benchmarking.md
```

Document:

* metrics
* instrumentation
* methodology
* warmup
* sample size
* statistical method
* CI threshold
* engineering budget
* environment differences
* artifact handling

---

# 21. Final Report

Return:

## Measurements

Provide the full 30-run local distribution.

## CI Measurements

Provide available GitHub-run distribution.

## Root Cause

Classify variance as:

```text
product
runner
OS
harness
unknown
```

## Metric Definition

Explain exactly what `input_latency_apply_p95_ms` measures.

## Recommendation

Choose exactly one:

```text
KEEP 8MS GATE
REDESIGN CI GATE
INVESTIGATE PRODUCT REGRESSION
```

Do not start Phase 5 automatically.

The objective is:

> **Make FlashTerminal's performance claims measurable, reproducible and scientifically trustworthy.**
