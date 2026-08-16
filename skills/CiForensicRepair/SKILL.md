---
name: CiForensicRepair
version: 1.0.0
description: Diagnose and repair a flaky, broken, or silently-lying GitHub Actions CI using evidence from actual run history rather than guesswork. USE WHEN CI is failing, flaky, red, intermittent, or the user asks for a CI audit, CI forensic repair, or "why does GitHub Actions keep failing". Also covers a flaky performance/benchmark CI gate. NOT FOR designing a new CI pipeline from scratch (that's ordinary DevOps work) or one-off local test debugging with no CI involvement.
---

# CiForensicRepair

CI that's "randomly red" is never random — it's evidence you haven't read yet. This skill turns "CI keeps failing" into a repaired, green, *trustworthy* pipeline, and into a build that stays trustworthy when the next hidden bug surfaces.

## Workflow Routing

| Trigger | Workflow |
|---|---|
| CI is red/flaky and the root cause is unknown | `Workflows/Audit.md` |
| A performance/benchmark CI gate is flaky or its threshold is in question | `Workflows/GateDesign.md` |

## The ideal state

A repaired CI is one where:
- Every historical failure is explained by evidence (`gh run view --log-failed`), not assumption — never patch a hypothesis you haven't confirmed against the actual failing log.
- Each fix is verified against a **fresh, real CI run**, watched to completion — a local pass is necessary, never sufficient.
- The pipeline reports the truth: no step's real exit code is masked (a piped command whose failure silently returns 0 is worse than no check at all), and no optional/unavailable dependency reports as a failure instead of a skip.
- Every failure is attributed to exactly one class — **product bug**, **test-harness bug**, **environment difference**, or **measurement-methodology flaw** — because the fix for each is different and fixing the wrong class either doesn't work or hides a real defect.
- A performance/benchmark gate's threshold is backed by a measured distribution, not intuition, and the CI **regression-detection threshold** is allowed to differ from the **engineering target** it protects.
- The forensic trail (what failed, why, what was fixed, what was deliberately deferred and why) is written down somewhere durable in the repo — the next person (or the next you) shouldn't have to re-derive it.

## Constraints

- Never weaken a check to make it pass: no deleted tests, no indefinitely widened timeouts, no silenced warnings, no gate marked "informational" to dodge a real finding.
- Never change a performance/regression budget before the evidence (a real distribution, a real environment comparison) justifies it.
- Respect any explicit scope boundary the task states (e.g. "don't touch the architecture", "don't start the next phase") — defer out-of-scope findings **transparently**, documented as such, rather than silently fixing them or silently sitting on them.
- A fix that "looks done" after one green run isn't verified — rerun or push a fresh commit and watch it again; the previous fix in a chain often unmasked a second bug that was hiding behind the first (a suite that dies on failure A never reaches failure B).

## Tools

- `gh run list` / `gh run view --log-failed` / `gh run rerun` — the actual evidence; always start here, across the last 10–20 runs, before touching any code.
- The exact CI commands, run locally, in a real release/test build — if the toolchain isn't installed locally, ask before installing one.
- `git stash` (temporarily reverting a fix) to prove a regression test actually reproduces the bug it claims to fix, then restore it.

## Gotchas

- **A pipe swallows exit codes.** `cargo run | tee out.txt` (or any `X | Y`) reports the *last* command's exit status by default — a broken `X` can report as a passing step forever. Check for `set -o pipefail` (or the CI system's equivalent) whenever a step pipes through `tee`/`grep`/anything.
- **CI runners are not your dev machine.** Missing `TERM`, fewer cores, a different OS default, or a different available-binaries set can make a test fail on CI while passing locally — and vice versa. Reproduce the *exact* CI environment variables/toolchain before concluding "it's a real bug," and reproduce the *exact* failure locally before declaring victory.
- **Fixing bug #1 can unmask bug #2 unchanged since forever.** A test suite that always died early on one failure has never actually exercised what comes after it. Don't be surprised when a "fully fixed" CI reveals a new failure on the very next push — that's the audit working, not a regression you introduced.
- **A benchmark that "sometimes fails" is a measurement problem until proven otherwise.** Don't retune the number first. Run N repeated trials, get the real distribution (p50/p90/p95/p99/stddev), and only then decide: keep the gate, redesign *how the gate is applied* (e.g. "fail after 2 consecutive regressions" instead of "fail on any single sample over X"), or accept there's a genuine regression.
- **The CI regression threshold and the engineering target are two different numbers with two different jobs.** An engineering budget ("p95 ≤ 8ms is our real target") can stay fixed while the CI gate that *detects a regression against it* is redesigned to tolerate measured environment noise — these are not required to be the same value.
- **A single green run proves nothing about flakiness.** If the bug being chased is intermittent, the only real proof is repetition: rerun the same commit, or push a fresh no-op commit, and watch several runs.
