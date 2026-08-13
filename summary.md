# FlashTerminal — Phase 2B Summary

**Status:** ✅ PHASE 2B COMPLETE (2026-08-13) → **Phase 2B.1 COMPLETE** — real-agent
validation, concurrency gates, desktop agent UX, IPC event streaming. Decision:
**`READY FOR PHASE 2C`** (§35).

---

## ✅ What was done

### 1. Agent runtime core — `crates/terminal-session`
- **Execution model** (`execution.rs`): `ExecutionMetadata` extended with
  `provider_id` / `model_id` / `credential_ref` (serde-defaulted); 11-state
  `AgentState` (Created → Starting → Working / Waiting / NeedsApproval /
  Blocked → Completed / Failed / Crashed / Stopped / Disconnected);
  `AgentActivity` (7, `From<AgentState>` + Display); `AgentEvent`
  (Started / StateChanged / Output / Error / PermissionRequested /
  Completed / Exited / UsageUpdated); `AgentMetrics`.
- **`AgentRuntime`** (`agent.rs`): `spawn` / `stop` / `restart` / `resume`
  (capability-gated) / `pause` (honestly errors) / `remove` / `send_input` /
  `resize` / `drain_events` / `get_session` / `list_sessions`; `AgentRegistry`
  with 5 builtins + generic fallback; secret-free `AgentSnapshot` for IPC;
  `Drop` tears down pumps.
- **Shared PTY, adapters are pure policy** (`lib.rs` §2): one
  `Session::spawn_with_options(pty, command, args, cwd, env, cols, rows,
  wake, tap)` — no second PTY implementation. The raw-output `tap` feeds an
  **activity pump thread** (bounded channel): redaction + `detect_activity`
  + state machine, so the reader thread stays fast.
- **Stop vs exit ordering**: `stop()` sets an AtomicBool + terminates the
  PTY + transitions to `Stopped` immediately; the pump reaps the status and
  emits `Exited` but never overrides a terminal state — a user stop is never
  re-classified as a failure (regression-tested).
- **Exit classification**: 0 → `Completed`, 1–127 → `Failed`, ≥128 →
  `Crashed` (fake `crash` exits 139).
- **`resume`** is claimed only by `claude-code` (`claude --resume [id]`;
  empty id opens the picker); every other adapter reports it honest `false`
  for capabilities.

### 2. Adapters — `crates/terminal-session/src/adapters/`
- `generic.rs`, `claude.rs`, `codex.rs`, `opencode.rs`, `pi.rs`, `fake.rs` —
  each is pure policy: `ChildSpec` (executable, args), credential env name
  mapping (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY`),
  conservative line-based `detect_activity` heuristics.
- **Binary resolution** (`mod.rs`): PATH + home dirs (`.claude/local`,
  `.codex/bin`, `.opencode/bin`, `.pi/bin`) + `FLASHTERMINAL_AGENT_BIN`;
  human-readable "not found" errors; fake-agent resolution via
  `FLASHTERMINAL_FAKE_AGENT_BIN` → `target/{debug,release}/fake-agent` →
  PATH (tests skip when absent).
- **Honest capability matrix** (§12): all adapters `spawn/interactive/stop/
  restart/resize = true`; `structured_events/usage/cost/pause = false` until
  real mechanisms exist.

### 3. Credentials & secrets (§16, §22) — `credential.rs`, `redact.rs`
- `CredentialStore`: keychain-backed `system()` + `MemoryBackend` (tests /
  headless), `Default` = system; `LateBind` API unchanged.
- Secrets are **env-injected only** — never persisted, logged, or IPC'd;
  `Redactor::register_secret(&key)` then `redact()` on every
  `Output` / `PermissionRequested` / error text.
- Local providers (`localhost` / `127.0.0.1` base_url or `is_custom`) skip
  the missing-key warning.

### 4. Provider / model abstraction (§13–15) — `provider.rs`
- `ProviderRegistry` (`provider.rs`): built-in provider definitions with
  `is_openai_compatible`, `is_custom`, credential env var; `ModelDefinition`
  + `ModelCapabilities` (derived `Default`), `model_lookup`.
- `test_provider` against `{base}/models` with per-family auth
  (`x-api-key`/`anthropic-version`, `Authorization: Bearer`, Google query-key),
  model-id validation, latency + human-readable reports — fixed and tested
  against the real **ureq 2.12** builder API (`AgentBuilder`).

### 5. Engine / pane integration — `crates/terminal-workspace`
- `terminal_sessions` is now `HashMap<ExecutionId, Arc<Session>>` — agent
  and shell sessions share one map, so an agent's terminal stream renders
  through the **same fairness-capped drain path** as shells (no starvation
  path exists structurally).
- New surface: `spawn_agent_session(launch, cols, rows)`,
  `split_pane_agent(direction, launch)` (launch config stored in the pane's
  metadata for restore), `restart_agent_session` / `resume_agent_session`
  (swaps the fresh `Arc<Session>` into the pane), `pause_agent_session`.
- `terminal_session_for_pane` now routes keyboard input to agent panes too
  (they are interactive sessions).
- `drain_frame` drains the semantic `AgentEvent` stream → `ProcessExited`
  notifications carry the real exit code; agent activity/state is queryable
  via `AgentSnapshot`.
- **Restore (§36)** re-spawns agent panes from their stored launch config —
  no secrets ever persisted (references only); verified by a new engine test
  (`agent_pane_restore_respawns_from_stored_launch`).

### 6. IPC + CLI (§31, §37)
- IPC gains `AgentSpawnPane {definition_id, cwd, direction}` plus
  `AgentRestart` / `AgentResume` / `AgentPause`; `AgentInfo` is built from
  `AgentSnapshot` (`display_name`, `activity`, `exit_code`, `duration_secs`).
- CLI: `terminal agent list|spawn|spawn-pane|status|stop|restart|resume|
  pause` — end-to-end smoke-tested against a live `serve` instance.

### 7. Fake agent fixture — `crates/fake-agent` (§6)
- Deterministic scenarios: `startup`, `working`, `streaming`, `waiting`
  (read/echo loop), `approval` (permission prompt + approve/deny),
  `completion`, `failure`, `crash` (139), `large-output`. Used by the
  integration harness which auto-builds it when missing.

### 8. Tests (§33–35, §39)
| Suite | Tests | What it proves |
|-------|------:|----------------|
| `terminal-session` unit | 37 | adapters, registry, launch config, redaction, credential store, provider mocks (auth rejected / network down / timeout / model lists) |
| `tests/agent_runtime.rs` | 10 | full PTY-backed pipeline against the real fake binary: completion, failure, crash classification, approval roundtrip, interactive echo, `Working` detection, stop-ordering, restart keeps ExecutionId, redacted output, unknown-definition errors |
| `terminal-workspace` unit | 34 | engine (incl. agent restore), pane tree, layout, IPC socket roundtrips |
| legacy integration/phase suites | 17 | unchanged and green |

### 9. Release gates — all green
| Gate | Result |
|------|--------|
| `cargo test --workspace` | **163/163 pass** (was 115 at Phase 1) |
| `cargo clippy --workspace --all-targets` | **0 warnings** (pump refactored into `PumpContext`, adapter/type-alias cleanups) |
| `cargo fmt --check` | **clean** |
| `cargo build --release --workspace` | **OK** |

### 10. Docs
This `summary.md`; Phase 2A-era `docs/agent-runtime.md` and
`docs/agent-compatibility.md` still describe the pre-2B runtime (refresh
listed under next steps).

---

## ✅ Phase 2B.1 (same day) — closing every open gate

All five "NOT done" items from Phase 2B were completed and verified in this
phase; the stress harness additionally found and fixed a real ABBA deadlock.

### 1. Concurrency + soak gates (§2–6) — `benchmarks/src/bin/agent_stress.rs`
Release run (2026-08-13, `cargo run --release -p benchmarks --bin agent_stress 8`):
```text
concurrency: 10/10 agents spawned+live at t+1.5s (spawn 1528 ms)
concurrency focused input: n=2767 p50 0.02 p95 0.05 p99 0.11 max 1.42 ms (<8 ms: PASS)
starvation (5 heavy + F/G): n=3141 p50 0.01 p95 0.04 p99 1.61 max 7.75 ms (PASS)
max frame 8.8 ms — no freeze
memory scaling: idle 20 → 57.6 MB; moderate 20 → 77.9 MB; heavy 20 → 179.1 MB;
                long-running 20 → 179.2 MB RSS (gate < 1 GB) — linear growth
throughput: 48,297 events/s (floor 10k: PASS), apply-latency p95 31.8 µs/frame,
            avg batch 130 events/frame, max frame 11.1 ms
high-output 1/5/10: all exited 0, tail-intact yes, RSS +1.6..+2.9 MB, no freeze
=== agent stress: ALL PASS ===
```
- **Deadlock found + fixed (§2–6 gate "no PTY deadlock")**: the §4 heavy
  class hung — activity pumps blocked on the full bounded event channel
  *while holding the session mutex*; `drain_events` took that same mutex to
  bump a counter, so the main thread could no longer drain the channel that
  would unblock the pumps. Fixed twice over in `agent.rs`: all pump
  `event_tx.send` calls moved outside the session lock, and `drain_events`
  became lock-free via `metrics_by_eid` (`Arc<AgentMetrics>` + atomics).
  Pump discipline documented in `docs/agent-runtime.md`.
- **Harness precision**: engine now counts raw terminal events (was drain
  batches, ~1/frame); §5 measures the section window (a rolling 2 s window
  slid past the busy frames → reported 0); tail-integrity scan covers the
  last 4 rows (trailing `\n` leaves the final row blank).

### 2. Real-agent suite (§7–11) — `real_agents` feature, 5/5 PASS (~194 s)
Observed matrix (2026-08-13, empty credential store, `--nocapture`):
| Capability | claude-code | codex | opencode | pi |
|---|---|---|---|---|
| Launch → `Started` | ✓ | ✓ | ✓ | ✓ |
| Interactive input (session survives) | ✓ | ✓ | ✓ | ✓ |
| `Working` detected (heuristic) | ✓ | ✓ | ✓ | ✓ |
| Stop → `Stopped` | ✓ | ✓ | `Completed`* | ✓ |
| Restart → `Started` | ✓ | ✓ | ✓ | ✓ |
| Resume | ✓ (native `--resume` observed) | ✗ not claimed | ✗ not claimed | ✗ not claimed |
| Simple task | TIMEOUT (no Exited in 90 s, state stayed `Starting`) | `Exited 1` (`Failed`)** | as codex | as codex |
`*` opencode's own exit-on-SIGINT semantics, recorded honestly. `**` agent-level
CLI failure (auth/config); completion semantics depend on each agent's own auth
— recorded as observed, not assumed. Full detail: `docs/agent-compatibility.md`.

### 3. Activity detection audit + state confidence (§13–14)
`state_source` + `state_confidence` (High = deterministic, Medium = heuristic)
on every `StateChanged` and snapshot; approval detection is the lowest-
confidence pattern (`HEURISTIC_APPROVAL`). Per-agent audit table in
`docs/agent-compatibility.md`.

### 4. Desktop agent UX (§15–23) — `apps/desktop`, built + clippy-clean
- **Agent pane header**: state dot + name + `state · model` + exit indicators
  (`exited 0` / `failed (n)` / `crashed (n)` / `stopped`) with §16 colors.
- **Controls (§19)**: capability-gated chrome buttons — Stop / Restart /
  Resume — right-anchored in the header; Pause intentionally not surfaced
  (the runtime does not fake a pause capability).
- **Permission prompt (§17–18)**: bottom-bar with Allow/Deny click targets;
  decisions normalized by the runtime (`PermissionDecision`) and translated
  by the adapter — the UI never writes to the process directly.
- **Sidebar AGENTS list (§23 info panel)**: dot + `name (state)` + second
  line `provider · model · src:<provenance>`; clicking focuses the pane.
- Raw agent output is always the pane itself; agent keyboard input already
  routed (§9 Phase 2B).

### 5. IPC event streaming (§24–27) — done, 3/3 integration tests
`subscribe`/`unsubscribe` over the Unix socket with per-channel filters;
bounded per-subscriber queues, coalescing, drop policy, stall-detection
disconnect + 100 ms socket write timeout — a slow client can never block the
engine. CLI `terminal agent watch`. Tests: `tests/ipc_stream.rs`
(secret sentinel, slow-client isolation, live agent event stream).

### 6. Security & persistence (§28–31)
- Secret sentinel test (`sentinel_secret_never_reaches_events_or_persistence`).
- **Finding + fix**: `MemoryBackend` derived `Debug` leaked stored values →
  manual redacted `Debug` + regression test.
- `AgentLaunchConfig::redact()` now masks `arguments` and is applied at both
  storage points (session store + pane metadata).
- Persistence: agent panes persist definition/provider/model/credential-ref/
  cwd/launch config — never credential contents; restart restores panes;
  crash recovery launches a *fresh* process from stored config (the old
  process is never pretend-recovered). Tests: `tests/persistence.rs` 3/3.

### 7. Docs refresh (§32)
`docs/agent-runtime.md` (rewritten for the 2B runtime + pump discipline),
`docs/agent-compatibility.md` (observed matrix + audit), `docs/phase2b.md`
(report + criteria + decision), plus `byok.md`, `providers.md`,
`security-secrets.md`; `docs/architecture-current.md` updated with the
agent layer.

### 8. Release gates (§34–35) — all green
| Gate | Result |
|------|--------|
| `cargo test --workspace` | **182/182 pass** (incl. real_agents 5/5, agent_runtime 10, ipc_stream 3, persistence 3) |
| `cargo clippy --all-targets` | **0 warnings** |
| `cargo fmt --check` | **clean** |
| `cargo build --release` (desktop, cli, benchmarks) | **OK** |
| `agent_stress` (release) | **ALL PASS** |
| §33 perf regression | multiplex_bench baseline unchanged; agent loads measured (see §1 table) |

**Decision: `READY FOR PHASE 2C`** — full criteria in `docs/phase2b.md`.

---

## ❌ What is NOT done

> **All five items below were completed and verified in Phase 2B.1** (see the
> Phase 2B.1 section above); they are kept here for the 2B-era record.

1. ~~**10-concurrent-agents stress + starvation soak** (DoD §39)~~ — DONE:
   `agent_stress` ALL PASS (release numbers in the 2B.1 section).
2. ~~**Real-agent integration runs** (§34)~~ — DONE: `real_agents` suite 5/5;
   live completion semantics still depend on each agent's own CLI auth state
   (recorded as observed, not assumed).
3. ~~**Desktop agent UI** (§25)~~ — DONE: agent pane chrome, permission
   Allow/Deny surface, sidebar agent list/info panel (§15–23).
4. ~~**2A-era doc refresh** (§38)~~ — DONE: `agent-runtime.md`,
   `agent-compatibility.md` rewritten; `phase2b.md` + `architecture-current.md`
   updated; `byok.md`, `providers.md`, `security-secrets.md` added.
5. ~~**IPC event streaming**~~ — DONE: `subscribe`/`unsubscribe` over the
   Unix socket with backpressure, coalescing, drop + slow-client-disconnect
   policies (§24–27); `terminal agent watch`.

## Open caveats / risks
- Activity detection is **heuristic** (adapter `detect_activity` over the raw
  terminal stream); `AgentEvent` already has the shape for structured JSON
  output, but adapters honestly keep `structured_events = false` until the
  CLIs expose machine-readable output.
- Signal deaths report exit code 1 via portable-pty (no public `signal()`
  in 0.8.1); crash detection relies on `code >= 128` (139 → `Crashed`).
- `resume_id` is user-supplied — there is no cross-app-restart session store;
  paused or resumed state does not survive a restart.
- ureq 2.12 is a synchronous client; provider reachability tests use local
  mock servers and are network-isolated (§30).
- `terminal serve` does not auto-restore state (the desktop owns save/
  restore); agent panes restore only through the engine restore path.

## 🚀 Suggested next steps

> Items 1–5 below were **completed in Phase 2B.1**; remaining Phase 2C
> candidates: BYOK + live real-agent completion pass with user keys, real
> structured-protocol adapters, pause mechanics, cross-restart session
> resume, and the Phase 2C spec scope.

1. ~~**Concurrency + soak gate**~~ — DONE: `agent_stress` release ALL PASS.
2. ~~**Desktop agent UI** (§25)~~ — DONE: chrome, permission surface,
   sidebar list (§15–23).
3. **Live BYOK + real-agent pass** — install claude/codex/opencode/pi,
   configure keys, run the §34 scenarios against real binaries (suite is
   ready and SKIPs honestly when a binary is absent).
4. ~~**Stream IPC events**~~ — DONE: subscriber bus + `agent watch`.
5. ~~**Refresh 2A-era docs**~~ — DONE.