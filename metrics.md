# FlashTerminal — Metrics & Test Numbers (Phase 2B.1)

All figures captured 2026-08-13 from the authoritative release run
(`agent_stress` log: `/tmp/agent_stress_release4.log`, EXIT=0) and the
workspace gate runs.

---

## 1. Test suite (workspace)

| Gate | Result |
|------|--------|
| `cargo test --workspace` | **182/182 pass** |
| `cargo clippy --all-targets` | **0 warnings** |
| `cargo fmt --check` | **clean** |
| `cargo build --release` (desktop, cli, benchmarks) | **OK** |

### Per-suite breakdown

| Suite | File | Tests | Result |
|-------|------|-------|--------|
| Real agents (feature `real-agents`) | `crates/terminal-session/tests/real_agents.rs` | 5 | PASS, ~194 s |
| Agent runtime e2e (fake-agent backed) | `crates/terminal-session/tests/agent_runtime.rs` | 10 | PASS |
| IPC event streaming | `crates/terminal-workspace/tests/ipc_stream.rs` | 3 | PASS |
| Persistence / crash recovery | `crates/terminal-workspace/tests/persistence.rs` | 3 | PASS |
| Secret sentinel | engine-level leak test | 1 | PASS |
| Unit tests | crates (session 71, workspace 46, core/parser/text/renderer, etc.) | rest | PASS |
| Phase 0.5.1 compatibility | `crates/terminal-session/tests/phase051.rs` | — | PASS |
| Session integration | `crates/terminal-session/tests/integration.rs` | — | PASS |

---

## 2. Concurrency stress (§2) — 10 agents

```text
spawn: 10/10 agents spawned+live at t+1.5s (spawn took 1523.4 ms)
focused input: n=3157  p50 0.03  p95 0.06  p99 0.11  max 1.32 ms
                (<8 ms target: PASS)
terminal states at section end: Waiting 2 · Completed 5 · NeedsApproval 2 · Working 1
```

## 3. Starvation (§3) — 5 heavy agents + interactive F/G panes

```text
focused input: n=3141  p50 0.01  p95 0.04  p99 1.61  max 7.75 ms
                (<8 ms target: PASS)
max frame: 8.8 ms — no freeze
```

## 4. Memory scaling (§4) — RSS / agent-state MB, @1/5/10/20 agents

| Workload | 1 | 5 | 10 | 20 |
|----------|-------|-------|--------|--------|
| idle | 57.0 MB / 0.08 | 57.0 / 0.24 | 57.1 / 0.44 | 57.6 / 0.85 |
| moderate | 57.6 / 1.39 | 57.7 / 6.81 | 64.1 / 13.58 | 77.9 / 27.11 |
| heavy | 81.5 / 2.65 | 96.4 / 13.11 | 120.4 / 26.18 | 179.1 / 52.31 |
| long-running | 149.7 / 0.12 | 155.5 / 0.42 | 163.4 / 0.79 | 179.2 / 1.56 |

```text
queue depths stayed q0 (idle/moderate) and bounded (~q1185–1256 heavy): PASS
gate: 20-agent engines < 1 GB RSS          → 179.2 MB max: PASS
20 heavy agents ≈ +122 MB over idle → linear, ~2.6 MB/agent marginal
```

## 5. Event throughput (§5)

```text
throughput:      48,297 events/s            (floor 10k: PASS)
events applied:  386,378
frames:          2,971                      avg batch 130.0 events/frame
apply latency:   p95 31.83 µs/frame
max frame:       11.1 ms
```

## 6. High-output agents (§6) — 1 / 5 / 10 agents × 2048-line flood

| Count | Exit | Tail intact | Max frame | RSS delta | Frames |
|-------|------|-------------|-----------|-----------|--------|
| 1 | 0 | yes | 11.4 ms | +1.6 MB | 134 |
| 5 | 0 | yes | 8.7 ms | +1.6 MB | 1127 |
| 10 | 0 | yes | 10.5 ms | +2.9 MB | 1003 |

```text
=== agent stress: ALL PASS ===  (EXIT=0)
```

---

## 7. Real-agent observations (§7–12) — feature `real-agents`, 5/5

Suite: Launch→Started · interactive input · Working heuristic · Stop ·
Restart · Resume-claim · simple-task completion. (~194 s wall, empty
credential store, `--nocapture`.)

| Capability | claude-code | codex | opencode | pi |
|---|---|---|---|---|
| Launch → `Started` | ✓ | ✓ | ✓ | ✓ |
| Interactive input (session survives) | ✓ | ✓ | ✓ | ✓ |
| `Working` detected (heuristic) | ✓ | ✓ | ✓ | ✓ |
| Stop → `Stopped` | ✓ | ✓ | `Completed`* | ✓ |
| Restart → `Started` | ✓ | ✓ | ✓ | ✓ |
| Resume | ✓ native `--resume` | not claimed | not claimed | not claimed |
| Simple task (90 s cap) | TIMEOUT in `Starting` | `Exited 1` → `Failed`** | as codex | as codex |

`*` opencode's own exit-on-SIGINT semantics. `**` agent-level CLI failure
(no keys/auth); recorded as observed, not assumed.

## 8. Deadlock regression (found by §4 gate)

Fix in `agent.rs`: pump `event_tx.send` moved outside the session mutex;
`drain_events` lock-free via `metrics_by_eid` (`Arc<AgentMetrics>` +
atomics). Re-verified: harness ALL PASS (twice) + real_agents 5/5.

---

## 9. Perf regression baseline (§33)

- `multiplex_bench`: multiplexer baseline unchanged by the agent layer.
- Agent-load figures: the §2–6 tables above (RSS 57–179 MB, input p95
  ≤ 0.06 ms, render max 11.1 ms, throughput 48,297 events/s).

## 10. Reproduce

```text
cargo run --release -p benchmarks --bin agent_stress 8     # stress gates
cargo test --workspace                                      # 182/182
cargo test -p terminal-session --features real-agents --test real_agents -- --test-threads=1
cargo clippy --all-targets && cargo fmt --check
```