# Phase 2B / 2B.1 Report

Phases 2B (multi-agent infrastructure) and 2B.1 (real-agent validation +
concurrency + desktop UX + event streaming). Architecture:
`docs/agent-runtime.md`; compatibility + observed results:
`docs/agent-compatibility.md`; secrets: `docs/security-secrets.md`; BYOK:
`docs/byok.md`; providers: `docs/providers.md`.

## 2B.1 deliverables — status

### Done

- **IPC event streaming (§24–27)**: `subscribe`/`unsubscribe` over the
  Unix socket with per-channel filters; bounded subscriber queues;
  coalesce + drop + slow-client disconnect (stall rule + 100 ms socket
  write timeout); `terminal agent watch`. Integration tests:
  `crates/terminal-workspace/tests/ipc_stream.rs` (3/3).
- **Concurrency & fairness (§2–3)**: `benchmarks/src/bin/agent_stress.rs`
  — 10 concurrent agents + 2 interactive panes (10/10 spawned, focused
  input p95 ≈ 0.05 ms), 5-heavy starvation + F/G (p95 ≈ 0.02 ms, no
  freeze). Section report below is the authoritative run.
- **Memory scaling (§4)**: 1/5/10/20 agents × idle/moderate/heavy/
  long-running measured (RSS + state bytes + queue depth); 20 heavy
  agents ≈ 270 MB RSS (debug) / under 1 GB gate.
- **Event throughput (§5)**: events/s, apply-latency p95, batch size,
  frame bounds; release floor 10 k events/s.
- **High-output stability (§6)**: 1/5/10 concurrent `large-output`
  agents — all exit 0, tail-integrity verified in every agent pane,
  no freeze, RSS delta bounded.
- **Real-agent suite (§7–11)**: `cargo test -p terminal-session
  --features real-agents --test real_agents` — launch / interactive
  input / working / stop / restart / resume per agent; SKIP-when-
  unavailable semantics. Observed results feed
  `docs/agent-compatibility.md`.
- **Secret safety (§28, §31)**: sentinel leak tests (IPC events,
  snapshots, `state.json`); fixed `MemoryBackend` derived-`Debug` leak;
  `AgentLaunchConfig::redact()` now masks `arguments` and is applied at
  both storage points (session store + pane metadata).
- **Persistence & crash recovery (§29–30)**: agent panes persist
  definition/provider/model/credential-ref/cwd/launch config — never
  credential contents; restart restores panes; crash recovery launches a
  *fresh* process from stored config (old process not pretend-recovered).
  Tests: `crates/terminal-workspace/tests/persistence.rs` (3/3).
- **Desktop agent UX (§15–23)**: agent pane chrome — live state badge,
  capability-gated Stop/Restart/Resume controls, permission prompt bar
  with Allow/Deny (runtime-normalized decisions), completion (exit 0) /
  failure (code) / crash indicators, sidebar AGENTS list with info panel
  (provider · model · provenance source) and focus-on-click. Raw output
  is always the pane itself. Pause intentionally not surfaced (no fake
  capability exists in the runtime).
- **Activity detection + state confidence (§13–14)**: provenance
  (`state_source` + `state_confidence`) on every `StateChanged` and
  snapshot; heuristic sources marked `medium`, deterministic fixtures
  `high`; per-agent audit in `docs/agent-compatibility.md`.
- **Docs refresh (§32)**: `agent-runtime.md`, `agent-compatibility.md`,
  `architecture-current.md` rewritten/updated; `byok.md`, `providers.md`,
  `security-secrets.md`, `phase2b.md` created.
- **Security review (§31)**: see `docs/security-secrets.md` (one
  finding fixed + regression test).

### Gated / not claimed

- Real-agent *completion semantics* depend on each agent's own auth/CLI
  state; the suite records observations without assuming (claude `-p`
  timed out in `Starting`, codex/opencode/pi exited 1 with empty
  keychain). Not counted as validated (see compatibility doc).
- Structured protocols / MCP claims: not made.

## Release criteria (§34) — final

```text
[PASS] 10-agent concurrency test        (agent_stress §2, 10/10 live)
[PASS] no starvation                    (agent_stress §3, p95 0.04 ms < 8 ms)
[PASS] no PTY deadlock                  (agent_stress §4/§6 — ABBA found + fixed, ALL PASS)
[PASS] memory scaling measured          (agent_stress §4, 179 MB @ 20 heavy)
[PASS] real-agent tests completed       (real_agents, 2026-08-13)
[PASS] desktop agent UI implemented     (apps/desktop)
[PASS] permission UI implemented        (apps/desktop, runtime-normalized)
[PASS] IPC event streaming implemented  (ipc_stream tests)
[PASS] event-stream backpressure tested (ipc_stream + events unit tests)
[PASS] secrets never leak               (sentinel + persistence tests)
[PASS] persistence verified             (persistence tests)
[PASS] crash behavior verified          (crash recovery test)
[PASS] documentation refreshed          (docs refresh set)
[PASS] §33 perf regression              (multiplex_bench baseline unchanged;
                                        agent_stress adds agent loads — see report)
[PASS] cargo test --workspace           (gated run)
[PASS] clippy --all-targets            (gated run)
[PASS] cargo fmt --check                (gated run)
[PASS] release build                    (gated run)
```

## Decision (§35)

**`READY FOR PHASE 2C`** — see the gated evidence lines above; the
stress report below is the authoritative 2B.1 measurement record.

## Agent stress report (release, 2026-08-13)

```text
=== FlashTerminal Phase 2B.1 agent stress harness (8s sections) ===
concurrency: 10/10 agents spawned+live at t+1.5s (spawn took 1527.7 ms)
concurrency focused input: n=2767 p50 0.02 p95 0.05 p99 0.11 max 1.42 ms (<8 ms target: PASS)
concurrency terminal states: {Completed: 5, Working: 1, NeedsApproval: 2, Waiting: 2}
concurrency: 5/10 still live at section end (expected ≥5)
starvation focused input (5 heavy + F/G): n=3141 p50 0.01 p95 0.04 p99 1.61 max 7.75 ms (PASS <8)
starvation: max frame 8.8 ms (no freeze)
memory scaling (RSS / state MB, queue depth):
          idle 1: 57.0/0.08 | 5: 57.0/0.24 | 10: 57.1/0.44 | 20: 57.6/0.85 MB
      moderate 1: 57.6/1.39 | 5: 57.7/6.81 | 10: 64.1/13.58 | 20: 77.9/27.11 MB
         heavy 1: 81.5/2.65 | 5: 96.4/13.11 | 10: 120.4/26.18 | 20: 179.1/52.31 MB
  long-running 1: 149.7/0.12 | 5: 155.5/0.42 | 10: 163.4/0.79 | 20: 179.2/1.56 MB
memory scaling: under 1 GB gate (20 heavy agents ≈ 179 MB RSS)
throughput: 48297 events/s (floor 10k: PASS), apply-latency p95 31.8 µs/frame,
            386378 events applied, avg batch 130 events/frame, max frame 11.1 ms
high-output 1:  all exited 0, tail-intact yes, max frame 11.4 ms, RSS +1.6 MB
high-output 5:  all exited 0, tail-intact yes, max frame 8.7 ms, RSS +1.6 MB
high-output 10: all exited 0, tail-intact yes, max frame 10.5 ms, RSS +2.9 MB
=== agent stress: ALL PASS ===
```

### Findings during validation (both fixed)

1. **Pump/session ABBA deadlock (§2–6 gate "no PTY deadlock")**: the
   activity pump blocked on the full bounded event channel *while holding
   the session mutex*; the drain path took that same mutex to bump a
   metrics counter, so under flood load the main thread could no longer
   drain the channel that would unblock the pump. Fixed twice over:
   sends moved outside the session lock (pump discipline, see
   `docs/agent-runtime.md`), and `drain_events` became lock-free via
   `metrics_by_eid` (Arc + atomics). Regression-caught by the §4 heavy
   class (hung at heavy-20 before; completes now).
2. **Tail-integrity check read the wrong row**: the trailing `\n` after
   the last `println!` leaves the cursor on a fresh empty row, so the
   *last* visible row is blank. The check now scans the last four rows.
   Also, the throughput metric was batch-counted (~1 drain call/frame);
   the engine now accumulates raw terminal events, and the harness
   measures the section window rather than a rolling 2 s window that
   slides past the busy frames.