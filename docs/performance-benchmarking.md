# Performance Benchmarking Methodology

How FlashTerminal measures performance, what each number actually means, why the CI gate is designed the way it is, and how to reproduce every claim in this document yourself. See `docs/performance-benchmark-audit.md` for the full forensic investigation that produced this design.

## Metrics: what each one actually measures

`cargo run --release -p benchmarks -- --ci` runs a fixed sequence of measurements in one process (see `benchmarks/src/main.rs::main`), in this order:

1. `idle_ram_mb` / `ten_panes_ram_mb` — real PTY child shells spawned in-process; RSS of the harness process while holding 1 / 10 live `terminal_session::Session`s. **Harness metric, not desktop-app RSS** — see `docs/ci.md`.
2. `parse_1m_lines_ms` / `parse_10m_lines_ms` — best-of-3 in-memory VT parse + state-apply of a synthetic 1M/10M-line colored buffer. No PTY, no I/O.
3. `input_latency_apply_p95_ms` — see below; this is the metric this document's audit is about.
4. `snapshot_frame_us`, `render_prep_10k_rows_ms`, `glyph_raster_us_per_glyph`, `scrollback_10k_rows_ms`, `grid_10m_cells_mb`, `unicode_write_ns_per_char` — further in-memory microbenchmarks, no PTY/GPU involvement.

### `input_latency_apply_p95_ms`

**What its name and `docs/performance.md`'s engineering budget claim it measures**: "Keypress to pixel change on screen" — i.e. the full input pipeline (input event → PTY write → PTY read → VT parse → state apply → render → frame).

**What `measure_input_p95()` actually measures** (`benchmarks/src/main.rs`): for 200 iterations, generate a synthetic 2000-line colored buffer, feed it through `Parser::advance_bytes` in memory, then for every resulting VT event call `state.apply_event(e)` and record `Instant::now() - t0` where **`t0` is set once per 200-line batch, not per event**. There is no PTY, no keypress, no render, no GPU submission anywhere in this function — it is a pure VT-parse-and-apply microbenchmark. Full audit and the numeric proof: `docs/performance-benchmark-audit.md` § Metric Definition.

**The consequence**: because `t0` resets only once per batch, an event's recorded "latency" is really *cumulative time since the start of its batch, including every earlier event's apply cost in that same batch* — not that event's own isolated apply time. Later-in-batch events are structurally biased toward higher recorded values regardless of any actual per-event cost change, and the aggregate p95 mixes samples from different positions in different batches. This is a genuine harness/measurement-methodology defect, not benchmark noise or a product regression, and it is fixed as of this audit — see the "Marginal" vs "Original" comparison in `docs/performance-benchmark-audit.md`.

**Warmup**: `measure_input_p95()` runs after `sessions_ram_mb(1)`, `sessions_ram_mb(10)`, and both `pipeline_ms` calls (the 10M-line parse alone takes single-digit seconds) — by construction the process is already fully warmed (allocator, CPU caches, code paths) by the time this metric runs. No separate warmup step is needed for this specific metric.

## Statistical methodology

A single benchmark process run produces one `input_latency_apply_p95_ms` value (itself a p95 over ~hundreds of thousands of in-process samples accumulated across 200 batches). To characterize the *run-to-run* variance of that value — the thing that actually causes CI flakiness — this audit ran the benchmark process itself N times independently and computed p50/p90/p95/p99/mean/stddev/coefficient-of-variation **across those N run-level p95 values**. See `docs/performance-benchmark-audit.md` § Measurements for the full dataset and computed statistics.

## Environment differences

Local development hardware and GitHub's hosted `macos-latest` runner are not equivalent machines — different CPU generation/architecture, core count, and load profile. Any CI gate calibrated purely against local numbers risks being either too tight (false failures from real environment noise) or too loose (masking a real regression a faster local machine wouldn't reveal). See `docs/performance-benchmark-audit.md` § CI Measurements / environment comparison for the specific numbers.

## Engineering budget vs. CI regression gate

These are deliberately two different things:

- **Engineering budget** (`docs/performance.md`): the product target FlashTerminal is designed to meet — currently input latency p95 < 8ms, unchanged by this audit unless the evidence demonstrated it was wrong.
- **CI regression gate**: the mechanism that fails a build. `docs/performance.md` §3.2 already documented the *intended* design — an allowed-regression-from-baseline threshold (+1ms), not a flat absolute cutoff — but the implementation in `benchmarks/src/main.rs::budget_table()` never actually used baseline-relative comparison for this metric; it used a flat `current > 8.0` check identical in shape to the RAM budgets. This audit's recommendation (§ below) reconciles the implementation with the originally documented intent, backed by the measured distribution rather than by re-reading the old doc alone.

## Artifacts

On any Performance Check failure, CI uploads `perf_report.txt` and `docs/performance-report.md` as build artifacts (`.github/workflows/ci.yml`) — a failure is diagnosable from the artifact without re-running blindly.

## Reproducing this audit

```bash
# N independent local runs, release build, extracting one metric's value each run
for i in $(seq 1 30); do
  cargo run --release -p benchmarks -- --ci 2>&1 | sed -n '/^{$/,/^}$/p'
done
```

Compute p50/p90/p95/p99/mean/stddev across the extracted `input_latency_apply_p95_ms` values. Compare against a deliberately CPU-loaded machine (e.g. `yes >/dev/null &` a few instances) to gauge contention sensitivity.
