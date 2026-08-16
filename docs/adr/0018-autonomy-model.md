# ADR 0018: Autonomy Model

## Problem
Phase 4 §14 requires explicit autonomy modes. Before this, agent
execution was effectively all-or-nothing with respect to human
supervision: either everything needed approval or (in the future) nothing
would. Users need a graduated, explicit contract for how much may happen
automatically — while the system must never let "autonomous" be read as
"unrestricted".

## Goal
Define exactly what each autonomy level allows, bound automation by risk,
and keep Critical actions and safety constraints human-only at every level.

## Options Considered
1. **Two modes (manual / autonomous)**: Rejected — too coarse; no room for
   low-risk convenience vs supervised medium-risk work.
2. **Levels defined by permission lists per command**: Rejected — brittle,
   not risk-based, hard to audit.
3. **Four risk-thresholded levels**: Selected. `Manual | Assisted |
   Supervised | Autonomous`, where the level selects the maximum *risk*
   that may execute automatically, and Critical is never automatic.

## Decision
- **`AutonomyLevel`**: `Manual` (every risky action requires approval),
  `Assisted` (low-risk automatic), `Supervised` (medium-risk automatic
  inside policy scope), `Autonomous` (high automation within a strict
  sandbox and budget).
- **`auto_threshold()`** maps level → max automatic risk: Manual/Assisted
  → Low, Supervised → Medium, Autonomous → High. Critical is never
  automatic at any level.
- **Invariants across all levels**: filesystem scope, network policy,
  secret policy, budget caps, dangerous-command rules, approval integrity
  and human escalation all remain absolute. A higher level can only move
  the *automatic* boundary, never weaken the safety constraints.
- **Autonomy is engine-owned**: `Action::AutonomyChange` always evaluates
  to `RequireApproval` ("autonomy changes are human-only decisions"). The
  planner cannot change its own level.
- **Default** is `Manual` — nothing risky happens without a human until a
  human explicitly raises the level.

## Consequences
- **Positive**: a clear, auditable contract for automation; safety
  constraints are level-independent; users can trust a supervised or
  autonomous agent without babysitting every Low/Medium action.
- **Negative**: autonomy cannot exceed the configured level; some flows
  will pause for approval even at Autonomous when risk is Critical or the
  action is unknown — by design.
