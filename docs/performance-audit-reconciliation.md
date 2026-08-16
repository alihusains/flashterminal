# Performance Audit Reconciliation

`phases/pb-audit2.md` arrived after `phases/Performance-benchmark-audit.md` was already investigated, fixed, committed, and pushed (`3d41f51`) by a parallel session on this same repo. This document reconciles the two before any further change, per `pb-audit2.md` §1's explicit instruction not to overwrite or discard that work.

## Existing hypothesis and what was already done (commit `3d41f51`)

`docs/performance-benchmark-audit.md` (already committed) independently reached the same root cause pb-audit2.md states as its "new finding":

- `measure_input_p95()` reset its timer once per ~200-line batch (~113,000–800,000 pooled samples per run across 200 batches), not per event — a recorded sample was cumulative batch-progress time, not an isolated event's latency.
- The metric was compared against the 8ms "keypress to pixel" engineering budget (`docs/performance.md`) despite never touching a PTY, keypress, or render stage — a category mismatch independent of the timing bug.
- Fixed: `t0` now resets immediately before every `apply_event` call. Post-fix value: **~42 nanoseconds**, reproducibly identical under deliberate 14×-oversubscribed CPU contention.
- CI gate for this metric changed from an absolute `current > 8.0ms` cutoff to baseline-relative (`current > baseline × 5`, floor `0.001ms`).
- `benchmarks/src/bin/staged_latency_probe.rs` was added and run: real PTY write→read→parse→apply, 500 samples, `p95 = 0.856ms` total (`write_to_pty_read` p95 0.744ms dominating over `read_to_apply_visible` p95 0.161ms).
- A regression-test proof (inject a 50µs/event delay, confirm gate fails; remove it, confirm gate passes) was run and documented.
- `benchmarks/baseline.json` was updated in place to the corrected value, under the **same metric name** (`input_latency_apply_p95_ms`).

## What pb-audit2.md asks for beyond that

Verified against the source (not assumed): the existing fix corrects the *timer bug* and *CI gate mechanism* for one metric, but does **not**:

1. **Rename the metric.** It is still called `input_latency_apply_p95_ms` even though — by the existing audit's own documented conclusion — it measures only the isolated `TerminalState::apply_event` cost, never a PTY, keypress, or render stage. The name remains a category-mismatched label on an otherwise-correct measurement. pb-audit2.md's core new instruction (§2–§3, §7) is exactly this: *do not leave an incorrectly named performance metric in the product* — a naming defect distinct from (and not fixed by) the timing-bug fix.
2. **Wire the real PTY/echo probes into CI as tracked, named metrics.** `staged_latency_probe.rs` and the pre-existing `echo_probe.rs` are standalone binaries, run manually, not part of `--ci`'s `Report`/`baseline.json`/`budget_table`. pb-audit2.md §5–§6 wants `input_to_apply_ms` and `shell_echo_roundtrip_ms` as permanent, CI-tracked regression benchmarks.
3. **Full metric taxonomy** (§3, §8): separate, explicitly named metrics for input latency, state-apply latency, batch throughput, render prep, and events/sec — today there is one renamed-in-place metric plus two disconnected standalone probes.
4. **Multi-pane load test** (§14) and **multi-agent load test** (§15) — not attempted by the existing audit.
5. **Versioned baseline** (§21) — `baseline.json` was overwritten in place with a corrected value under the old name; no `baseline-v2.json` with the new taxonomy exists.
6. **CI artifact/documentation breadth** (§22–23) — `docs/performance.md` was touched minimally (4 lines); `docs/ci-forensics.md` and `docs/architecture-current.md` were not touched for this metric at all.

## Conflicts

None found. Both audits agree on the root cause and the recommendation (`REDESIGN CI GATE`, keep the 8ms engineering target). pb-audit2.md is additive — it extends an already-correct fix rather than contradicting it. The newest instructions do not automatically win by being newest; they were checked against `3d41f51`'s actual diff and found to be requesting real, unimplemented follow-on work, not a re-litigation of settled findings.

## Final conclusion

Proceed as an **extension**, not a redo:

- Keep the existing timer-bug fix and baseline-relative gate for the synthetic batch metric — just rename it (`batch_apply_p95_ms`) to stop it from masquerading as input latency, per §7/§20.
- Add real, CI-tracked `input_to_apply_p95_ms` and `shell_echo_p95_ms` metrics (promoting the existing standalone probes into the main `--ci` report), each gated against the actual 8ms engineering target — which they clear by roughly an order of magnitude.
- Complete the remaining unimplemented sections (multi-pane, multi-agent, versioned baseline, doc breadth) below.

See `docs/performance-benchmarking.md` for the resulting design and `docs/performance-benchmark-audit.md` for the original forensic evidence (unchanged, still authoritative for what it covers).
