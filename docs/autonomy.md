# Autonomy Model

Phase 4 (phases/4.md §14) introduces explicit autonomy modes. The
governing rule: **Autonomous is not unrestricted.**

## Levels

| Level | Max automatic risk | What runs automatically |
|-------|-------------------|-------------------------|
| `Manual` | Low | Nearly nothing; every risky action requires approval. |
| `Assisted` | Low | Low-risk actions run automatically. |
| `Supervised` | Medium | Medium-risk actions run automatically inside policy scope. |
| `Autonomous` | High | High automation within a strict sandbox and budget. |

**Critical** risk requires explicit user approval at *every* level —
including `Autonomous`.

## What bounds autonomy

Autonomy never exists in a vacuum. At every level the following remain
absolute:

1. **Filesystem scope** (§7): the agent's scope is fixed by policy
   (default `WorktreeOnly`). No level expands it.
2. **Network policy** (§10): `Blocked` stays blocked; the planner cannot
   flip it. `Prompt` always asks.
3. **Secret policy** (§11): Critical secrets require approval; no level
   auto-accesses SSH keys, `.env`, keychain contents, etc.
4. **Budget** (§13): caps on agents, tokens, cost, runtime, replans,
   commands, network requests. The planner cannot raise its own budget.
5. **Dangerous commands** (§4): deterministic Deny / approval-gate, never
   bypassed by a higher level.
6. **Approval integrity** (§15–§16): approval IDs, expiry, action-hash
   binding, replay protection — level-independent.
7. **Human escalation** (§33): reaching replan limits or budget exhaustion
   escalates to the user, never "solves itself" by lowering constraints.

## Decision flow

```text
action
  → deterministic dangerous-command rules      (Deny / gate)
  → category gates: scope, network, secrets    (Deny / waiver)
  → risk classification                        (Low…Critical)
  → autonomy matrix at current level           (auto ≤ threshold?)
  → unknown executable?                        (RequireApproval)
  → decision: Allow | Deny | RequireApproval
```

`AutonomyLevel::auto_threshold()` is the only place a level changes how
much may proceed automatically — it can only move the *automatic* boundary
between Low/Medium/High risk, and Critical is never automatic.

## Changes

Autonomy is engine-owned. `Action::AutonomyChange` always evaluates to
`RequireApproval` with the reason "autonomy changes are human-only
decisions". The planner cannot change the autonomy level.

## Defaults

- `AutonomyLevel::default()` = `Manual`.
- `FilesystemScope::default()` = `WorktreeOnly`.
- `NetworkPolicy::default()` = `Blocked`.

The defaults compose to: nothing risky happens without a human. Raising
autonomy is an explicit, audited, human decision.
