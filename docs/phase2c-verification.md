# Phase 2C Verification

Verification run against commit under test: `2c1.md` Phase 2C gates.

Legend: ✅ implemented and verified · 🟡 partial (backend/IPC, no desktop
surface) · ❌ absent. "Manual" = exercised by hand against the running
desktop app; no desktop manual pass was performed in this phase, so every
Manual cell is honest: not performed.

## Truth table (§36)

| Feature        | Backend | UI | Automated Test | Manual Test | Performance | Final |
| -------------- | ------- | -- | -------------- | ----------- | ----------- | ----- |
| Agent Work     | ✅ `AgentWork` (one per execution id, idempotent finish, commands ≤512, files, errors, usage) | ✅ review/work overlay (`draw_work_view`, bounded diffs ≤64 files/200 lines) | ✅ `work_lifecycle_and_idempotent_finish`, `work_serializes_roundtrip`, `work_items_share_a_session`, `session_survives_work_completion_and_restart` | ❌ not performed | ✅ bounded structures (activity ≤32, timeline ≤512, commands ≤512) asserted by tests | ✅ backend+UI+tests |
| Activity       | ✅ heuristic `ActivityKind`, coalescing (400 ms window, folded counts) | ✅ pane chrome state/activity badge + diagnostics rows show `activity_kind`/confidence | ✅ `activity_kinds_are_deterministic`, `activity_coalescing_folds_rapid_events`, `activity_histories_are_bounded` | ❌ not performed | ✅ throttle: ≤1 activity event per 400 ms window, history bounded at 32 (flood test) | ✅ backend+UI+tests |
| Timeline       | ✅ bounded ring (`AgentTimeline`, default 512, deterministic, newest-first) | 🟡 no dedicated desktop view; surfaced via `terminal agent timeline <id>` | ✅ `timeline_is_bounded_ordered_deterministic`, `timeline_growth_is_bounded_via_work` (5 k event flood) | ❌ not performed | ✅ ring never exceeds capacity; flood-tested | ✅ backend+CLI+tests; UI pending |
| Dashboard      | ✅ `agent_dashboard(filter)` — explicit overlapping counts (`needs_you` may include `failed`), deterministic sort (needs-you → running → failed → completed) | ✅ filtered sidebar agent list + diagnostics overlay counts/rows | ✅ `dashboard_and_summary_counts_with_live_agents` (4 live fake agents: completed/failed/needs-you/running all counted; filter rows exact) | ❌ not performed | ✅ sorting/counts deterministic; bounded render (top 9 rows in diagnostics) | ✅ backend+UI+tests |
| Attention      | ✅ single source `attention_for(state)` (approval → permission, waiting → input, blocked → ambiguous, failed/crashed → error) | ✅ pane chrome permission Allow/Deny bar + attention filters in sidebar | ✅ `attention_map_is_exact` (full state→reason map), dashboard overlap assertions | ❌ not performed | — (pure map; constant time) | ✅ backend+UI+tests |
| Notifications  | ✅ `NotificationCenter` — meaningful transitions only, once per state, pane-attached agents only; deduped | 🟡 engine/IPC-level only — desktop does not subscribe yet | ✅ `approval_notifications_fire_once_and_respect_quiet_mode` (fire-once + dedup + quiet suppression) | ❌ not performed | ✅ no per-output noise (transition-gated) | ✅ backend+tests; desktop UI pending |
| Quiet Mode     | ✅ `NotificationPrefs` (defaults: needs-me + failure on) persisted in snapshots | ✅ `Toggle Quiet Mode` command — key binding + palette via `run_command` | ✅ `quiet_mode_prefs_persist_across_restore`, quiet suppression in notification test | ❌ not performed | — | ✅ backend+UI+tests |
| Provider Setup | ✅ provider registry/model catalog, `provider_status()`, health rows secret-free | ✅ provider setup overlay (`draw_provider_setup`); empty state Enter opens it | ✅ `health_rows_are_present_and_secret_free` (no `sk-`/`key=`/`Bearer` leaks), existing provider registry tests | ❌ not performed | — | ✅ backend+UI+tests |
| Cost           | ✅ `AgentUsage` + `PricingRegistry` — table-backed, `None` when unknown/incomplete, min 1¢ when known | 🟡 engine record carries `estimated_cost_cents`; no desktop/CLI display surface yet | ✅ `pricing_estimates_only_with_known_data` (known model exact math; unknown provider/model → `None`; zero/partial usage → `None`) | ❌ not performed | — | ✅ backend+tests; UI pending |
| Intent         | ✅ `IntentResolver` — deterministic pure phrase→intent map (filters incl. needing-input/approval) | 🟡 no desktop surface; IPC/model-level | ✅ `intent_resolution_is_deterministic` + filter alias coverage | ❌ not performed | — | ✅ backend+tests; UI pending |
| Replay         | ✅ deterministic fixtures (`all_fixtures`) + `replay_into` (identical events/timeline/summary per replay) | 🟡 no desktop surface; test/debug tooling only | ✅ `replay_fixtures_are_deterministic` (every fixture, two runs compared) | ❌ not performed | ✅ replay bounded by fixture size | ✅ backend+tests |

## Palette / dispatch (§37 "Command palette commands execute")

Every Phase 2C command is palette-reachable (`CommandRegistry::palette()`:
bound commands + parameterless extras incl. empty-pid `FocusAgent`), and
every palette selection executes through the desktop's single `run_command`
dispatch — the same path as key bindings.
Test: `palette_covers_all_phase2c_commands` (labels + ≥22 entries) ✅.

## Release gates

| Gate | Result |
| ---- | ------ |
| `cargo test --workspace` | ✅ 183 tests green across all crates (terminal-core, terminal-session incl. `phase051`, terminal-parser, terminal-renderer, terminal-text, terminal-workspace incl. 18 `phase2c` tests, persistence, ipc-stream, desktop/cli) |
| Phase 2C regression suite | ✅ `crates/terminal-workspace/tests/phase2c/` — 18/18 |
| `cargo clippy --workspace --all-targets` | ✅ 0 warnings (only third-party `block v0.1.6` future-incompat note) |
| `cargo fmt --all -- --check` | ✅ clean |
| `cargo build -p desktop` | ✅ clean |
| `cargo build --workspace --release` | ✅ clean |
| Secret safety | ✅ health/persistence secret-free tests green; launch records redacted in snapshots |

## Fixes made during this verification

1. **Engine counting bug**: `agent_dashboard`/`workspace_agent_summary`
   checked `attention_for` first, so `Failed|Crashed|Blocked` counted only as
   `needs_you` and the `failed` branch was unreachable (`d.failed` was always
   0). Fixed with explicit overlapping counting — verified live: Failed agent
   → `failed=1, needs_you=1`.
2. **Phase 2C test suite issues** (5 failures): notifications tests spawned
   agents without panes (notifications are pane-attached) → switched to
   `split_pane_agent`; `NeedsAttention` filter correctly includes Failed
   (assertion updated); activity-bounds expectation used `1000 % 3` math;
   pricing expectations were 100× off and missing required output tokens;
   palette lacked the parameterized `FocusAgent` → added empty-pid palette
   entry with "focus first agent pane" dispatch.
3. **Earlier `phase051` failures**: 11-test target passes deterministically
   (4/4 runs incl. `--test-threads=1`); no Phase 2C code touched it —
   classified prior flaky/environmental with evidence, not marked
   "pre-existing".

## Known limitations (documented, not hidden)

- One `AgentWork` per `ExecutionId`; restart replaces the work — concurrent
  works per session unsupported by design.
- Notifications, workspace summary, and diffs apply to pane-attached agents
  only.
- Desktop UI for Notifications, Timeline, Cost, Intent, Replay not yet
  surfaced (backend, IPC, CLI, and tests complete).