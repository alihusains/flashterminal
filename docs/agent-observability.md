# Agent Observability (Phase 2C)

Phase 2C ("agent observability") builds on the Phase 2 / 2B.1 agent runtime.
This document describes the implementation that actually exists — nothing
here is a placeholder. Surfaces live in `terminal-session` (models), the
`terminal-workspace` engine (aggregation + IPC), the desktop app (chrome),
and the `terminal` CLI.

## AgentWork

One `AgentWork` record exists per `ExecutionId`, created at spawn and
replaced on restart (restart reuses the same execution id). The runtime does
not support multiple concurrent works per session — that is a documented
limitation, not a hidden capability.

- `status`: `Running | Completed | Failed | Attention`; `finish()` is
  idempotent — the first terminal status wins.
- `commands`: bounded command history (≤ 512, deduped against the previous
  entry).
- `files_changed`: deduplicated set observed from agent output (heuristic).
- `activity`: bounded activity history (capacity 32) with coalescing — two
  entries of the same kind within the 400 ms window fold into a count.
- `timeline`: bounded, ordered, deterministic event ring (default capacity
  512; oldest entries drop).
- `errors`, `usage`: see below.
- Serde round-trip stable (work state survives persistence).

## Activity

`ActivityKind`: `Starting`, `Reading`, `Thinking`, `Planning`, `Editing`,
`RunningCommand`, `RunningTests`, `WaitingForInput`, `WaitingForPermission`,
`Reviewing`, `Finishing`, `Idle`, `Unknown`. Sources distinguish heuristic
detection from authoritative output events. The heuristic layer (`detect_activity`
on adapters + `detect_activity_kind` on output lines) refines state and
activity; agent output remains authoritative.

Emission is throttled: at most one activity event per coalescing window per
agent, so a chatty agent cannot flood the event bus.

## Timeline

`TimelineKind`: `Started`, `Activity`, `State`, `File`, `Command`,
`Approval`, `Error`, `Completed`. Entries carry a UTC timestamp; the ring is
capacity-bounded (default 512) and `recent(n)` returns newest-first.

## Summary, Attention, Dashboard

- `attention_for(state)` is the single source of truth (§12): `NeedsApproval`
  → PermissionRequested, `Waiting` → NeedsInput, `Blocked` → Ambiguous,
  `Failed`/`Crashed` → ErrorIntervention.
- `agent_dashboard(filter)` (engine) counts `total / running / needs_you /
  failed / completed`. Counting is explicit: `needs_you` overlaps with
  `failed` (a failed agent still needs attention). Rows are sorted
  deterministically: needs-you first, then running, failed, completed.
- Filters: `All`, `NeedsAttention`, `Failed`, `Completed`, `Running`,
  `NeedingInput`, `NeedingApproval`.
- `workspace_agent_summary()` (engine) counts per active workspace from the
  pane tree — only agents attached to a pane appear (same counting rules as
  the dashboard).

## Review / diffs

`agent_review(eid)` (engine) returns the bounded command history plus up to
64 changed files, each with a best-effort `git diff` capped at 200 lines.
No diff is fabricated — files without a readable diff carry none.

## Usage & cost

`AgentUsage`: input/output/cached token counts (`None` when the agent does
not report them — nothing is fabricated). `PricingRegistry` is a table
(provider/model → price per million tokens, USD cents, dated) seeded from
`PricingDefinition::defaults()` (Claude Sonnet 4.5 / Opus 4 / Haiku 4.5).
`estimate_cents` returns `None` for unknown provider/model or when usage is
incomplete; it never guesses a price. A minimum of 1 cent applies when
pricing is known.

## Health

`health()` (engine/runtime) returns per-definition rows (`definition_id`,
`display_name`, `detail`) for the built-in agent definitions. Details are
redacted: no API keys, `sk-` prefixes, or `key=`/`Bearer ` material ever
reaches health output.

## Replay

Deterministic event fixtures (`all_fixtures()`) drive `replay_into`:
replaying the same fixture twice yields identical events, timeline, and
summary. Used by tests and available to debugging tooling.

## Intent

`IntentResolver` maps natural-language phrases to intents
(`ShowAgents { filter }`, focus, etc.) deterministically, including
"agents needing approval / input" aliases. Resolution is a pure function.

## Notifications & quiet mode

`NotificationCenter` + `NotificationPrefs` (workspace-level, persisted in
snapshots):

- defaults: `on_needs_me: true`, `on_failure: true`, `on_completion: false`,
  `on_start: false`;
- kinds: `AgentCompleted`, `AgentFailed`, `AgentNeedsApproval`,
  `AgentNeedsInput`, `AgentProviderFailure`, `AttentionSummary`;
- notifications are meaningful transitions only (never per-output noise),
  emit once per state, and apply only to agents attached to a pane;
- quiet mode = needs-me/failure/completion/start toggles off.

## Persistence

`snapshot_state()/restore()` persist notification prefs, workspace layout
and agent panes with **redacted** launch records (credentials and registered
secrets never reach `state.json`). Restored agent panes respawn live
sessions under the same pane.

## Command palette

`CommandRegistry::palette()` returns every offerable command: bound commands
plus the remaining parameterless ones (`Show Agents`, `Show Agents Needing
Attention`, `Show Failed Agents`, `Show Completed Agents`, `Focus Agent`,
`Review Agent Changes`, `Open Agent Logs`, `Resume Agent`, `Approve`,
`Deny`, `Toggle Quiet Mode`, `Command Palette`). Parameterized commands run
against the focused target — the empty-pid `FocusAgent` focuses the first
agent pane.

## UI surface (desktop)

- Sidebar agent list filtered by the current dashboard filter (All /
  Needs Attention / Failed / Completed).
- Agent pane chrome: state badge, Stop/Restart/Resume (capability-gated),
  Allow/Deny permission bar, completion/failure indicators.
- Overlays: empty state (no agent sessions → Enter opens provider setup),
  provider setup, agent work/review view (changed files + bounded diffs),
  diagnostics (dashboard counts, event subscriber count).
- Key bindings execute through the same `run_command` dispatch as the
  palette. `Toggle Agent Work View` / `Review Agent Changes` show the review
  overlay for the focused agent; `Open Agent Logs` opens the session cwd;
  `Toggle Quiet Mode` flips needs-me/failure notification policy.

## CLI surface

`terminal agents [filter]` (dashboard), `terminal agent work|timeline|
review|health <id>`, plus the existing Phase 2 agent lifecycle commands.
All run against the IPC server of a running app instance.

## Verification

Permanent regression coverage: `crates/terminal-workspace/tests/phase2c/`
(AgentWork lifecycle + idempotent finish, session/work separation, activity
coalescing + bounds, timeline bounds + determinism, attention map, dashboard
+ workspace summary with live fake agents, pricing honesty, health
secret-free, replay determinism, intent resolution, palette coverage,
notification dedup + quiet mode, prefs persistence, work serde round-trip).
See `docs/phase2c-verification.md` for the full truth table.
