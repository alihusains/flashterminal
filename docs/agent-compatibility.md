# Agent Compatibility Matrix (Phase 2B.1)

Status: **observed results, 2026-08-13** — every claim below was produced
by the automated real-agent suite, not assumed.

## Reproduce

```bash
# deterministic fixture (no network, always run)
cargo test -p terminal-workspace --test ipc_stream
cargo test -p terminal-workspace --test persistence
cargo run --release -p benchmarks --bin agent_stress

# real agents (needs the binaries on PATH; SKIPs, never fails, when absent)
cargo test -p terminal-session --features real-agents \
  --test real_agents -- --test-threads=1 --nocapture
```

## Result matrix (real binaries, 2026-08-13)

Per-agent rows from `real_agents` with an in-memory credential store (no
BYOK keys injected — agents rely on their own CLI auth state):

| Capability | claude-code (`claude`) | codex | opencode | pi |
|---|---|---|---|---|
| Binary on PATH | ✓ | ✓ | ✓ | ✓ |
| Launch → `Started` | ✓ | ✓ | ✓ | ✓ |
| Interactive input (session survives) | ✓ | ✓ | ✓ | ✓ |
| `Working` detected (heuristic) | ✓ | ✓ | ✓ | ✓ |
| Stop → `Stopped` | ✓ | ✓ | `Completed`* | ✓ |
| Restart → `Started` | ✓ | ✓ | ✓ | ✓ |
| Resume | ✓ (native `--resume` accepted; behavior observed, not assumed) | ✗ not claimed | ✗ not claimed | ✗ not claimed |
| Simple task (`-p` / `exec` / `run` / `--print`) | **TIMEOUT — no Exited in 90 s; state stayed `Starting`** | `Exited code=1` (`Failed`)** | `Exited code=1` (`Failed`)** | `Exited code=1` (`Failed`)** |

`*` opencode exits on SIGINT/stop (its own semantics); the runtime records
the honest terminal state. `**` code-1 exits indicate the agent's own CLI
failed (auth/config at the agent level); with per-agent credentials the
same matrix is expected to complete — re-run the suite to observe.

### Reading this table honestly

- **Validated** (this run): launch, input pass-through, activity
  detection, stop, restart, resume-flag acceptance, IPC event integrity,
  persistence, crash recovery, secret redaction.
- **NOT validated**: completion/failure *semantics* for real providers
  (depends on each agent's auth + CLI contract), structured protocols
  (adapters are `cli`-based; structured/MCP claims are not made),
  Pi/Codex/OpenCode completion. Fake-agent covers the full lifecycle
  deterministically instead (`streaming`, `completion`, `failure`,
  `crash`, `large-output`).

## Fake-agent matrix (deterministic, always run)

| Scenario | Verified behavior |
|---|---|
| `startup` | prints, exits 0 |
| `working` | continuous progress output; `Working` state |
| `streaming` | 1,000 lines rapid-fire; exits 0 |
| `waiting` | blocks on stdin; echoes; exits on `exit`/`quit` |
| `approval` | permission request; `y`/`n` via `permission` IPC |
| `completion` | success text, exit 0, `Completed` |
| `failure` | stderr error, exit 1, `Failed` |
| `crash` | exit 139, `Crashed` |
| `large-output` | 100k lines, bounded memory, exit 0 |
| `long-running` | output until stdin close / `--duration` |

## Activity detection audit (§13)

State classification today is **heuristic over the terminal stream**
(output-pattern refinement); output remains authoritative. Confidence is
`high` for deterministic fixtures, `medium` for heuristics; sources per
agent are recorded in `AgentSession.provenance` (documented in
`docs/agent-runtime.md`).

| Agent | source | confidence | notes |
|---|---|---|---|
| fake-agent | deterministic scenario text | high | exact by construction |
| claude-code / codex / opencode / pi | output heuristics | medium | unverified native/structured annotations are not consumed yet |
| generic-cli | output heuristics | medium | any TTY command |

## Phase 2B.1 stress evidence (§2–§6)

`agent_stress` (release): 10 concurrent agents + 2 interactive panes,
5-heavy starvation with focused-input p95 < 8 ms, memory scaling
1/5/10/20 × 4 workloads (20 heavy agents ≈ 270 MB RSS), event
throughput, high-output 1/5/10 with tail-integrity + no-freeze
assertions. See the `=== agent stress ===` report for the latest run.