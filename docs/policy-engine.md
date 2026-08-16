# Policy Engine

`crates/terminal-session/src/policy.rs` — the centralized policy layer
introduced in Phase 4 (phases/4.md §1–§16).

## Architecture

```text
User
  ↓
Policy Engine      ← you are here
  ↓
Planner            (proposes plans; never the final authority)
  ↓
Plan Validator     (deterministic structure + constraint checks)
  ↓
Approval           (§15–§16)
  ↓
Execution          (structured argv, worktree-isolated, audited)
```

Every executable action is evaluated against policy domains:

```text
Filesystem   Network   Process   Secrets   Shell
Workspace    Agent     Budget    Autonomy
```

## Decisions (§2)

`PolicyDecision` is exactly one of:

```text
Allow
Deny
RequireApproval
```

There is **no** fourth "maybe-allow" state, and a `Deny` is never silently
downgraded to `Allow`. `Deny` only ever comes from deterministic rules,
and every evaluation records `PolicySource` (which configuration produced
the decision).

## Risk classification (§3)

`RiskLevel`: `Low < Medium < High < Critical`.

The classifier is deliberately conservative and extensible — not a perfect
command classifier:

```text
ls                          → Low
npm install                 → Medium
network request             → Medium
rm -rf project              → High
modify SSH credentials      → Critical
```

When uncertain, the engine returns `RequireApproval`. Unknown executables
require approval by default.

## Dangerous command protection (§4)

Deterministic `DangerousRule`s run before anything else. Covered:

```text
rm      rmdir     sudo     chmod    chown    mkfs
dd      disk operations      system configuration
credential access            SSH key access
destructive Git commands     force push
mass deletion
```

The verdict is `Deny` or `DenyUnlessExplicitlyAuthorized` (approval-gated
override). No LLM classifier sits on this path.

## Structured execution (§5)

Process actions carry `executable` + `arguments[]` (`CommandSpec`), never
shell strings. `ShellInterpolationGuard` gates values destined for shell
interpretation (§6): `;`, `&&`, `||`, `|`, `$`, backticks, `$(…)`, `>`,
`>>`, `<`, newlines, quotes and backslashes in agent/planner/path/env
values are either argv-safe literals or rejected — they can never become
unintended shell commands.

## Filesystem policy (§7)

`FilesystemScope`:

```text
ProjectOnly    WorktreeOnly    Workspace    CustomPaths    NoFilesystem
```

Default for autonomous coding tasks: **WorktreeOnly**.

`PathValidator` (§8) rejects `../`, `../../`, absolute paths outside scope,
symlink escapes and mount escapes — canonicalization is *not* assumed
sufficient. Tested against relative/absolute paths, symlinks, Unicode,
spaces, special characters and case differences.

## Network policy (§10)

`NetworkPolicy`:

```text
Blocked    Allowed    Allowlist(Vec<NetworkAllowance>)    Prompt
```

Default: **Blocked**. `Prompt` evaluates as *allowed pending approval* at
the raw layer and the engine maps it to `RequireApproval` — never a
silent `Allow`, never a silent `Deny`. The planner cannot change the
network policy (engine-owned; covered by regression test).

## Secret policy (§11)

`SecretCategory`: `Safe < Sensitive < Critical`. Paths to SSH private
keys, `.env`, credential files, keychain contents, cloud/browser
credential stores classify as Critical. Access requires a human-granted
`SecretAllowance`; Critical secrets require explicit approval at every
autonomy level.

## Budget policy (§13)

`BudgetPolicy` caps per dimension (`BudgetLedger`):

```text
Agent count    Token usage    Estimated cost    Runtime duration
Replan count   Command count  Network requests
```

A planner cannot increase its own budget — `authorize_increase` requires
policy configuration *or* human approval. Exceeding a cap blocks further
starts.

## Autonomy levels (§14)

`AutonomyLevel`:

```text
Manual      → every risky action requires approval
Assisted    → low-risk actions automatic
Supervised  → medium-risk actions automatic inside policy scope
Autonomous  → high automation within a strict sandbox and budget
```

`Critical` risk requires explicit user approval at **every** level.
Autonomous ≠ unrestricted. See `docs/autonomy.md`.

## Approval integrity (§15–§16)

`ApprovalStore` issues stable `ApprovalId`s bound to:

```text
workflow_id   task_id   agent_id
action        action_hash   risk   policy_reasons
created_at    expires_at
```

- stale approval reuse → rejected (expiry),
- wrong-workflow / wrong-agent approval → rejected (binding),
- approval replay → rejected (hash + one-shot grant),
- post-approval action change → old approval invalidated, new approval
  required.

## Engine surface

`Multiplexer` (terminal-workspace `engine.rs`):

```text
policy_state() / policy_state_mut()
evaluate_action(action, ctx) → PolicyEvaluation
request_policy_approval / grant_policy_approval / reject_policy_approval
honor_policy_approval / pending_approvals
set_network_policy / network_policy
record_budget / budget_exceeded
```

## Test coverage

- Unit: every domain in `policy.rs` (`#[cfg(test)]`).
- Adversarial: `crates/terminal-workspace/tests/phase4/main.rs` (§6, §10,
  §20, §22).
- Engine-level: `crates/terminal-workspace/tests/phase3f/main.rs`
  (dangerous command, secret exfiltration, policy/budget bypass, replan
  safety, approval integrity).
