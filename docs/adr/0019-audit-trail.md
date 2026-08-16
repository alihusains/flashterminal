# ADR 0019: Audit Trail

## Problem
Before Phase 4, the system had no unified answer to "why did FlashTerminal
do this?" Events were recorded per feature (scheduler events, agent
events, replan signals) with no shared model, no uniform "who/what/when/
why", and no user-facing explanation surface.

## Goal
A first-class, bounded, persisted, secret-free audit system that records
every meaningful event (§17) and renders a readable explanation (§18), so
users can always determine exactly why an action happened.

## Options Considered
1. **Structured logging only**: Rejected — logs are ephemeral, not queryable
   as a trail, and not a UX surface.
2. **Per-feature event streams only** (status quo): Rejected — no unified
   "why", no consistent fields.
3. **First-class audit trail**: Selected. A dedicated `AuditTrail`
   (`audit.rs`) with a stable event-kind taxonomy, uniform fields
   (timestamp, workflow, agent, task, action, result, source), bounded
   in-RAM retention with disk-backed persistence, defensive redaction at
   write time, and an `explain()` renderer for the UX.

## Decision
- **Events** cover plans (created/validated/approved/rejected/executed/
  superseded), policy (evaluated, action allowed/denied/required-approval),
  approvals (requested/granted/rejected/expired/invalidated/replay-blocked),
  agents (started/stopped/crashed/resumed), artifacts (created/modified/
  invalidated), replans (created/approved/rejected/invalidated/limit-reached),
  workflow lifecycle, safety (escalation, budget, network/secret/
  filesystem denials), and global controls (pause/stop/revert).
- **Fields**: timestamp, workflow, agent, task, action, result, source;
  human-initiated events are marked (`is_human_initiated`).
- **Bounded**: newest records kept in RAM; older history disk-backed with
  workspace state (§39). The trail never grows unbounded in memory.
- **Secret-free**: the writer re-runs the redactor defensively before
  storing (§40); sentinel regression tests prove it.
- **Explain (§18)**: `explain(id)` renders "Reason / Policy / Approval /
  Result" or "Risk / Policy / Decision (approved by X at T)" — readable
  without leaking internal details unless expanded.
- **Replan auditability (§19)**: every replan records trigger, evidence,
  severity, old/new plan, changed tasks/agents/dependencies, budget change
  and approval.

## Consequences
- **Positive**: auditable trust; a stable answer to "why did this happen";
  replayable history; secret-safe by construction.
- **Negative**: an extra write per meaningful event (cheap, bounded, and
  verified not to affect input-path latency); older history is disk-backed
  rather than instantly in RAM.
