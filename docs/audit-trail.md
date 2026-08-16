# Audit Trail

`crates/terminal-session/src/audit.rs` — the first-class audit system from
Phase 4 (phases/4.md §17–§19).

## What is recorded

Every meaningful event carries:

```text
timestamp   workflow   agent   task   action   result   source
```

Event kinds (`AuditEventKind`):

```text
plan created / validated / approved / rejected / executed / superseded
policy evaluated
action allowed / denied / required approval
approval requested / granted / rejected / expired / invalidated / replay blocked
agent started / stopped / crashed / resumed
artifact created / modified / invalidated
replan created / approved / rejected / invalidated / limit reached
workflow started / paused / resumed / stopped / completed / failed
human escalation raised
budget exceeded / increased
network denied / secret denied / filesystem denied
pause all / stop all / workflow reverted
```

Each event records its source (user / agent / engine / policy) and whether
it was human-initiated (`AuditEventKind::is_human_initiated`).

## Boundaries

- **Bounded in RAM** (§39): the newest records are kept; older history is
  disk-backed with the workspace state — the trail never grows unbounded
  in memory.
- **Persisted** with workspace state, so the trail survives restart.
- **Never contains credentials** (§40): the writer re-runs the redactor
  defensively on every record before it is stored.
- **Versioned** with the persisted schema (§29) — old state migrates,
  future versions are refused.

## Why-did-this-happen UX (§18)

`AuditTrail::explain(id)` renders a readable explanation:

```text
Claude modified auth.ts.

Reason:   Task "Implement OAuth"
Policy:   WorktreeOnly
Approval: Not required
Result:   Success
```

For dangerous actions:

```text
Claude requested:

npm install

Risk:     Medium
Policy:   Network requires approval
Decision: Approved by Ali at 14:31
```

Internal implementation details are withheld unless the user expands the
record.

## Engine surface

```text
audit_trail() → &AuditTrail
audit_records() → &[AuditEvent]
audit_kind(kind, workflow, action, source)     (record helper)
audit_explain(id) → Option<String>
audit_latest(kind) → Option<String>
```

## Replan auditability (§19)

Every replan records: trigger, evidence, severity, old plan, new plan,
changed tasks, changed agents, changed dependencies, budget change and
approval decision — so the user can always see *why the workflow changed*.
See `docs/replanning.md` and ADR 0016.

## Coverage

- Unit: `crates/terminal-session/src/audit.rs` (`#[cfg(test)]`).
- Engine-level: `crates/terminal-workspace/tests/phase3f/main.rs`
  (`audit_trail_versions_diffs_interventions`,
  `secret_audit_persisted_state_clean`).
- Adversarial: `crates/terminal-workspace/tests/phase4/main.rs`
  (network denial lands in the trail).
