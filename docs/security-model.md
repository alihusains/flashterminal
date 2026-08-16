# Security Model

Phase 4 (phases/4.md) turns FlashTerminal from "agents can run" into
"agents can run **while the user remains in control**". This document is
the security model: what agents can do, what they cannot do, and which
defense is layered where.

## Principles

1. **Deterministic rules come first.** Dangerous-command protection and
   path validation are pure, tested rules (`policy.rs`) — never LLM
   judgment. An LLM classifier may *inform* risk, but it never *decides*
   a Deny.
2. **Conservative by default.** When uncertain, the decision is
   `RequireApproval`, never an optimistic `Allow`.
3. **Never silently downgrade.** A `Deny` from a deterministic rule is
   never turned into an `Allow`. `Deny` only ever comes from deterministic
   rules, and the decision record carries its source (`PolicySource`).
4. **The planner is never the final authority.** The policy engine sits
   above the planner; the planner cannot change filesystem scope, network
   policy, autonomy level, budgets, or agent caps.
5. **Secrets are referenced, never stored.** Credentials live in the OS
   keychain; every other layer holds a `keychain://…` reference. All
   boundaries redact.

## Layered defense

```text
User
  ↓
Policy Engine          §1–§4  (deterministic rules + risk)
  ↓
Planner               (proposes; never decides)
  ↓
Plan Validator        (deterministic structure + constraint checks)
  ↓
Approval              (§15–§16, stable IDs, expiry, action-hash binding)
  ↓
Execution             (structured argv; worktree-isolated; audited)
```

## What agents CAN do

- Run *inside* an explicit filesystem scope (§7). Default for autonomous
  coding tasks: **WorktreeOnly** — the task's isolated git worktree.
- Run commands classified **Low/Medium** automatically, at the configured
  autonomy level, *within* policy scope (§14).
- Write artifacts into their own worktree; artifacts are recorded with
  lineage and bounded payloads (`artifacts.rs`).
- Execute structured commands (`executable + arguments[]`), never shell
  interpolation of untrusted values (§5).

## What agents CANNOT do

- **Cannot escape their filesystem scope.** `../` traversal, absolute
  paths outside scope, symlink escapes and mount escapes are rejected by
  `PathValidator` (§8) — canonicalization is *not* assumed sufficient.
- **Cannot write to another agent's worktree.** Worktree isolation is
  enforced per `workflow_id / agent_id / worktree_path / allowed_paths`
  (§9).
- **Cannot access the network by default.** The default engine policy is
  `NetworkPolicy::Blocked` (§10). Allowlist/Prompt/Allowed are explicit
  user changes; the planner cannot make them.
- **Cannot touch secrets without authorization.** SSH private keys, `.env`
  secrets, credential files, keychain contents, cloud/browser credential
  stores are classified (§11) and denied unless a human-granted allowance
  exists; **Critical** secrets require explicit approval at every autonomy
  level.
- **Cannot run dangerous commands.** `rm -rf`, `mkfs`, `dd`, disk ops,
  credential access, destructive git, force-push, mass deletion and
  privilege escalation are deterministically denied or approval-gated
  (§4).
- **Cannot raise its own budget.** Budget increases require policy
  configuration *or* human approval (§13).
- **Cannot change autonomy.** Autonomy changes are human-only (§14).
- **Cannot mint approvals.** Approval claims in planner payloads are
  ignored; only the user-facing engine API grants approvals, recording the
  actor (§22).
- **Cannot silently complete.** A fast 0-exit without work is honest
  `NeedsReview`, never auto-`Completed` (§22).
- **Cannot execute a plan without human approval** when the autonomy
  policy requires it; self-approval payloads land the plan at the same
  human gate.

## Secret lifecycle

| Layer | Holds | Never |
|-------|-------|-------|
| OS keychain | the value (single durable copy) | — |
| Config / `state.json` / IPC / events / audit | `keychain://flashterminal/<provider>` refs | values |
| `AgentLaunchContext` (ephemeral) | resolved value for the child env | persisted/logged/IPC'd |
| logs, errors, diagnostics | provider ids, redacted text | values |

Redaction (`redact.rs`) masks known key shapes (`sk-ant-…`, `sk-proj-…`,
`sk-…`, `AIza…`, `xai-…`, `ghp_…`) and registered runtime secrets
longest-first, applied at agent output, permission payloads, errors,
spawn diagnostics, IPC frames and before any persistence boundary.
Sentinel-secret regression tests prove no credential reaches logs, IPC,
workflow history, plan versions, replan signals, audit trail, diagnostics,
screenshots or persisted state (§40).

## Auditability

Every policy evaluation, allowance, denial, approval and refusal is a
first-class audit event (§17) with timestamp, workflow, agent, task,
action, result and source. The user can ask **"Why did FlashTerminal do
this?"** and receive a readable explanation (§18).

## Security regression coverage

- `crates/terminal-workspace/tests/phase4/main.rs` — shell injection,
  network exfiltration, self-approval, fake completion, invalid artifact,
  replan safety.
- `crates/terminal-workspace/tests/phase3f/main.rs` — dangerous command,
  secret exfiltration, policy/budget bypass, worktree path traversal,
  command injection (literal argv), approval integrity, secret-free
  persisted state.
- `crates/terminal-workspace/tests/ipc_stream.rs`, `persistence.rs` —
  sentinel secret never reaches events/persistence.
- `crates/terminal-session/src/policy.rs` — unit tests for every policy
  domain.
