# Orchestration — operator guide

How to create, run, and manage deterministic multi-agent task workflows in
FlashTerminal. Architecture: `docs/phase3a.md`.

## Task lifecycle

```text
Pending ──► Ready ──► Running ──► Completed
             │                      └─► NeedsReview ──► approve ──► Completed
             │                                       └─► reject ──► Failed
             ▼
          Blocked (dependency failed, policy=block)
          Skipped (dependency failed, policy=skip)
Running ──► Failed ──► Ready (retry, if policy allows)
         └─► Cancelled
Restore: Running/Waiting ──► Interrupted (never resumes silently)
```

Every transition is validated; invalid moves return a typed error, never a
silent no-op.

## CLI

Start a headless server first:

```text
terminal serve
```

Then (each over the socket):

```text
terminal task create <title> [--agent fake-agent] [--depends-on <id>…] [--review]
terminal task list
terminal task show <id>            # full record: deps, attempts, duration, result, artifacts
terminal task run [<id>]           # run the whole graph or one task
terminal task cancel <id>
terminal task retry <id>
terminal task review approve|reject <id>
terminal task attach <id>          # attach a live agent pane to the session
terminal task validate [<id>]      # workflow validation report
terminal task set-policy <key> <value>
terminal task policy
terminal task scheduler            # scheduler status snapshot
terminal workflow list
terminal tasks                     # short status list
```

Policies: `max_agents`, `max_parallel_tasks`, `review_required`,
`max_cost_cents`. Setting is read-modify-write, so unknown keys are
rejected and other settings are preserved.

## Desktop

`Cmd+Option+T` (or palette) opens the task dashboard.

| Key | Action |
|-----|--------|
| ↑/↓ | select (respects the active filter) |
| Enter | open the task detail panel |
| u | run all tasks |
| c / r | cancel / retry the selected task |
| a / d | approve / reject (review boundary) |
| p | attach the task's agent pane |
| v | open the attached agent's work view (detail panel) |
| Esc | unwind: form → detail → dashboard → close |

Palette: **Create Task** (title + agent form), **Show Blocked Tasks**,
**Show Tasks Needing Review**, **Open Task**, plus the full Phase 3A
action set. Ctrl-Alt bindings: t=Tasks, Enter=Run All, c=Cancel,
x=Retry, p=Open Agent.

## Deterministic fixtures (fake-agent)

The `fake-agent` binary provides deterministic scenarios for testing and
benchmarking — never escapes the workspace:

```text
FAKE_AGENT_SCENARIO=completion|failure|flaky|auth-failure|long-running|streaming|waiting|approval|large-output
FAKE_AGENT_ATTEMPT=n   # with flaky: fail the first n attempts
```

Tasks carry their scenario via launch environment (task `environment`
has priority over adapter-built env).

## Failure classes and retry

Exit codes classify into `FailureClass` (authentication, network,
provider, crash, generic). Policy default: auth failures are **never**
retried; flaky failures retried once; transient provider/network/crash
failures retried once (`max_retries` in `RetryPolicy`).

## Budgets

`max_cost_cents` on the task policy is a hard ceiling: once the remaining
budget is exhausted the scheduler blocks further starts (typed
`BudgetExceeded`), never overspending automatically.

## Safety

- The scheduler emits typed commands only; the agent runtime owns all
  process spawning (no raw subprocess creation from orchestration code).
- Persisted scheduler state stores configuration and references — never
  launch secrets (verified by sentinel tests).
- Cost estimates are recorded from the pricing table with known data;
  unknown/partial data yields `None`, minimum 1¢ when known.