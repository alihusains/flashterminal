# FlashTerminal GitHub Actions Forensic Audit + CI Repair

The GitHub repository is:

```text
https://github.com/alihusains/flashterminal
```

Current repository state must be inspected from the actual checkout.

The current `.github/workflows/ci.yml` contains five jobs:

```text
Check
Test
Rustfmt
Clippy
Performance
```

The workflow currently uses:

```yaml
runs-on: macos-latest
```

and:

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo run --release -p benchmarks
```

The local Phase 4 release gates were:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --release --workspace
```

GitHub Actions is currently failing.

Your task is to perform a forensic investigation and repair the CI system.

Do NOT change product behavior.

Do NOT start Phase 5.

Do NOT modify terminal architecture.

Do NOT simply weaken or disable failing checks.

The goal is:

> Make GitHub Actions accurately validate the same engineering guarantees we care about locally.

---

# 1. Establish Current GitHub State

Inspect:

```bash
git status
git branch --show-current
git log --oneline -10
```

Inspect:

```text
.github/workflows/
```

Determine the actual workflow files.

---

# 2. Inspect GitHub Actions Runs

Use the GitHub CLI if available:

```bash
gh auth status
gh run list --workflow CI --limit 20
```

If authentication is unavailable, inspect the public repository and workflow history.

Determine for each recent run:

```text
run number
commit SHA
job
status
conclusion
failed step
error message
duration
```

Do not guess.

Create:

```text
docs/ci-forensics.md
```

with the actual failure evidence.

---

# 3. Reproduce Each Failure Locally

Run the exact CI commands.

## Check

```bash
cargo check --workspace
```

## Test

```bash
cargo test --workspace --all-features
```

## Format

```bash
cargo fmt --all -- --check
```

## Clippy

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Performance

```bash
cargo build --release -p benchmarks
cargo run --release -p benchmarks
```

Record which commands differ from the Phase 4 release gates.

---

# 4. Compare All-Features vs Normal

Run:

```bash
cargo test --workspace
cargo test --workspace --all-features

cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Compare:

```text
tests
compilation
warnings
features activated
execution time
```

If `--all-features` causes failures that normal CI does not, determine why.

Do NOT simply remove `--all-features` without understanding what functionality it was intended to test.

---

# 5. Audit Cargo Features

Inspect every workspace feature.

Pay particular attention to:

```text
real-agents
gpu-bench
```

Determine:

```text
which package defines it
what code it activates
whether it belongs in normal CI
whether it requires external binaries
whether it requires credentials
whether it requires hardware
```

The normal CI test suite must NOT require:

```text
Claude Code
Codex
OpenCode
Pi
API keys
network access
GPU/display
```

unless there is a dedicated opt-in integration job.

---

# 6. Separate Test Tiers

Create three conceptual CI test tiers.

## Tier A: Deterministic CI

Must always run:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

No:

```text
network
real AI agents
credentials
interactive GUI
```

## Tier B: Optional integration

Separate:

```text
real agent tests
provider connectivity
```

These may be manually triggered or require configured secrets/binaries.

## Tier C: Desktop/manual

Separate:

```text
GPU
window
sleep/wake
TUI
interactive UI
```

Do not make desktop UI testing depend on a headless Linux runner.

---

# 7. Fix Performance Benchmark Semantics

Inspect:

```text
benchmarks/src/main.rs
```

Current RSS measurement uses the benchmark process itself.

Determine whether this measures:

```text
benchmark harness RSS
```

or:

```text
FlashTerminal desktop application RSS
```

Do not use benchmark-harness RSS as a proxy for desktop application RAM.

Separate metrics:

```text
benchmark process RSS
desktop app RSS
process-tree RSS
```

The hard product budget should apply to the actual relevant target.

---

# 8. Make CI Performance Deterministic

The performance CI command must:

```text
build
run
measure
compare
report
```

It must NOT silently rewrite the baseline.

Do not overwrite:

```text
benchmarks/baseline.json
```

in CI.

CI should compare current measurements to a committed baseline.

Local benchmark mode may update the baseline explicitly.

---

# 9. Create CI Benchmark Mode

Implement:

```bash
cargo run --release -p benchmarks -- --ci
```

or an equivalent explicit mechanism.

CI mode must:

* avoid modifying committed baselines
* print machine-readable JSON
* exit nonzero only on real hard-budget breaches
* distinguish unavailable measurements from failures
* preserve logs
* make failures reproducible

---

# 10. Environment-Aware Memory Tests

Do not assume GitHub-hosted macOS has identical process RSS behavior to the development Mac.

Separate:

```text
absolute product budget
```

from:

```text
benchmark harness overhead
```

A benchmark environment should not fail the product because the harness itself consumed additional memory.

If an application-level memory test is required, build the actual release desktop binary and measure that process/process-tree.

---

# 11. Performance Job Timeouts

Add explicit job and step timeouts.

Do not allow a hung benchmark to consume an uncontrolled amount of CI time.

Use sensible values based on the benchmark's actual runtime.

A timeout must be treated as:

```text
CI FAILURE
```

not:

```text
PASS
```

---

# 12. Benchmark Flakiness

The multiplexer benchmark has historically exhibited a rare PTY fixture wedge.

Do not silently suppress this.

CI should:

1. run the deterministic benchmark
2. capture forensic diagnostics on timeout
3. classify the failure
4. fail if FlashTerminal itself is responsible

If the issue is an OS/TTY fixture artifact, report it separately.

---

# 13. PR Comment Permissions

The current performance job uses:

```yaml
actions/github-script@v7
```

to call:

```javascript
github.rest.issues.createComment(...)
```

Add the minimum required explicit workflow permissions.

For example, evaluate whether:

```yaml
permissions:
  contents: read
  issues: write
  pull-requests: write
```

is required.

Do not grant unnecessary permissions.

Ensure the workflow behaves correctly for:

```text
push
same-repository pull request
fork pull request
```

A performance comment should never make an otherwise successful benchmark fail.

If commenting is impossible because of fork permissions:

```text
log the report
```

rather than failing the benchmark.

---

# 14. Upgrade Action Dependencies

Review:

```text
actions/checkout
actions/cache
actions/github-script
dtolnay/rust-toolchain
```

Upgrade stale major versions where appropriate.

Do not perform blind upgrades.

Verify compatibility.

---

# 15. Cache Design

Use cache keys that account for:

```text
runner OS
Cargo.lock
Rust toolchain
```

Add fallback restore keys where useful.

Do not cache:

```text
secrets
machine-specific data
generated reports
```

---

# 16. CI Artifact Handling

On benchmark failure, upload:

```text
performance report
JSON output
benchmark logs
forensic diagnostics
```

Use GitHub Actions artifacts.

Do not commit generated benchmark output from CI.

---

# 17. Add a Dedicated Build Job

Create a release build validation:

```bash
cargo build --release --workspace
```

This job should be independent from performance benchmarks.

Its responsibility is simply:

> Does the entire release workspace build?

---

# 18. Desktop Build Job

Since FlashTerminal is a desktop application, create a separate job that verifies:

```bash
cargo build --release -p desktop
```

Do not require the graphical application to launch in headless CI unless a real GUI-capable runner is configured.

---

# 19. Platform Matrix

Do not immediately expand every job to every OS.

First make macOS stable.

Then document the intended future matrix:

```text
macOS
Linux
Windows
```

Eventually:

```text
core tests → all platforms
desktop build → all supported platforms
GPU/UI tests → platform-specific
```

---

# 20. Normal CI Workflow

The final workflow should look approximately like:

```text
CI
├── check
├── test
├── fmt
├── clippy
├── release-build
└── performance
```

Optional workflows:

```text
real-agents.yml
desktop-validation.yml
release.yml
```

Do not put everything into one enormous workflow.

---

# 21. Dependency Between Jobs

Keep independent static jobs independent where possible.

Do not make:

```text
fmt → test → clippy → build
```

unless there is a real dependency.

Parallel jobs provide faster feedback.

---

# 22. Concurrency Cancellation

For pull requests, consider:

```yaml
concurrency:
  group: ci-${{ github.workflow }}-${{ github.event.pull_request.number || github.ref }}
  cancel-in-progress: true
```

This prevents obsolete runs from consuming runner time.

Do not cancel main-branch verification runs inappropriately.

---

# 23. CI Security

Use:

```yaml
permissions:
  contents: read
```

by default.

Grant additional write permissions only to the specific step/job requiring them.

Never expose:

```text
API keys
credentials
GITHUB_TOKEN
```

to arbitrary benchmark subprocesses.

---

# 24. Generated Repository Artifacts

Inspect the repository for:

```text
.repowise/
graphify-out/
.DS_Store
local caches
generated databases
```

Determine which belong in Git.

Do not delete useful source documentation automatically.

The objective is to prevent tool-generated caches from polluting source control.

Create/update `.gitignore` where appropriate.

---

# 25. Important Current Repository Observation

The GitHub repository currently has a later commit after `ef188ba`:

```text
1ebf6ca
```

and GitHub shows that commit as the latest Phase 4 push.

That commit contains a very large amount of generated/tooling content.

Do not assume this is intended.

Inspect it and document whether these files should remain tracked.

Do not rewrite history during this task unless explicitly instructed.

---

# 26. Do Not Solve CI Failures By Weakening Gates

Never:

```text
remove tests
increase timeouts indefinitely
ignore failures
turn warnings off
remove performance checks
```

The objective is:

> accurate CI, not green CI at any cost.

---

# 27. Required Final Workflow Behavior

For a healthy commit:

```text
Check         ✅
Test          ✅
Rustfmt       ✅
Clippy        ✅
Release Build ✅
Performance   ✅
```

For a genuine performance regression:

```text
Performance   ❌
```

with a clear report.

For an unavailable optional external agent:

```text
SKIPPED
```

not a misleading failure.

---

# 28. Add Workflow Validation

Use a YAML/workflow validation tool where practical.

At minimum verify:

```text
YAML parses
workflow discovered
all actions resolve
permissions are valid
jobs have valid dependencies
```

---

# 29. Verify Locally Before Committing

Run the exact commands used by CI.

Then use a local CI emulation mechanism if available.

Do not commit the workflow until:

```text
local CI commands
and
workflow intent
```

match.

---

# 30. Final Documentation

Create:

```text
docs/ci.md
```

Document:

```text
workflow structure
test tiers
performance benchmarks
permissions
caching
artifacts
real-agent integration
desktop build
failure policy
```

---

# 31. Definition of Done

CI repair is complete only when:

```text
✓ Actual GitHub failures identified
✓ Exact failed steps documented
✓ Local reproduction performed
✓ Root causes established
✓ Test tier separation implemented
✓ all-features behavior understood
✓ performance semantics corrected
✓ benchmark CI mode created
✓ baseline no longer overwritten in CI
✓ PR comment permissions corrected
✓ action versions reviewed
✓ caching corrected
✓ release build job added
✓ desktop build validation added
✓ benchmark artifacts uploaded on failure
✓ timeouts added
✓ concurrency policy added
✓ CI security permissions minimized
✓ generated repository artifacts reviewed
✓ GitHub Actions passes on a fresh commit
```

---

# 32. Final Report

Return:

## GitHub Run Analysis

For the last 10 runs:

```text
run
commit
job
status
failure
root cause
```

## Root Causes

Separate:

```text
workflow bug
benchmark bug
product bug
environment issue
test issue
```

## Files Changed

List every CI-related change.

## Validation

Show:

```text
cargo test
cargo clippy
cargo fmt
cargo build
benchmark
```

## GitHub Actions

Confirm actual GitHub result:

```text
Check       PASS/FAIL
Test        PASS/FAIL
Fmt         PASS/FAIL
Clippy      PASS/FAIL
Build       PASS/FAIL
Performance PASS/FAIL
```

## Final Decision

Return exactly:

```text
CI HEALTHY
```

or:

```text
CI NOT HEALTHY
```

Do not start Phase 5 until CI is healthy.
