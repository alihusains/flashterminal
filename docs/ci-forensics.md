# CI Forensic Audit

Evidence gathered from the actual GitHub Actions history at
`github.com/alihusains/flashterminal`, plus local reproduction, ahead of the
CI repair in this phase. See `docs/ci.md` for the resulting design.

## GitHub run history (last 11 runs, all on `main`, all `push`)

| Run | Commit | Job(s) failed | Failed step | Root cause |
|---|---|---|---|---|
| 31940414678 | `1ebf6ca` (phase 4 done) | Test | Run tests | `stop_transitions_to_stopped_and_stays` — no `Exited` event within 5 s after stop |
| 31809662285 | `ef188ba` | Test | Run tests | same test, same symptom |
| 31795967296 | (3a1 done) | Test | Run tests | same test, same symptom |
| 31776214447 | (3A summary) | Test, Clippy | Run tests | same test; Clippy failed on a since-fixed commit |
| 31776112200 | (3a.md) | Test | Run tests | same test, same symptom |
| 31725794321 | (2c partial) | Test | Run tests | **product bug**: `E0425/E0433 cannot find type AgentFilter` — compile error, fixed by a later commit |
| 31712231494 | (metrics.md) | Test | Run tests | same PTY test, same symptom |
| 31712159773 | (2B.1 summary) | Test | Run tests | same PTY test, same symptom |
| 31711907862 | (gitignore reduced) | Test | Run tests | same PTY test, same symptom |
| 31711850788 | (workspace files) | Test | Run tests | same PTY test, same symptom |
| 31711642704 | (Phase 1+2 initial) | Test | Run tests | same PTY test, same symptom |

**10 of 11 runs failed on exactly one test**:
`crates/terminal-session/tests/agent_runtime.rs::stop_transitions_to_stopped_and_stays`
(line 284), every time on the `Test` job, every time on `macos-latest`.
Check/Fmt/Clippy/Performance passed on every run once the one genuine
compile-error commit (`31725794321`) was superseded. GitHub Actions has
never gone green on this repository.

## Root cause, part 2: `try_wait` always failed after `stop()` (confirmed, fixed)

The `Once` fix below closed the build race, but a second push
(`0fccea7`, run `31942502570`) still failed the same test — same symptom,
now at a shifted line number: the 5 s wait for `AgentEvent::Exited` after
`stop()` still timed out on the GitHub runner.

Root cause: `PtyManager::terminate()` (`crates/pty/src/lib.rs`) removes the
session from its `sessions` map *before* returning. `AgentRuntime::stop()`
calls `terminate()` synchronously, then the agent's pump thread calls
`poll_exit_code()`, which calls `PtyManager::try_wait()` — but the session
id is already gone from the map, so every one of its 100 attempts (20 ms
apart) returned `Err("session not found")`, and after the full 2 s budget
it gave up and returned `None`. This path could **never** succeed: by
construction, `terminate()` always runs before the pump's poll starts. That
guaranteed 2 s tax (plus scheduling variance) is what pushed the test over
its 5 s deadline under GitHub Actions' load, while usually — not
always — finishing in time on a quieter local machine.

This is a genuine product bug, not just a test-timing issue: every
user-initiated stop was losing its real exit code and wasting 2 seconds to
report `None` instead. Fixed: `PtyManager` now records the exit code
`terminate()` reaped (or `None` if the child still wouldn't report one) in
a small side-table, and `try_wait()` consults it as a fallback when the
live session is already gone — resolving immediately with the real code
instead of retrying a lookup that could never succeed.
`try_wait()`'s return type simplified from `Option<ExitStatus>` to
`Option<i32>` (its only two callers only ever used the raw code /
existence, never other `ExitStatus` fields).

Verified: `stop_transitions_to_stopped_and_stays` dropped from ~3.5–8 s
(and a frequent timeout) to a consistent ~0.58 s across 5 clean runs.

## Root cause, part 1: unsynchronized fixture build (confirmed, fixed)

`ensure_fake_agent_built()` in `agent_runtime.rs` ran on every `#[test]`
that needed the `fake-agent` fixture binary:

```rust
fn ensure_fake_agent_built() {
    if terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_ok() {
        return;
    }
    // ... cargo build -p fake-agent ...
}
```

`cargo test` runs `#[test]` functions in the same binary on separate
threads concurrently. With no synchronization, multiple threads observe the
binary missing at the same instant, each starts its own `cargo build -p
fake-agent`, and a caller can attempt to spawn the binary in the narrow
window where a concurrent build's rename into `target/debug` hasn't landed
yet.

This reproduced locally as an outright `ENOENT` (binary doesn't exist) on a
clean `target/`, and reproduced in GitHub Actions — which always builds on
a clean runner — as the 5 s wait for the post-stop `Exited` event timing
out (a slower, more contended failure shape of the same underlying race:
build/spawn timing on a colder, differently-scheduled machine). This is a
**test-harness bug**, not a FlashTerminal product bug — no `AgentRuntime`
or `pty` code was implicated.

**Fix**: guard the build with `std::sync::Once` so exactly one thread
builds the fixture and every other caller blocks until it's done. Verified
with 3 consecutive clean-`target` local runs, all green
(`crates/terminal-session/tests/agent_runtime.rs`).

## Cargo feature audit

| Feature | Package | Gates | Requires | Belongs in Tier A CI? |
|---|---|---|---|---|
| `real-agents` | `terminal-session` | `tests/real_agents.rs` (real Claude Code / Codex / OpenCode / Pi CLI matrix) | agent binaries on PATH, credentials, network | **No** — was previously *always* built and run (no `required-features` gate existed on the `[[test]]` target, so the feature name was aspirational only), and `--all-features` enabled it explicitly. Fixed: `terminal-session/Cargo.toml` now declares `required-features = ["real-agents"]`, and CI's `Test` job dropped `--all-features`. Moved to `real-agents.yml`, `workflow_dispatch` only. |
| `gpu-bench` | `benchmarks` | nothing — no `#[cfg(feature = "gpu-bench")]` reference exists anywhere in `benchmarks/`. Currently a dead/placeholder feature. | N/A | N/A — inert either way; left as-is, noted here so it isn't mistaken for an active gate. |

`--all-features` vs normal, compared directly: the only behavioral
difference across the whole workspace is enabling `real-agents` (which,
before the `required-features` fix, changed nothing observable in CI since
the agent binaries are never on the GitHub runner's PATH and the suite
skips gracefully) and `gpu-bench` (compiles identically either way — no
`cfg` reads it). No test outcomes, warnings, or compile results differed.

## Benchmark CI-mode bugs (confirmed, fixed)

`benchmarks/src/main.rs` `main()` unconditionally called `save_baseline()`
— **every CI run overwrote the committed `benchmarks/baseline.json`**,
which defeats the purpose of a baseline comparison (a regression commit
would silently become the new baseline on its own CI run). Fixed: added
`--ci` flag; in `--ci` mode the baseline is read but never rewritten, and
local (non-CI) runs still auto-update it as before.

`rss_mb()` measures the *benchmark harness process's own PID*
(`std::process::id()`), not the FlashTerminal desktop application. This was
already an explicit, documented design choice in `sessions_ram_mb()` (real
PTY child sessions held in-process, not a naive extrapolation) — it is not
mistaken for desktop RSS anywhere in the code, but it is not truly desktop
RSS either. A genuine desktop-process RSS budget would require building
and measuring the actual release desktop binary, which in turn needs a
GUI-capable runner this repository does not have configured (§18/§19 of
the repair spec explicitly say not to force GUI launches onto a headless
runner). Documented as a known limitation in `docs/ci.md`; deferred to a
future `desktop-validation.yml` rather than bolted on here.

No `--ci` mechanism existed at all before this phase; no job/step timeouts
existed on the Performance job; no artifacts were uploaded on failure.
All three fixed — see `docs/ci.md`.

## Local-only flake observed (not a CI issue)

`crates/terminal-session/tests/phase051.rs::malformed_bytes_through_pty`
writes 64 KB of adversarial bytes through a real spawned shell PTY with a
30 s deadline. Under `cargo test --workspace` **on this development
machine only** (load average 3–6, ~780 concurrent processes from unrelated
sessions) it fails intermittently-to-consistently; run in isolation
(`cargo test -p terminal-session --test phase051 malformed_bytes_through_pty`)
it passes every time. It has **never appeared in any of the 11 GitHub
Actions runs inspected**. Classified per the wedge protocol in
`docs/benchmark-reliability.md`: OS/scheduling contention artifact of this
particular machine, not a FlashTerminal bug — left untouched (no timeout
change, no assertion change) rather than weakened. Flagged here for
visibility if it ever surfaces in actual CI.

## Repository hygiene (§24–§25)

`.gitignore` ignored only `target/`. Two tool-generated cache trees were
tracked in git and growing every commit:

- `.repowise/` — 103 files, ~19 MB (pickles, a LanceDB blob store, job
  JSON, `episodes.db`) — 100% regenerable index cache, zero source value.
- `graphify-out/` — 138 files, ~20 MB, including a `graph.json` that grew
  by 140,931 lines in a single commit (`1ebf6ca`) — a knowledge-graph
  analysis cache, also 100% regenerable.

Both were introduced by the commit `Track .codegraph, .claude, .pi,
graphify-out (gitignore reduced to target/ only)`. By contrast,
`.codegraph/` already self-excludes via its own `.codegraph/.gitignore`
(`*` / `!.gitignore`), and `.claude/CLAUDE.md` plus `.pi/skills/**` are
genuine hand-authored source docs — left tracked. Fixed: `.gitignore` now
excludes `.repowise/`, `graphify-out/`, and `.DS_Store`; the 247
previously-tracked generated/junk files were removed from the index (not
from disk).

## Action dependency versions

| Action | Before | After | Why |
|---|---|---|---|
| `actions/checkout` | `v4` | `v7` | `v4` is Node 20 (deprecated on GitHub-hosted runners, forced to Node 24 with a warning on every run); `v7` ships `node24` natively. |
| `actions/cache` | `v3` | `v6` | same Node 20→24 forcing issue; `v6` ships `node24` natively. |
| `actions/github-script` | `v7` | `v9` | same; `v9` ships `node24` natively. |
| `actions/upload-artifact` | not used | `v7` | new — added for benchmark failure artifacts (§16). |
| `dtolnay/rust-toolchain` | `@stable` | `@stable` (unchanged) | rolling tag, already current; no pin exists to bump. |

Verified via `gh api repos/<org>/<repo>/contents/action.yml?ref=<tag>` for
each new tag before adopting it (all four `runs.using: node24`).
