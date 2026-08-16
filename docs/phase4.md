# Phase 4 Report — Security, Policy, Autonomy, Recovery, Auditability

**Status:** `READY FOR PRODUCTION BETA`

Phase 4 turns FlashTerminal from "agents can run" into "agents can run
**while the user remains in control, can understand what happened, can
recover from failure, and can trust the system**". This report is the
§48 final report. Companion documents: `docs/security-model.md`,
`docs/policy-engine.md`, `docs/autonomy.md`, `docs/audit-trail.md`,
`docs/recovery.md`, `docs/benchmark-reliability.md`, ADRs
`0017–0020`, and `docs/screenshots/phase4/`.

---

## A. Security

**What agents can do** — inside an explicit, per-workflow scope (default
`WorktreeOnly`), under deterministic policy evaluation:

- Run commands and modify files **only within their declared filesystem
  scope** (`ProjectOnly | WorktreeOnly | Workspace | CustomPaths |
  NoFilesystem`, §7). A task's worktree is isolated per task.
- Perform actions classified **Low–High risk** up to the current
  autonomy level's automatic threshold; anything above requires human
  approval.
- Make network requests **only if the network policy permits**
  (`Blocked` by default; `Prompt` always asks, `Allow` is explicit).
- Reference secrets through `keychain://…` references — **never store
  or log credential material** (OS keychain is the only holder).
- Spend up to the budget caps (agents, tokens, cost, runtime, replans,
  commands, network requests) — hard caps, never exceeded.

**What agents cannot do:**

- **Dangerous commands** (`rm -rf`, `mkfs`, `dd`, sudo, credential/SSH
  access, destructive git, force push, mass deletion, …) are **denied
  or approval-gated by deterministic `DangerousRule`s** before anything
  else (§4). A `Deny` is never silently downgraded.
- **Escape their filesystem scope.** Path validation is a pure, tested
  rule — never LLM judgment.
- **Change policy or autonomy.** The planner cannot change filesystem
  scope, network policy, autonomy level, budgets, or agent caps
  (adversarial tests enforce this).
- **Self-approve** (approval integrity: IDs, expiry, action-hash
  binding, replay protection).
- **Fake completion.** Completed work that cannot be verified falls
  back to `NeedsReview` (adversarial test: `adversarial_fake_completion_requires_review`).
- **Exfiltrate.** Network policy blocks by default
  (`adversarial_network_exfiltration_denied`), and output redaction
  scrubs secrets at every boundary.

**Verification:** `tests/phase4` — 7 adversarial tests, all passing:
shell-injection literals, network exfiltration denied, planner cannot
change network policy, self-approval ignored, fake completion requires
review, invalid artifact denied, replan-v2 cannot weaken safety
constraints.

## B. Policy

Every executable action passes through one central `PolicyEngine`
(`policy.rs`), which returns exactly `Allow | Deny | RequireApproval`:

```text
action
  → deterministic dangerous-command rules      (Deny / gate)
  → category gates: scope, network, secrets    (Deny / waiver)
  → risk classification                        (Low…Critical)
  → autonomy matrix at current level           (auto ≤ threshold?)
  → unknown executable?                        (RequireApproval)
  → decision: Allow | Deny | RequireApproval
```

- **Deterministic rules come first.** Dangerous-command protection and
  path validation are pure rules — an LLM classifier may *inform* risk,
  but never *decides* a `Deny`.
- **Conservative by default.** Uncertainty → `RequireApproval`, never an
  optimistic `Allow`.
- **Source-tagged, auditable.** Every decision carries its `PolicySource`
  and lands in the audit trail.
- **The planner is never the final authority.** The policy engine sits
  above the planner (ADR 0017).

See `docs/policy-engine.md` and ADR `0017-policy-engine.md`.

## C. Autonomy

| Level | Max automatic risk | What runs automatically |
|-------|-------------------|-------------------------|
| `Manual` | Low | Nearly nothing; every risky action requires approval. |
| `Assisted` | Low | Low-risk actions run automatically. |
| `Supervised` | Medium | Medium-risk actions run automatically inside policy scope. |
| `Autonomous` | High | High automation within a strict sandbox and budget. |

- **Critical risk requires explicit approval at every level** — including
  `Autonomous`.
- `AutonomyLevel::auto_threshold()` is the only place a level changes
  behavior, and it can only move the *automatic* boundary between
  Low/Medium/High — Critical is never automatic.
- Autonomy changes are human-only decisions (`Action::AutonomyChange` →
  `RequireApproval`); the planner cannot change the level.
- Default: `Manual`.

See `docs/autonomy.md` and ADR `0018-autonomy-model.md`.

## D. Recovery

| Event | Behavior |
|-------|----------|
| **Agent dies** (§23) | Workflow stays valid; dependents block cleanly; artifacts and worktree records survive; failure is explicit (`TaskFailure` replan signal reaches the user). |
| **Planner dies** (§24) | Provider vanishing mid-flow is a typed, visible failure (`PlannerPhase::Failed`), never a half-created plan; scheduler untouched; a healthy provider is fully usable again. |
| **Provider fails** (§26) | Same surface as planner loss — typed failure, no silent corruption, retry from a healthy provider. |
| **Application crashes** (§25) | State is persisted; restart marks Running/Waiting tasks `Interrupted` (never silently resumed) and preserves plans + decisions. |
| **System sleeps** (§27) | Pause/resume preserves state consistency; running tasks are explicitly handled on resume. |
| **Corrupted persistence** (§28–§29) | Schema-versioned; corruption detection on load; never silently rewritten. |

Governing principle: **fail visibly, never silently corrupt, and recover
explicitly.** See `docs/recovery.md` and ADR `0020-recovery-model.md`.

## E. Auditability

- Every policy decision, approval, replan, escalation, and state
  transition is recorded with **who/what decided it and why**
  (`AuditEventKind` + source). Bounded in RAM; older history
  disk-backed.
- **Why-did-this-happen UX** surfaces the decision chain for any action
  (approval request → policy evaluation → outcome).
- Replan changes are diffed (`PlanDiff`), versioned (`PlanVersion`),
  and history is immutable across restarts.
- Live validation via the desktop IPC surface: `audit.log` (decisions +
  approvals), `audit.summary` (counts), `audit.replan` (replan history).

See `docs/audit-trail.md` and ADR `0019-audit-trail.md`.

## F. Performance

Phase 4 adds the policy/audit/recovery layers **without measurable
input-path cost** — multiplexer baseline is unchanged from Phase 3F
(`docs/performance-report.md`):

```text
workspace create: 100x avg ~2.5 ms     (<100 ms target: PASS)
tab create:       50x avg ~2.5 ms      (<100 ms target: PASS)
pane split:       49x avg ~2.7 ms      (<30 ms target: PASS)
stress 20 panes:  input p95 1.10 ms    (<8 ms target: PASS) · echo timeouts 0
focused input:    p95 0.18–1.81 ms     (<8 ms target: PASS) · timeouts 0 (0.0%)
flood:            ~800–2700 batches/s · apply p95 ~100–146 µs
orchestration:    wide scales cap1 48 → cap10 296 t/s; RSS 12–24 MB
agent_stress:     ALL PASS (48.8k events/s, p95 16.4 µs)
```

The focused-input p95 bimodality (~0.2 ms vs ~1.8 ms) is the Phase 3F
documented shell/TTY bimodality — nowhere near the 8 ms target.

## G. Benchmark Reliability

```text
20-run success rate: 20/20
timeouts:           0
hangs:              0
outliers:           0 FAIL lines across all 20 runs
wedge classifications: 0 (the documented Phase 3F shell/TTY bimodality
                         is observed, not classified as a wedge)
```

Every run emitted the `DONE` marker with exit 0; all run logs preserved.
See `docs/benchmark-reliability.md`.

## H. UX

The desktop app (`--demo` mode) is validated end-to-end through its live
IPC surface against a running app: PAUSE ALL, RESUME ALL, STOP ALL
(7 agents stopped, task + 3 decisions preserved), approval requests
(4 human decisions), workflow summary, and audit surfaces
(`audit.log` / `audit.summary` / `audit.replan`). Screenshots and the
evidence notes are in `docs/screenshots/phase4/` (README explains the
wgpu/Metal capture limitation — identical to Phase 3A.1; on-screen
rendering and IPC behavior are the validated evidence).

## I. Remaining Risks

1. **wgpu surface capture** — `screencapture` cannot composite the
   wgpu/Metal layer without Screen Recording permission (same as Phase
   3A.1). Screenshots show the window chrome; live IPC validation is the
   authoritative evidence. Granting Screen Recording permission yields
   pixel-perfect captures.
2. **Focused-shell bimodality** — documented Phase 3F behavior, always
   far under target; wedge protocol + forensic dump preserved if it ever
   recurs.
3. **Policy/audit storage** — bounded in RAM with disk-backed history;
   long-running workflows should confirm retention expectations.
4. **Provider-dependent paths** (planner/provider loss) are covered by
   tests with real subprocess crashes; behavior with third-party
   providers under real network conditions should be spot-checked before
   wide beta.
5. **Secrets** rely on the OS keychain backend; keychain availability /
   unlock prompts are the one external dependency.

## J. Final Decision

```text
READY FOR PRODUCTION BETA
```

All release gates pass: full workspace test suite (354 tests, 0
failures), `clippy -D warnings`, `cargo fmt --check`, release build,
and the 20-run benchmark reliability gate. The §48 determinism flake
(`determinism_ten_runs_produce_identical_schedules`) is fixed at the
root: exits were observed on two independent threads (PTY reader EOF and
the pump's 25 ms state poll), so co-exiting tasks could land in
different engine frames under load. Three changes eliminate the race:
(1) the reader forwards an EOF tap sentinel so pumps notice exits
immediately, (2) the engine's runtime view treats authoritative
`try_wait` as exited, and (3) the scheduler defers terminal transitions
while a co-started sibling is still settling, bounded by a 100 ms
window so a slow-starting task can never stall completions. Verified
with 25 consecutive full-suite runs under parallel load (0 failures;
the pre-fix rate was ~60%).

---

**Final principle honored:** FlashTerminal optimizes for *"the agent
successfully did something while the user remained in control, could
understand what happened, could recover from failure, and could trust
the system."* Performance, human control, security, and transparency
remain non-negotiable.
