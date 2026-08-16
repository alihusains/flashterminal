# CI

See `docs/ci-forensics.md` for the audit that produced this design and the
evidence behind each decision below.

## Workflow structure

```text
.github/workflows/
├── ci.yml            # Tier A — always runs on push/PR to main
└── real-agents.yml    # Tier B — workflow_dispatch only
```

`ci.yml` jobs, all independent (no `needs:` chain) so they run in parallel
and fail fast individually:

```text
CI
├── check          cargo check --workspace
├── test           cargo test --workspace
├── fmt            cargo fmt --all -- --check
├── clippy         cargo clippy --workspace --all-targets -- -D warnings
├── release-build  cargo build --release --workspace
├── desktop-build   cargo build --release -p desktop   (build-only, no launch)
└── performance    build + run + compare against committed baseline
```

## Test tiers

**Tier A — deterministic CI** (`ci.yml`, always on): no network, no real AI
agent binaries, no credentials, no interactive GUI. `cargo test --workspace`
deliberately omits `--all-features` — the only feature it would add is
`real-agents` (Tier B, below). `gpu-bench` (in `benchmarks`) is currently an
inert placeholder feature with no `cfg` reference anywhere in the crate.
Runs with `TERM: xterm-256color` (workflow-level `env:`) and
`RUST_TEST_THREADS=1` on the `Test` job:

- **`TERM`**: several integration tests spawn `vim`/`less`/`top` through a
  real PTY and assert they enter the terminfo alternate screen. GitHub job
  steps run with no `TERM` set, so those programs correctly decline
  alt-screen mode for what looks like an incapable terminal — every such
  test failed on every run until this was set. Confirmed root cause: see
  `docs/ci-forensics.md`.
- **`RUST_TEST_THREADS=1`**: a smaller, separate contention issue —
  several real-PTY tests running concurrently on a small runner can push
  one past its own timeout. `timeout-minutes: 15` on the job covers the
  ~2-3 extra minutes serial execution costs.

**Tier B — optional integration** (`real-agents.yml`, `workflow_dispatch`
only): exercises `terminal-session`'s `real-agents` feature against the
actual `claude-code` / `codex` / `opencode` / `pi` CLIs
(`crates/terminal-session/tests/real_agents.rs`). Requires those binaries
on the runner's PATH and, for a non-`SKIPPED` result, provider credentials.
The test suite prints `SKIPPED` (not `FAILED`) per agent when its binary
isn't found — never a false failure for an absent optional dependency.
Gated at the Cargo level: `terminal-session/Cargo.toml` declares
`required-features = ["real-agents"]` on this `[[test]]` target, so it
cannot be built, let alone run, without the feature explicitly enabled.

**Tier C — desktop/manual**: GPU, window, sleep/wake, TUI, interactive UI.
Not yet a distinct workflow — `desktop-build` in `ci.yml` only compiles
`apps/desktop` in release mode; it never launches the app, since GitHub's
`macos-latest` runner has no attached display. A future
`desktop-validation.yml` (manual or self-hosted-runner-gated) is the right
home for actually launching and driving the app; out of scope here per the
repair spec (§18/§19 — make the headless jobs stable first).

## Performance benchmarks

```bash
cargo run --release -p benchmarks -- --ci
```

- **`--ci`** (used by CI only): reads `benchmarks/baseline.json`, compares
  the current run against it, prints the JSON report + comparison table,
  and **never overwrites the baseline file**. Exits non-zero only when a
  *hard* budget (idle RAM, 10-pane RAM, input-latency p95) is breached.
- **without `--ci`** (local/manual): behaves as before — updates
  `benchmarks/baseline.json` and `docs/performance-report.md` after every
  run, so the next local run compares against fresh numbers.
- A metric that can't be measured (e.g. no monospace font found for glyph
  rasterization) reports as `NaN` → printed as `UNAVAILABLE`, distinct from
  `FAIL`; `NaN > budget` is always false in Rust, so an unavailable metric
  never fails the run.
- **RSS semantics**: `idle_ram_mb` / `ten_panes_ram_mb` measure the
  benchmark harness process while it holds real, in-process
  `terminal-session::Session`s backed by real PTY child shells — not the
  FlashTerminal desktop app's process RSS. This is close to (not identical
  to) the terminal-engine's own memory footprint, and is *not* used as a
  substitute claim for desktop RSS anywhere. A true desktop-process RSS
  budget needs a GUI-capable runner FlashTerminal doesn't have configured
  yet (see Tier C above) — tracked as a known gap, not silently proxied.
- **Timeouts**: job-level `timeout-minutes: 20`; step-level 10 min
  (release build) and 5 min (benchmark run). A timeout is a CI **failure**,
  never a silent pass.
- **Flakiness**: the multiplexer benchmark's historical PTY-fixture-wedge
  classification protocol lives in `docs/benchmark-reliability.md` (capture
  forensic dump → determine shell/PTY/reader/drain state → classify as
  fixture artifact / OS-TTY behavior / FlashTerminal bug). Apply the same
  protocol to any future performance-job hang before assuming a product
  regression.
- **Artifacts**: `/tmp/perf_report.txt` and `docs/performance-report.md`
  are uploaded via `actions/upload-artifact@v7` on every run (`if: always()`),
  14-day retention. CI never commits generated benchmark output.

## Permissions

Workflow default: `permissions: contents: read`. The `performance` job
additionally grants `pull-requests: write` (needed for the PR comment
step) — no other job or step has elevated permissions. The PR-comment step
itself wraps `github.rest.issues.createComment` in try/catch with
`continue-on-error: true`: a fork PR's `GITHUB_TOKEN` is read-only against
the base repo regardless of the grant above, so a failed comment logs the
report to the job log instead of failing an otherwise-successful
benchmark.

## Caching

Keyed on `runner.os` + toolchain channel + `Cargo.lock` hash, e.g.
`${{ runner.os }}-cargo-stable-${{ hashFiles('**/Cargo.lock') }}`, with
`restore-keys` fallbacks (`...-stable-`, then `...-cargo-`) so a
`Cargo.lock` change still gets a warm start instead of a fully cold build.
Separate keys per job (`-stable-`, `-clippy-`, `-release-`, `-desktop-`,
`-perf-`, `-real-agents-`) avoid different jobs thrashing one shared cache
entry. Never caches secrets, machine-specific data, or generated reports.

## Concurrency

```yaml
concurrency:
  group: ci-${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true
```

A new push to a PR cancels that PR's in-flight run. `main` only ever has
one active head, so this doesn't cancel a "genuine" main-branch
verification run out from under itself in practice.

## Action dependency versions

See `docs/ci-forensics.md` for the audit table. `actions/checkout@v7`,
`actions/cache@v6`, `actions/github-script@v9`, `actions/upload-artifact@v7`
— all confirmed `node24`-native (resolves the prior Node 20 deprecation
warnings on every run). `dtolnay/rust-toolchain@stable` is an unpinned
rolling tag, already current.

## Repository hygiene

`.gitignore` now excludes `.repowise/` and `graphify-out/` (tool-generated
caches — indexes, LanceDB blobs, analysis JSON, fully regenerable) and
`.DS_Store`. `.codegraph/` already self-excludes via its own nested
`.gitignore`. `.claude/CLAUDE.md` and `.pi/skills/**` are genuine
hand-authored docs and remain tracked.

## Failure policy

- A genuine performance regression against the committed baseline: `FAIL`,
  with the comparison table in the job log / artifact / PR comment.
- An unavailable optional external agent (Tier B): `SKIPPED`, never a
  failure.
- A timeout on any job/step: `FAILURE`, never a silent pass.
- Never: removed tests, indefinitely-widened timeouts, silenced clippy
  warnings, or a disabled performance check, to make a run go green.
