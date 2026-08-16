# ADR 0017: Policy Engine

## Problem
Before Phase 4, agents executed with per-feature guards (worktree
isolation, budgets, secret redaction) but no *central* authority over
what an agent may do. Each guard was independent, decisions were not
audited as a class, and there was no single place to enforce "an agent
may not do X". With multiple agents executing meaningful work, the system
needed one policy layer above the planner that every executable action
passes through — and that the planner can never override.

## Goal
Every executable action is evaluated against a centralized, deterministic,
conservative policy before it may run; the planner is never the final
authority; and every decision is auditable with its source.

## Options Considered
1. **Per-feature guards only** (status quo): Rejected — no single
   authority, no uniform decision model, audit impossible per action.
2. **LLM-based action classification**: Rejected — the plan explicitly
   forbids relying solely on an LLM classifier (§4); deterministic rules
   come first.
3. **Central policy engine with deterministic rules + conservative
   defaults**: Selected. One `PolicyEngine` evaluates every action across
   domains (filesystem, network, process, secrets, shell, workspace,
   agent, budget, autonomy), with `Allow | Deny | RequireApproval`
   decisions, risk classification, and source-tagged records.

## Decision
- **`PolicyDecision`** is exactly `Allow | Deny | RequireApproval`
  (§2). There is no optimistic fourth state; `Deny` is never silently
  downgraded.
- **Risk** is `Low | Medium | High | Critical` (§3), conservative and
  extensible; uncertainty → `RequireApproval`.
- **Dangerous commands** (`rm -rf`, `mkfs`, `dd`, sudo, credential/SSH
  access, destructive git, force push, mass deletion, …) are denied or
  approval-gated by deterministic `DangerousRule`s before anything else
  (§4).
- **Filesystem scope** is explicit per agent/workflow
  (`ProjectOnly | WorktreeOnly | Workspace | CustomPaths | NoFilesystem`);
  default `WorktreeOnly` (§7). `PathValidator` rejects traversal,
  absolute-out-of-scope, symlink and mount escapes (§8).
- **Network** is `Blocked | Allowed | Allowlist | Prompt`; default
  `Blocked`; `Prompt` maps to `RequireApproval` (§10).
- **Secrets** are classified `Safe | Sensitive | Critical`; Critical
  requires approval at every autonomy level (§11).
- **Budget** caps agents/tokens/cost/runtime/replans/commands/network;
  increases require policy or human approval (§13).
- **Autonomy** levels bound what may run automatically; Critical is never
  automatic (§14 — see ADR 0018).
- **Approvals** are stable IDs bound to workflow/agent/action-hash/expiry;
  replay, stale reuse, wrong-workflow and post-approval changes are
  rejected (§15–§16).
- The engine is the owner: `set_network_policy`, autonomy and budget
  increases are engine APIs, never planner-visible mutations.

## Consequences
- **Positive**: one auditable decision path for every action; conservative
  defaults; deterministic safety rules; planner can never self-authorize.
- **Negative**: a conservative policy layer may require approvals where a
  permissive system would not — that is the intended Phase 4 tradeoff.
  Perfect command classification is explicitly out of scope (§3).
