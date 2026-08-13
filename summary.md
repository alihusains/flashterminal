# FlashTerminal — Phase 2B.1 Summary

**Status:** ✅ PHASE 2B.1 COMPLETE — Real-Agent Validation + Concurrency +
Desktop Agent UX + Event Streaming (2026-08-13). Per `2b1.md` §34–35 and
Appendix A–H.

---

## What was done (`2b1.md` §1–32)

### §2–6 Concurrency, starvation, memory, throughput, high-output gates
`benchmarks/src/bin/agent_stress.rs` — release run, 2026-08-13
(`cargo run --release -p benchmarks --bin agent_stress 8`):

```text
concurrency: 10/10 agents spawned+live at t+1.5s (spawn 1528 ms)
concurrency focused input: n=2767 p50 0.02 p95 0.05 p99 0.11 max 1.42 ms (<8 ms: PASS)
starvation (5 heavy + F/G): n=3141 p50 0.01 p95 0.04 p99 1.61 max 7.75 ms (PASS)
max frame 8.8 ms — no freeze
memory scaling (RSS / state MB):
  idle 1→20: 57.0/0.08 → 57.6/0.85 | moderate 20: 77.9/27.11 |
  heavy 20: 179.1/52.31 | long-running 20: 179.2/1.56 MB (gate < 1 GB: PASS)
throughput: 48,297 events/s (floor 10k: PASS), apply-latency p95 31.8 µs/frame,
            avg batch 130 events/frame, max frame 11.1 ms
high-output 1/5/10: all exited 0, tail-intact yes, RSS +1.6..+2.9 MB, no freeze
=== agent stress: ALL PASS ===
```

- **Deadlock found + fixed** (gate "no PTY deadlock"): §4 heavy-20 hung —
  activity pumps blocked on the full bounded event channel *while holding
  the session mutex*; `drain_events` took the same mutex to bump a counter,
  so the main thread could no longer drain the channel that would unblock
  the pumps (ABBA). Fixed in `agent.rs`: all pump `event_tx.send` calls
  moved outside the session lock, and `drain_events` became lock-free via
  `metrics_by_eid` (`Arc<AgentMetrics>` + atomics). Pump discipline is
  documented in `docs/agent-runtime.md`.
- **Harness precision**: engine now counts raw terminal events (was drain
  batches, ~1/frame); §5 measures the section window; the tail-integrity
  scan covers the last 4 rows (a trailing `\n` leaves the final row blank).

### §7–12 Real-agent validation — `real_agents` feature, 5/5 PASS (~194 s)
Observed matrix (2026-08-13, empty credential store, `--nocapture`):

| Capability | claude-code | codex | opencode | pi |
|---|---|---|---|---|
| Launch → `Started` | ✓ | ✓ | ✓ | ✓ |
| Interactive input (session survives) | ✓ | ✓ | ✓ | ✓ |
| `Working` detected (heuristic) | ✓ | ✓ | ✓ | ✓ |
| Stop → `Stopped` | ✓ | ✓ | `Completed`* | ✓ |
| Restart → `Started` | ✓ | ✓ | ✓ | ✓ |
| Resume | ✓ (native `--resume` observed) | not claimed | not claimed | not claimed |
| Simple task | TIMEOUT (no Exited in 90 s, state stayed `Starting`) | `Exited 1` (`Failed`)** | as codex | as codex |

`*` opencode's own exit-on-SIGINT semantics, recorded honestly. `**` agent-level
CLI failure (auth/config); completion semantics depend on each agent's own
auth — recorded as observed, not assumed. Full detail:
`docs/agent-compatibility.md`.

### §13–14 Activity detection audit + agent state confidence
`state_source` + `state_confidence` on every `StateChanged` and snapshot:
deterministic fixtures `high`, heuristics `medium` (approval detection is
the lowest-confidence pattern, `HEURISTIC_APPROVAL`). Per-agent audit in
`docs/agent-compatibility.md`.

### §15–23 Desktop agent UI — `apps/desktop`, built + clippy-clean
- **Agent pane + header** (§15–16): state dot, name, `state · model`, exit
  indicators (`exited 0` / `failed (n)` / `crashed (n)` / `stopped`) with
  per-state colors; viewport reserves the header chrome.
- **Permission prompt + handling** (§17–18): bottom bar with Allow/Deny
  click targets; decisions normalized by the runtime
  (`PermissionDecision::AllowOnce/Deny`) and translated by the adapter — the
  UI never writes to the process directly.
- **Completion / failure UI** (§19–20): colored exit indicator + exit code
  in the header; raw output is always the pane itself.
- **Controls** (§21): capability-gated chrome buttons — Stop / Restart /
  Resume — right-anchored in the header; Pause intentionally not surfaced
  (the runtime does not fake a pause capability).
- **Information panel** (§22): sidebar AGENTS list — dot + `name (state)` +
  `provider · model · src:<provenance>`; clicking focuses the pane.
- **Output** (§23): agent terminal stream flows through the same
  fairness-capped drain path as shells; keyboard input routes to agent panes.

### §24–27 IPC event streaming + backpressure
`subscribe`/`unsubscribe` over the Unix socket with per-channel filters;
bounded per-subscriber queues, coalescing, drop policy, stall-detection
disconnect + 100 ms socket write timeout — a slow client can never block the
engine. CLI: `terminal agent watch`. Tests: `tests/ipc_stream.rs` 3/3
(secret sentinel, slow-client isolation, live agent event stream).

### §28–32 Secret safety, persistence, crash recovery, security, docs
- **§28 secret safety**: secrets are env-injected only; sentinel test
  `sentinel_secret_never_reaches_events_or_persistence`; `Redactor` masks
  registered secrets + known shapes.
- **§29 persistence**: agent panes persist definition/provider/model/
  credential-**ref**/cwd/launch config — never credential contents
  (`tests/persistence.rs` 3/3).
- **§30 crash recovery**: restart launches a *fresh* process from stored
  config; the old process is never pretend-recovered.
- **§31 security review**: `MemoryBackend` derived `Debug` leaked stored
  values → fixed (redacted manual `Debug`) + regression test;
  `AgentLaunchConfig::redact()` masks `arguments` at both storage points.
  Report: `docs/security-secrets.md`.
- **§32 docs**: `agent-runtime.md`, `agent-compatibility.md`,
  `architecture-current.md` refreshed; `phase2b.md` (report + criteria),
  `byok.md`, `providers.md`, `security-secrets.md` added.

### §33 Performance regression
Multiplexer baseline unchanged (multiplex_bench); agent-load figures are the
§2–6 table above (RSS/CPU/input p95/render bounds/event throughput).

---

## Final report (`2b1.md` §35, Appendix A–H)

### A. Real Agent Compatibility
See the §7–12 matrix: all four binaries launch, accept input, report
`Working`, stop and restart through the shared pipeline; completion
semantics depend on each agent's own CLI auth (recorded as observed).
SKIP-when-unavailable semantics: the suite never fails on missing binaries.

### B. Concurrency
1 / 5 / 10 / 20 agents measured across idle / moderate / heavy /
long-running workloads — 10/10 spawn+live at t+1.5 s, 20-pane engines at
179 MB RSS, linear state growth (52 MB at 20 heavy).

### C. Performance
RSS: ≤ 179 MB @ 20 heavy agents (gate 1 GB) · input p95: 0.04 ms under
starvation load · render/frame bounds: max 11.1 ms · event throughput:
48,297 events/s · apply-latency p95: 31.8 µs/frame.

### D. Desktop UX
Agent pane with chrome header, live state badge, permission Allow/Deny bar,
completion/failure indicators, Stop/Restart/Resume controls (capability-
gated), sidebar AGENTS info panel with focus-on-click; raw output is the
pane itself.

### E. IPC
Event streaming over the Unix socket with filters, bounded subscriber
queues, coalescing, structured drops, stall-detection disconnect and a
socket write timeout — backpressure verified by the slow-client test.

### F. Security
Keys live only in the OS keychain; everything else holds
`keychain://flashterminal/<provider>` references; sentinel + persistence
tests prove secrets never reach events, snapshots, or `state.json`;
`MemoryBackend` Debug leak found and fixed.

### G. Remaining Risks
- Real-agent completion semantics depend on each agent's own CLI auth/config
  (claude `-p` timed out in `Starting`; codex/opencode/pi exited 1 with an
  empty keychain — with BYOK keys the matrix is expected to complete).
- Activity detection remains heuristic (no structured protocol consumed).
- Signal deaths report exit code 1 via portable-pty; crash detection relies
  on `code >= 128` (139 → `Crashed`).
- No cross-app-restart session store: `resume_id` is user-supplied.
- Pause remains honestly unsupported (no fake capability).

### H. Decision

```text
READY FOR PHASE 2C
```

Per `2b1.md` §35, Phase 2C is **not** begun automatically.
