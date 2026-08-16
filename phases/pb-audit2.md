# FlashTerminal Performance Benchmark Audit

## Reconcile Existing Work, Correct the Metric, and Redesign the CI Gate

A previous performance-benchmark audit is already in progress.

Do NOT overwrite or discard the existing work.

The new investigation has produced a major finding:

> `input_latency_apply_p95_ms` does not actually measure input latency.

The current evidence indicates:

```text
Current CI metric:
input_latency_apply_p95_ms

Actual behavior:
The benchmark resets its stopwatch once per ~200-line batch containing ~113,000
synthetic events.

Therefore the reported p95 is effectively batch processing progress /
CPU throughput, not keyboard/input-to-terminal latency.
```

Independent measurements against the real system show:

```text
Actual PTY write → read → parse → apply:
~0.9 ms p95

Actual shell echo round-trip:
~1.5 ms p95
```

The current CI number has varied approximately:

```text
5.99 ms
8.44 ms
10.93 ms
```

on substantially unchanged code.

The current engineering target remains:

```text
true input latency p95 <= 8 ms
```

Do NOT raise this to 10 ms or 12 ms merely to make CI pass.

The likely correct outcome is:

> REDESIGN CI GATE

while preserving 8 ms as the actual engineering target.

Your task is to reconcile all existing performance-audit work, verify the finding rigorously, replace the incorrect metric with correct measurements, and redesign CI so it detects genuine regressions without failing because of an invalid benchmark.

Do NOT start Phase 5.

---

# 1. Reconcile Existing Work FIRST

Before changing anything:

Inspect all existing performance-audit work:

```text
phases/Performance-benchmark-audit.md
docs/ci-forensics.md
docs/performance-benchmarking.md
benchmarks/src/main.rs
benchmarks/
.github/workflows/
```

Also inspect any uncommitted performance-related changes.

Do NOT overwrite another agent/session's work.

Create a reconciliation report if necessary:

```text
docs/performance-audit-reconciliation.md
```

Document:

```text
existing hypothesis
new finding
evidence supporting each
conflicts
final conclusion
```

The newest evidence must not automatically win. Verify it against the source.

---

# 2. Verify the Current Metric Definition

Inspect the exact implementation of:

```text
input_latency_apply_p95_ms
```

Identify:

```text
timer start
timer stop
sample boundaries
batch boundaries
event count
aggregation
p95 calculation
```

Document the actual measurement in plain language.

If the metric is really:

> cumulative processing time for a batch

rename it.

Do not leave an incorrectly named performance metric in the product.

---

# 3. Remove Semantic Ambiguity

Separate these metrics:

```text
Input latency
State application latency
Batch processing time
Render preparation latency
Frame presentation latency
Throughput
```

Do not use one number for several different concepts.

Create explicit metric names.

For example:

```text
input_to_pty_write_ms
pty_write_to_read_ms
read_to_parse_ms
parse_to_apply_ms
input_to_apply_ms
input_to_visible_ms
state_batch_processing_ms
events_per_second
render_prepare_ms
frame_time_ms
```

Use names that describe exactly what is being measured.

---

# 4. Define the True Input Latency Metric

The actual product metric should be:

> Time from a user input event being accepted by FlashTerminal until the corresponding terminal state becomes observable.

Where practical, distinguish:

```text
input_to_apply
```

from:

```text
input_to_visible
```

The strongest product metric is:

```text
input_to_visible_ms
```

If actual pixel presentation cannot be measured reliably in headless CI, document that limitation and use the closest defensible proxy.

Do NOT label a proxy as "input latency" if it is not actually input latency.

---

# 5. Build a Real PTY Input Probe

Create a deterministic probe using a real PTY.

The sequence should be:

```text
Synthetic keyboard/input event
        ↓
FlashTerminal input handling
        ↓
PTY write
        ↓
child shell
        ↓
PTY read
        ↓
VT parse
        ↓
terminal state update
```

Record timestamps at each stage.

At minimum measure:

```text
input_to_pty_write
pty_write_to_read
read_to_parse
parse_to_apply
input_to_apply
```

Use a monotonic high-resolution clock.

---

# 6. Build a Real Shell Echo Probe

Use a deterministic shell command or tiny helper process that echoes a unique token.

Example conceptual flow:

```text
send token
→ shell/process receives token
→ writes token back
→ PTY reader observes token
```

Measure:

```text
input timestamp
response-visible timestamp
```

Run enough samples for meaningful p50/p95/p99 statistics.

This should become a permanent regression benchmark.

---

# 7. Keep the Synthetic Throughput Benchmark

The existing 113,000-event benchmark is still useful.

But rename it to something accurate, such as:

```text
state_batch_processing_p95_ms
```

or:

```text
batch_apply_throughput
```

It should no longer masquerade as input latency.

This metric is still valuable for:

* state engine efficiency
* multi-agent scalability
* CPU regression detection

---

# 8. Define Separate Performance Categories

The benchmark suite should expose at least:

## Interactive latency

```text
input_to_apply_ms
input_to_visible_ms where measurable
shell_echo_roundtrip_ms
```

## Terminal processing

```text
parse_latency_ms
state_apply_latency_ms
batch_apply_latency_ms
```

## Rendering

```text
render_prepare_ms
frame_time_ms
```

## Throughput

```text
events_per_second
pty_bytes_per_second
```

## Resource usage

```text
RSS
CPU
allocations
```

Do not collapse these into one generic performance number.

---

# 9. Collect a Real Local Distribution

Run approximately:

```text
30 independent benchmark runs
```

for the true latency metric.

Record:

```text
min
p50
p90
p95
p99
max
mean
standard deviation
sample count
```

Do this for:

```text
input_to_apply
shell_echo_roundtrip
batch_processing
```

Do not report only p95.

---

# 10. Compare With GitHub

Collect available GitHub run history.

For each performance run record:

```text
run ID
commit
runner
OS
Rust version
p50
p95
p99
max
pass/fail
```

Separate:

```text
real input latency
```

from:

```text
batch-processing metric
```

Do not compare incomparable metrics.

---

# 11. CPU Contention Experiment

Run the real benchmark under:

```text
idle
normal
background CPU load
```

Measure how:

```text
true input latency
batch processing
```

respond differently.

The hypothesis is:

```text
true interactive latency
≈ stable

batch CPU processing
≈ sensitive to runner contention
```

Verify rather than assume.

---

# 12. Pipeline Timestamp Instrumentation

Create a low-overhead trace mode.

For one sample:

```text
input
 t0

PTY write
 t1

PTY read
 t2

parse
 t3

state apply
 t4

render scheduling
 t5
```

Then:

```text
input_to_write = t1 - t0
write_to_read = t2 - t1
read_to_parse = t3 - t2
parse_to_apply = t4 - t3
input_to_apply = t4 - t0
```

If visible-pixel measurement is possible:

```text
input_to_visible = t6 - t0
```

Do not enable tracing in normal performance runs unless explicitly requested.

---

# 13. Validate the Existing 0.9 ms / 1.5 ms Findings

Reproduce:

```text
~0.9 ms p95
```

for:

```text
PTY write → read → parse → apply
```

and:

```text
~1.5 ms p95
```

for:

```text
real shell echo round-trip
```

Do not simply quote the earlier results.

Reproduce them.

---

# 14. Test Under Multi-Pane Load

Measure real input latency with:

```text
1 pane
5 panes
10 panes
20 panes
```

and:

```text
idle
moderate output
heavy output
agent-like output
```

For each measure:

```text
input_to_apply p95
shell_echo p95
batch processing p95
```

This is especially important because FlashTerminal's product claim depends on staying responsive under agent workloads.

---

# 15. Test Under Multi-Agent Load

Use the fake-agent framework.

Measure:

```text
1 agent
5 agents
10 agents
20 agents
```

while sending interactive input to a focused terminal.

The actual engineering gate remains:

```text
true input latency p95 <= 8 ms
```

---

# 16. Keep the 8 ms Engineering Target

The current engineering target remains:

```text
input latency p95 <= 8 ms
```

Do not change it.

The investigation should determine whether CI can reliably measure that target.

---

# 17. Redesign CI Gate

If GitHub's shared runner cannot produce a stable pixel/input-latency measurement, do NOT use the flawed batch metric as a proxy.

Instead:

### Engineering gate

Use real controlled benchmarks where possible:

```text
input_to_apply p95 <= 8 ms
shell_echo p95 within accepted target
```

### CI regression gate

Use a statistically robust regression check against baseline.

Possible approaches:

```text
current median vs baseline median
current p95 vs baseline p95
relative regression threshold
absolute regression threshold
multiple-run confirmation
```

Do not choose the exact threshold until the distribution is known.

---

# 18. CI Must Still Detect Real Regressions

This is mandatory.

Create an intentional benchmark regression.

For example, inject a controlled delay into the benchmark-only path.

Verify CI:

```text
FAILS
```

Then remove it.

Verify CI:

```text
PASSES
```

This proves the redesigned gate is actually protective.

---

# 19. Do Not Use Benchmark Noise as an Excuse

The goal is NOT:

```text
make CI always green
```

The goal is:

```text
detect genuine performance regression
without treating environmental variance as regression
```

Document this distinction clearly.

---

# 20. Preserve Current Batch Metric

Keep the synthetic batch metric.

Rename it accurately and continue tracking it.

For example:

```text
batch_apply_p95_ms
events_per_second
```

This is valuable for:

* EventBus
* terminal state
* agent workloads
* orchestration throughput

It is simply not user input latency.

---

# 21. Update Performance Baseline

Do not overwrite existing `benchmarks/baseline.json` blindly.

Create a new versioned baseline if necessary:

```text
benchmarks/baseline-v2.json
```

with clearly named metrics.

Include:

```json
{
  "input_to_apply_p95_ms": 0,
  "shell_echo_p95_ms": 0,
  "batch_apply_p95_ms": 0,
  "events_per_second": 0
}
```

Use actual measured values.

---

# 22. Update CI Artifacts

On every performance failure, upload:

```text
performance.json
raw samples
environment metadata
benchmark logs
timestamp traces where applicable
```

This should make a future failure explainable without reproducing it manually.

---

# 23. Update Documentation

Create or update:

```text
docs/performance-benchmarking.md
docs/ci-forensics.md
docs/performance.md
docs/architecture-current.md
```

Explicitly document:

```text
old metric
why it was wrong
new metrics
measurement methodology
engineering targets
CI thresholds
environment limitations
```

---

# 24. Final Verdict

The audit must finish with exactly one:

```text
KEEP 8MS GATE AS-IS
```

or:

```text
KEEP 8MS ENGINEERING TARGET, REDESIGN CI GATE
```

or:

```text
ACTUAL PRODUCT SLOWDOWN, INVESTIGATE/FIX
```

Based on the current evidence, the working hypothesis is:

```text
KEEP 8MS ENGINEERING TARGET, REDESIGN CI GATE
```

but do not finalize that conclusion until the 30-run distribution and GitHub comparison are complete.

---

# 25. Definition of Done

The audit is complete when:

```text
✓ Existing work reconciled
✓ Incorrect metric identified
✓ Metric renamed/corrected
✓ Real PTY latency measured
✓ Shell echo latency measured
✓ Stage timings implemented
✓ 30-run local distribution collected
✓ GitHub distribution collected
✓ CPU contention tested
✓ Multi-pane tested
✓ Multi-agent tested
✓ 8 ms engineering target evaluated
✓ CI gate redesigned if necessary
✓ Intentional regression test proves CI detects regressions
✓ Baseline versioned
✓ CI artifacts improved
✓ Documentation updated
✓ No product behavior unnecessarily changed
```

Do NOT start Phase 5 until the performance benchmark system is trustworthy.
