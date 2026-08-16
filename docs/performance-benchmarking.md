# Performance Benchmarking Methodology

How FlashTerminal measures performance, what each number actually means, why the CI gate is designed the way it is, and how to reproduce every claim in this document yourself. See `docs/performance-benchmark-audit.md` for the original forensic investigation and `docs/performance-audit-reconciliation.md` for how a follow-up audit (`phases/pb-audit2.md`) extended it — this document reflects the extended, current state.

## Metric taxonomy

`input_latency_apply_p95_ms` used to be one name covering two different things: a real (if unmeasured) product goal and a synthetic microbenchmark that never touched a PTY. It has been split into explicitly-named metrics, each measuring exactly what its name says:

| Metric | What it measures | Touches a PTY? | Gate |
|---|---|---|---|
| `input_to_apply_p95_ms` | Real input latency: `session.write()` → the terminal state reflecting it (`applied_batches` incrementing), via a live PTY-backed shell session | Yes | **8ms engineering budget** |
| `write_to_pty_read_p95_ms` | Sub-stage of the above: write → the reader thread first observing the echoed byte (an `OutputTap` firing) | Yes | informational |
| `read_to_apply_p95_ms` | Sub-stage of the above: raw-byte-read → VT parse + state apply complete | Yes | informational |
| `shell_echo_p95_ms` | Real shell tty-echo round trip — heavier than `input_to_apply` (includes the shell's own echo processing) | Yes | **8ms engineering budget** |
| `batch_apply_p95_ms` | Synthetic in-memory VT-parse-then-apply throughput (formerly `input_latency_apply_p95_ms`) — **not input latency**; see below | No | baseline-relative (5×) |
| `events_per_second` | Throughput companion to `batch_apply_p95_ms` | No | informational |

`input_to_visible_ms` (input → actual pixel presentation) is **not measured** — this repository has no headless-GUI-capable CI runner to submit a real GPU frame and observe it (§18/§19 of `docs/ci-forensics.md` — desktop UI validation is explicitly deferred to a future manual/GUI-runner workflow, not proxied here). `input_to_apply_p95_ms` is the closest defensible proxy and is labeled as exactly that, not as "input to visible."

`cargo run --release -p benchmarks -- --ci` runs all of these in one process (see `benchmarks/src/main.rs::main`), alongside the pre-existing memory/render/parse microbenchmarks (`idle_ram_mb`, `parse_1m_lines_ms`, `snapshot_frame_us`, etc. — unchanged by this audit, see `docs/ci.md`).

### `batch_apply_p95_ms` (formerly `input_latency_apply_p95_ms`)

**What its old name and `docs/performance.md`'s engineering budget claimed it measured**: "Keypress to pixel change on screen" — the full input pipeline (input event → PTY write → PTY read → VT parse → state apply → render → frame).

**What it actually measures**: the 95th percentile of one isolated `TerminalState::apply_event` call's cost, over 200 batches of ~2,000 synthetic lines each (`t0` resets before every event since the original timing-bug fix — see `docs/performance-benchmark-audit.md` § Metric Definition for why a per-batch reset previously made this cumulative-batch time instead of per-event time). There is no PTY, no keypress, no render, no GPU submission anywhere in this function.

**Renamed, not just fixed**: even with the timing bug fixed, a name claiming "input latency" on a function with zero I/O is a real defect — a future reader could reasonably assume this number reflects real interactive responsiveness, which it never has. `batch_apply_p95_ms` is still valuable (state-engine/CPU regression detection, agent-workload throughput), just under its own accurate name, with its own baseline-relative gate (§ below) — not compared against the 8ms input-latency budget at all anymore.

### `input_to_apply_p95_ms` / `shell_echo_p95_ms`

Real, PTY-backed measurements (`measure_input_to_apply_p95`/`measure_shell_echo_p95` in `benchmarks/src/main.rs`, modeled on the standalone `benchmarks/src/bin/staged_latency_probe.rs` and `echo_probe.rs` probes but promoted into the CI-tracked `--ci` report so every run — local or CI — produces a real, gated number, not just a manually-run investigation artifact). 300 samples each per run; warmup (30 drain cycles) before measuring; monotonic clocks; `f64::NAN` (reported as `UNAVAILABLE`, never a failure) if no shell/PTY is available.

Measured values (development hardware, unloaded): `input_to_apply_p95_ms` ≈ 0.68–0.98ms, `shell_echo_p95_ms` ≈ 1.3–2.2ms — both comfortably under the 8ms budget (roughly 4–12× margin depending on load). Under deliberate 14×-oversubscribed CPU contention: `input_to_apply_p95_ms` rose to ~1.6–2.0ms, `shell_echo_p95_ms` to ~1.8–2.2ms — real but modest sensitivity, never approaching the budget. See `docs/performance-audit-reconciliation.md` for the full contention comparison.

## Multi-pane and multi-agent load

`benchmarks/src/bin/multiplex_bench.rs` and `agent_stress.rs` already implement real multi-pane (1/5/10/20/50 panes) and multi-agent (1/5/10/20 agents) scenarios with focused-pane input-latency measurement — reused rather than duplicated for this audit. Representative results: 20 panes with a 5/5/5/5 flood mix — focused input p95 1.34ms, p99 1.64ms; 10 concurrent agents (mixed workload) — focused input p95 0.06ms (occasional outlier up to ~90ms attributable to OS scheduling stalls, not a systematic budget breach — see the wedge-classification protocol in `docs/benchmark-reliability.md`); agent flood (10 agents × 1,000 lines/sec) — focused input p95 1.49ms. All comfortably under the 8ms budget across every tested scale.

## Warmup

`measure_input_to_apply_p95`/`measure_shell_echo_p95` each run 30 drain cycles against the live shell before sampling begins (prompt/rc-file settling). `batch_apply_p95_ms` runs after `sessions_ram_mb(1)`, `sessions_ram_mb(10)`, and both `pipeline_ms` calls (the 10M-line parse alone takes single-digit seconds) — the process is already fully warmed (allocator, CPU caches, code paths) by the time it runs; no separate warmup step is needed for it specifically.

## Statistical methodology

30 independent local runs of the pre-fix `batch_apply_p95_ms` (then `input_latency_apply_p95_ms`) produced: `min=3.708 p50=3.741 p90=3.826 p95=3.847 p99=712.791 mean=27.381 stdev=129.453` — 29/30 clustered tightly, one discrete ~190× outlier traced to a real ~3.5-minute OS scheduling stall (not a gradual noise tail). Full dataset and computation: `docs/performance-benchmark-audit.md` § Measurements. The corrected metrics (`input_to_apply_p95_ms`, `shell_echo_p95_ms`) were validated across multiple independent runs (unloaded and under deliberate contention) rather than a single sample — see § above.

## Environment differences

Local development hardware (Apple M4 Pro, 12 cores, 24GB) and GitHub's hosted `macos-latest` runner are not equivalent machines. `docs/performance-benchmark-audit.md` § CI Measurements has the specific historical comparison. Any CI gate calibrated purely against local numbers risks being either too tight (false failures from real environment noise) or too loose (masking a real regression a faster local machine wouldn't reveal) — this is exactly why `batch_apply_p95_ms` uses a baseline-relative gate rather than an absolute one, and why `input_to_apply_p95_ms`/`shell_echo_p95_ms` are given generous margin (8ms against a sub-2ms real measurement) rather than a tight cutoff.

## Engineering budget vs. CI regression gate

These are deliberately two different things and are not required to be equal:

- **Engineering budget** (`docs/performance.md`): the product target FlashTerminal is designed to meet — input latency p95 < 8ms. Unchanged by either audit; no evidence ever demonstrated it was the wrong target, only that the originally-tracked benchmark measured a different, unrelated pipeline stage.
- **CI regression gate** (`benchmarks/src/main.rs::budget_table()`): the mechanism that fails a build, and it is *not* the same threshold as the engineering budget for every metric:
  - `batch_apply_p95_ms` uses a baseline-relative gate (current > baseline × 5, floor 0.001ms) since an absolute millisecond-scale cutoff has zero discriminating power against its true nanosecond scale.
  - `input_to_apply_p95_ms`/`shell_echo_p95_ms` are gated directly against the real 8ms engineering budget **on local runs** (where they clear it with wide margin, ~0.7–2ms even under deliberate contention) but against a wider, evidence-based `25ms` CI-only ceiling (`INPUT_LATENCY_CI_CEILING_MS`) on GitHub's runner — 3 reproduced real CI runs on unchanged code showed `5.17–13.57ms`, roughly 5–10× local, a genuine environment characteristic of the shared macOS runner's real PTY I/O, not a product regression or a measurement defect. See `docs/performance-audit-reconciliation.md` § Addendum for the evidence and the regression-test proof that the wider ceiling still catches a real regression.

## Versioned baseline

`benchmarks/baseline.json` is the live file CI compares against — all metrics, updated in place on every local (non-`--ci`) run, never overwritten by CI itself. `benchmarks/baseline-v2.json` is a point-in-time, clearly-labeled snapshot of just the new/corrected metric taxonomy (`input_to_apply_p95_ms`, `write_to_pty_read_p95_ms`, `read_to_apply_p95_ms`, `shell_echo_p95_ms`, `batch_apply_p95_ms`, `events_per_second`), for anyone auditing the rename/redesign later without needing to diff the full live baseline's history.

## Regression-test proof

Both gates were proven to actually detect a regression, not just pass permissively:

- `batch_apply_p95_ms`: injected `std::thread::sleep(50µs)` per `apply_event` call (~1,190× the ~42ns baseline) → `ERROR: hard budget breached`. Removed → `All hard budgets met.` (`docs/performance-benchmark-audit.md` § Regression test.)
- `input_to_apply_p95_ms`: injected `std::thread::sleep(12ms)` per sample right after `session.write()` → measured value jumped to 15.05ms, gate correctly reported `FAIL` (`input_to_apply_p95_ms 15.05 0.68 8.00 ms FAIL`). Removed → measured value returned to 0.69ms, `All hard budgets met.`

## Artifacts

On any Performance Check failure, CI uploads `perf_report.txt` and `docs/performance-report.md` as build artifacts (`.github/workflows/ci.yml`) — a failure is diagnosable from the artifact without re-running blindly.

## Reproducing this

```bash
# N independent local runs, release build, extracting the metrics each run
for i in $(seq 1 30); do
  cargo run --release -p benchmarks -- --ci 2>&1 | sed -n '/^{$/,/^}$/p'
done
```

Compute p50/p90/p95/p99/mean/stddev across the extracted values. Compare against a deliberately CPU-loaded machine (e.g. several `while true; do :; done &` busy-loops) to gauge contention sensitivity — kill them afterward (`pkill` by pattern; a plain `jobs -p` in a fresh shell won't see processes backgrounded in a different shell invocation).
