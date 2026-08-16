# Benchmark Reliability

Phase 4 (phases/4.md §34–§39) makes the benchmark suite a release gate,
not a smoke test. This document records the reliability evidence and the
known stress risk.

## Multiplexer benchmark — 20 consecutive release runs (§37)

Ran `multiplex_bench` in release mode, 25 s workload, 20 consecutive
runs (2026-08-15):

```text
run  1: PASS    run  6: PASS    run 11: PASS    run 16: PASS
run  2: PASS    run  7: PASS    run 12: PASS    run 17: PASS
run  3: PASS    run  8: PASS    run 13: PASS    run 18: PASS
run  4: PASS    run  9: PASS    run 14: PASS    run 19: PASS
run  5: PASS    run 10: PASS    run 15: PASS    run 20: PASS

20-run success rate: 20/20
timeouts:           0
hangs:              0
inner FAIL lines:   0
```

Every run emitted the `DONE` marker with exit 0. No logs were discarded —
all 20 run logs are preserved.

### Observed metrics (representative)

```text
workspace create: 100x avg ~2.5 ms     (<100 ms target: PASS)
tab create:       50x avg ~2.5 ms      (<100 ms target: PASS)
pane split:       49x avg ~2.7 ms      (<30 ms target: PASS)
stress 20 panes:  input p95 1.10 ms    (<8 ms target: PASS) · echo timeouts 0
orchestration scaling: PASS per row (input p95 < 8 ms, echo timeouts ≤ 5%)
flood:            ~800–2700 batches/s · apply p95 ~100–146 µs
focused input:    p95 0.18–1.81 ms     (<8 ms target: PASS) · timeouts 0 (0.0%)
```

### Focused-shell bimodality (§36) — documented, not a wedge

The focused-input p95 remains bimodal: some runs measure ~0.2 ms, others
~1.8 ms. This is the Phase 3F documented shell/TTY bimodality. It is
**not** classified as a benchmark artifact, and it is nowhere near the
8 ms target. In this 20-run batch it produced **zero hangs, zero echo
timeouts, zero failures** — every run completed with `DONE`.

**Wedge protocol (kept from Phase 3F):** if a focused-shell wedge recurs:

1. Capture the forensic dump.
2. Determine whether the focused shell exited.
3. Determine whether PTY bytes continue arriving.
4. Determine pending write length.
5. Determine reader progress.
6. Determine engine drain progress.
7. Classify: fixture artifact / OS-TTY behavior / FlashTerminal bug.

The current harness has the deterministic watchdog + forensic diagnostics;
the wedge dump is preserved, and a future occurrence is never silently
converted to PASS.

## Orchestration + agent stress (§35)

From Phase 3A.1/3F validation (release):

```text
orchestration_bench: 40 cells ({1,10,20,50,100}t × {1,2,5,10} caps ×
                    serial/wide) all started == completed; wide scales
                    cap1 48 → cap10 296 t/s; RSS 12–24 MB
fairness 80+20 @cap5: 5.22 s, 100 completed, 0 failed/blocked
agent_stress 8:     ALL PASS (48.8k events/s, p95 16.4 µs)
```

## Memory stability (§39)

All bounded structures remain bounded: timeline (ring, 512), event
queues (bounded per subscriber), replan history, workflow history and
audit trail (bounded in RAM, older history disk-backed). Verified by the
bounded-structure unit tests plus the flood/stress sections of the
benchmark (RSS stays flat across flood runs).

## Performance regression (§38)

Multiplexer baseline unchanged from Phase 3F (see `docs/performance-report.md`);
the Phase 4 policy/audit layers add no measurable input-path latency
(idle-drain gate and focused-input p95 both hold).
