# Audit — from "CI is red" to a repaired, evidence-backed pipeline

## Ideal state

A completed audit produces, in order:

1. **A forensic record** (e.g. `docs/ci-forensics.md`) of the actual GitHub Actions history — which runs failed, on which job/step, with the real error extracted from the log, not summarized from memory.
2. **A confirmed local reproduction** of each distinct failure, using the exact commands and environment CI uses.
3. **One classification per failure**: product bug / test-harness bug / environment difference / measurement flaw — each backed by the evidence that ruled out the other three.
4. **A minimal, targeted fix per failure**, verified locally, then pushed and watched through a complete real CI run.
5. **A final state** where either CI is fully green across a repeated check, or the remaining red is explicitly attributed and deferred with a stated reason (scope boundary, follow-up needed) — never silently left unexplained.

## Constraints

- Pull the last 10–20 runs' real logs before forming any hypothesis. A guess that happens to be right is still a guess — verify against the log.
- Fix one failure class at a time. A local pass after a fix is a checkpoint, not a conclusion — push and watch the actual CI run before moving to the next failure.
- When a fix resolves one failure and CI still fails, that's expected in a chain of masked bugs — return to step 1 (fresh log) rather than assuming the new failure is unrelated noise.
- If reproducing locally requires installing a toolchain or other nontrivial environment change, ask first.
- Never resolve a discrepancy by loosening the check unless the evidence justifies it (see GateDesign.md for the performance-gate case specifically).

## Tools

- `gh run list --workflow <name> --limit 20`, `gh run view <id> --log-failed`, `gh run rerun <id> [--failed]`
- The project's own test/build/lint commands, run with the same flags and environment variables CI uses
- `git stash` to toggle a fix off/on and prove a new regression test actually catches the bug (fails without the fix, passes with it)

## Output-format contract

The forensic record must let a future reader answer, for every historical failure: *what failed, on which run, why (root cause, in the reader's own words not just a log excerpt), what was fixed and how it was verified, and what — if anything — was deliberately left unfixed and why.* A record that's just pasted logs is not a forensic record.
