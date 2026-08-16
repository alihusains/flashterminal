# CI Forensic Audit

Evidence gathered from the actual GitHub Actions history at
`github.com/alihusains/flashterminal`, plus local reproduction, ahead of the
CI repair in this phase. See `docs/ci.md` for the resulting design.

**Outcome: run `31944416318` (commit `5d6b9d2`) is the first fully green
GitHub Actions run in this repository's history** — Check, Test, Rustfmt,
Clippy, Release Build, Desktop Build, and Performance Check all passed
(`4m8s` total). It took four root causes across four pushes to get there;
each is documented below in the order it was found, since each failure
only became reachable once the one before it was fixed.

**A follow-up rerun of the same commit surfaced a fifth, distinct issue**
(below, "Root cause, part 4") in an orchestration test file the suite had
never reached before in this repository's history. Unlike the first four,
this one is **not fixed** in this phase — see that section for why.

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

## Root cause, part 3: `TERM` unset on the runner — TUI tests never see alt-screen (confirmed, fixed)

With the first two fixes pushed (`b3919c1`, run `31942971604`), the `Test`
job finally got *past* `agent_runtime.rs` for the first time in this
repository's CI history — and immediately hit **four new failures**, all
in `crates/terminal-session/tests/phase051.rs`: `malformed_bytes_through_pty`,
`less_roundtrip`, `top_batch_and_interactive`, `vim_roundtrip`. None of
this had ever been observed before simply because `agent_runtime.rs` (Root
causes 1/2) always failed the `Test` job first.

An initial hypothesis (CPU contention: many real-PTY tests running
concurrently on a small runner) was plausible and *partially* correct —
`RUST_TEST_THREADS=1` was pushed (`69b5ff4`) as a legitimate determinism
improvement (it eliminates genuine same-binary thread contention and is
kept) — but it did **not** fix the failure on the actual GitHub runner: the
identical four tests failed again, this time fully serialized, each
timing out on its own with no other test running concurrently.

The actual log output pinpointed it: `less_roundtrip`'s captured grid
showed `less` printing `WARNING: terminal is not fully functional` and
never switching to the alternate screen; `vim_roundtrip` and
`top_batch_and_interactive` showed `TUI never entered alternate screen`.
These three tests assert `state.modes.alt_screen` becomes true — driven
entirely by whether the spawned `vim`/`less`/`top` process emits the
terminfo alt-screen escape sequence, which each program only does when its
`TERM` reports a capable terminal. Grepping the codebase confirms
`FlashTerminal` never sets `TERM` when spawning a session
(`Session::spawn`/`spawn_with_options` pass through the inherited
environment `+ env` additions — `crates/terminal-session/src/lib.rs`);
GitHub Actions job steps run with no interactive TTY and no `TERM` set.

Reproduced directly and unambiguously: `env -u TERM cargo test -p
terminal-session --test phase051 roundtrip` locally reproduces the exact
`less` warning and both `vim`/`less` failures; `env -u TERM
TERM=xterm-256color cargo test ...` (same command, `TERM` forced) passes
both in 3.46 s. `malformed_bytes_through_pty` doesn't touch `alt_screen` at
all and is a separate, genuine timing/resource-contention flake, which
`RUST_TEST_THREADS=1` addresses on its own terms.

This is a CI *environment* gap (§10 of the repair spec: don't assume
GitHub-hosted macOS matches the dev machine), not a FlashTerminal
correctness bug and not something to "fix" in product code — a terminal
emulator not forcing its own `TERM` on child shells is normal (the shell's
login profile/interactive `TERM` setup is what would normally supply it;
CI job steps have no such profile). Setting a capable `TERM` for the CI
job is the correct scope: it makes the *test environment* match what any
real interactive shell session already provides, without touching
`crates/pty` or `crates/terminal-session` at all. Fixed: `ci.yml`'s
top-level `env:` now sets `TERM: xterm-256color`.

## Root cause, part 4: orchestration event-coalescing races (found, NOT fixed — out of scope)

After the `TERM` fix landed and one full green run was confirmed
(`31944416318`), a docs-only follow-up commit (`0740cda`, no code change)
triggered a fresh run that failed again — this time in
`crates/terminal-workspace/tests/phase3d/main.rs`, a file the `Test` job
had *never reached* in any prior run (every earlier run died in
`agent_runtime.rs` or `phase051.rs` first). Rerunning the identical commit
a second time failed a *different* test in the same file
(`access_control_denies_unrelated_tasks`, then `cross_worktree_consumption`
on the next attempt) — genuinely intermittent, not deterministic.

Both failing tests share a shape: they subscribe to `EventBus`, drive a
task through the fake-agent to completion, call `events.flush()` once,
then `rx.try_iter()` to collect all delivered `AgentEvent::Output` text and
assert it contains an expected substring (e.g. `"cannot read secret.txt"`).
`EventBus::publish` (`crates/terminal-workspace/src/events.rs`) explicitly
**coalesces** `Output` events: it keeps only the *latest* event per
execution id in a `pending_output` map, replacing whatever was there
before, and only actually enqueues it to the subscriber on the next
`flush()` — deliberate, documented behavior ("a burst of output from one
agent becomes one subscriber message"). If the fake-agent's stdout
producing the assertable line and a later line both arrive before the
engine's next per-frame flush, the coalescing map overwrites the earlier
line and it never reaches any subscriber — a genuine race between I/O
chunk boundaries (how the reader thread happens to split the child's
output across syscalls) and the engine's flush cadence, both of which are
CPU-scheduling-dependent and therefore differ between this development
machine and GitHub's runner.

**Not fixed in this phase.** A correct fix here means changing how
`AgentEvent::Output` coalescing works (e.g. concatenating text across a
flush window instead of replacing) — that's a change to the orchestration
engine's event-delivery architecture, not a CI script, benchmark, or test
fixture. The repair spec for this phase is explicit: don't modify terminal
architecture, don't start Phase 5. This is real, and it's the correct kind
of thing this audit should surface — but it belongs to a dedicated
investigation into `EventBus` output-coalescing semantics, not a
workflow-level CI fix. Flagged here for that follow-up; not patched around
with a test change that would just paper over the same underlying race.

## `malformed_bytes_through_pty` — initial local-only observation, later explained

Before the `RUST_TEST_THREADS=1` fix above,
`crates/terminal-session/tests/phase051.rs::malformed_bytes_through_pty`
(64 KB of adversarial bytes through a real shell PTY, 30 s deadline) failed
intermittently-to-often under `cargo test --workspace` on a 12-core
development machine but passed every time in isolation. At that point it
had never appeared in the 11 historical GitHub Actions runs, so it was
provisionally classified as a local-machine contention artifact. Root
cause 3 (above) supersedes that: it's the same "real-process test loses
the CPU scheduling race under concurrent execution" pattern as the four
`phase051.rs` failures that surfaced in CI once the suite finally got past
`agent_runtime.rs` — not a machine-specific fluke, and now fixed by the
same `RUST_TEST_THREADS=1` change rather than left as an open question.
Classification protocol reference: `docs/benchmark-reliability.md`'s wedge
dump / fixture-artifact-vs-product-bug steps.

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
