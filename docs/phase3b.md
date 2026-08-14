# Phase 3B — Intelligent Planning + Agent Selection + Governed Orchestration

**Spec:** `phases/3b.md` · **Status:** implemented (see §G tests, §I decision)

Phase 3B adds a **planner** to the orchestration layer. The fundamental rule
(3b.md §2): the LLM is a *planner*, never an *orchestrator*. It proposes a
structured plan; only the deterministic validator, compiler, and scheduler
turn it into executed work.

```
User Intent
   ↓
IntentDisposition (deterministic — §43/§44: simple commands bypass)
   ↓
PlannerRequest ──► PlannerProvider (LLM) ──► ProposedPlan (structured JSON)
   ↓                                                ↓
PlannerContext (bounded, allowlisted)         PlanValidator (deterministic)
                                                   ↓
                                            PlanCompiler ──► TaskGraph
                                                   ↓
                                            TaskScheduler (authoritative)
                                                   ↓
                                            AgentRuntime → PermissionPolicy → Agent
```

There is **no shortcut** from the LLM to a process (3b.md §34).

---

## A. Planner architecture

All planning-domain code lives in
`crates/terminal-session/src/planning.rs` (new module, ~2,400 lines with
tests) and is exposed through `crates/terminal-workspace/src/engine.rs`
(the engine API), `ipc.rs` (socket surface), and `apps/cli` (`terminal plan
…`).

| Concept | Type | Role |
|---|---|---|
| `PlannerRequest` | struct | intent + workspace + bounded context + constraints (§4) |
| `PlannerContext` / `PlannerContextBuilder` | struct | bounded, allowlisted context (§5) — never secrets, never a filesystem dump |
| `PlannerConfig` + `PlannerProfile` | struct/enum | provider/model/temperature/budget/approval; Fast/Balanced/Deep presets (§8, §36) |
| `PlanSchema` → `ProposedPlan` | struct | strict structured output; prose is a typed error, never string-parsed (§9–§10, §20) |
| `PlanValidator` | struct | deterministic validation: agents, deps, cycles, budget, parallelism (§12–§14) |
| `PlanCompiler` | struct | deterministic proposal → `TaskGraph` + policy (§15) |
| `PlannerState` | struct | typed state machine: Idle → Planning → NeedsApproval → Approved → Executing → Done/Failed/… (§17–§19, §23, §25–§26) |
| `PlannerProvider` | trait | the LLM boundary; tests inject mocks (§7, §47) |
| `PlannerEvent` | enum | lifecycle events published as `ApplicationEvent::PlannerEvent` (§24) |
| `PlannerMetrics` / `PlannerAuditTrail` | struct | §39/§41 feedback loop + §29 audit records |
| `plan_hash` | fn | deterministic FNV-1a over the normalized plan (§51) |

Key invariants:

- **Plan → approval → execution.** `compile_for_execution` requires the
  `Approved` phase; `Auto` approval mode is the engine calling `approve()`,
  never a bypass in the state machine (§23, §30).
- **Re-validation on execute.** Human edits (§18/§19) mark the plan
  `edited`; `plan_execute` re-runs the full validator before compiling.
- **Scheduler stays authoritative.** The compiled policy is **not** applied;
  the engine's scheduler policy (budget, concurrency) is the ceiling
  (§33). The planner cannot raise `max_parallel_tasks` — plans that exceed
  the policy cap are rejected at validation.
- **Events on the one bus.** `ApplicationEvent::PlannerEvent` joins the
  existing event bus with a dedicated `EventFilter.planner` channel
  (§24).

## B. Agent selection

Planner steps carry an `AgentRecommendation` (id + reason + confidence,
§11). A recommendation is **never** an instruction to execute — the
deterministic `PlanValidator` enforces:

1. the agent id exists in the engine's `AgentRegistry` (`UnknownAgent` if not);
2. the agent's executable resolves through the adapter boundary exactly the
   way the runtime would spawn it (`AgentUnavailable` if not — fake-agent
   has its own binary lookup, mirrored in `engine::planner_agent_availability`);
3. confidence is in `[0, 1]`.

An unavailable recommendation is reported (`PlanValidationError`), **never
silently substituted** (§12). Deterministic agent selection by task type /
cost / policy is deliberately out of scope for 3B; the recommendation +
validation gate is the governed path.

## C. Security

The planner cannot bypass deterministic controls (§34). Demonstrated by
`tests/phase3b` (§49):

- **Unavailable agent** → `UnknownAgent` / `AgentUnavailable` validation error.
- **Ignore max parallelism** → plan with more independent wave-0 steps than
  the scheduler cap → `ParallelismExceeded`.
- **Exceed budget** → `BudgetExceeded` when plan estimate > scheduler
  `max_cost_cents`.
- **Bypass approval** → `plan_execute` before approval returns
  `NotAllowed`; compilation requires `Approved`.
- **Smuggled shell fields** → serde drops unknown JSON keys; the compiled
  `TaskGraph` carries only the user-approved title/description as *text*
  input (§28) — nothing spawns a shell.
- **Context secrecy** → the planner context is built from an explicit
  allowlist (`PlannerContextBuilder`): secret-shaped files (`.env`, keys,
  `.pem`, `id_rsa`, …) and dot-directories are excluded (§27), bounded
  (≤64 entries), deterministic (sorted). Audit records and persistence
  never contain credentials or private reasoning (§25, §29).

## D. UX

- **`terminal plan create "<intent>"`** routes through the planner;
  simple intents (`run tests`, `show agents`, `split pane`) are bypassed
  deterministically with `PlannerError::Bypassed` (§43–§44).
- **`terminal plan status`** renders the §42 review screen as a task list:
  goal, per-step `[symbol] id — title (agent)` lines, dependencies,
  estimated cost/duration, phase, edited flag.
- **`terminal plan approve|reject|edit set-agent|set-deps|validate|execute|resume|cancel|metrics`**
  covers the full loop: preview → approve → execute, or edit → re-validate.
- **Interruption recovery (§26):** a restored plan lands in `Interrupted`
  with a typed "interrupted — resume explicitly" error; nothing resumes
  silently.
- The desktop planning review overlay is **not** built in 3B (see §H).

## E. Metrics

`PlannerMetrics` (monotonic counters, aggregated latency): `plans_generated`,
`plans_valid`, `plans_invalid`, `invalid_schema_count`, `unknown_agent_count`,
`cycle_count`, `budget_violation_count`, `parallelism_violation_count`,
`human_edits` (+ per-edit-kind counters), `human_rejections`,
`executions_started/succeeded/failed`, `retries_used`, `bypassed_intents`,
latency count/total/max, estimated planning + execution cost (§39, §41).
Exposed via `terminal plan metrics` and `Multiplexer::planner_metrics`.

## F. Performance

- The planner never blocks the terminal render loop: `plan_request` runs
  the provider synchronously on the caller (IPC/CLI) thread; the engine's
  `drain_frame` cost is unchanged (planning adds zero work per frame when
  idle — the state machine is only touched by explicit calls and
  `publish_task_events` when a plan is `Executing`).
- Context building is bounded and deterministic (≤64 repo entries, ≤12
  active + ≤12 recent tasks) — no unbounded reads, no hidden LLM latency
  in the hot path.
- Offline behavior (§46): no provider configured ⇒ `plan_request` returns
  `NoProvider`; the terminal, existing agents, and existing task graphs
  work unchanged.

## G. Tests

Full workspace: **284 passed / 0 failed** (1 ignored — the real-provider
test, §48). Phase 3B suite (`crates/terminal-workspace/tests/phase3b/`,
21 tests): structured parsing, prose/invalid rejection, provider failure
typing (network/auth/timeout/rate), schema retry repair, intent bypass,
unknown agent, cycles, budget, parallelism, malicious-plan rejection
(§49), shell-field smuggling, editing + re-validation, dependency edits,
execution through the scheduler, approval gate, interrupt/resume,
persistence round-trip, context secrecy, event-bus publishing, metrics,
and deterministic replay of the three `fixtures/planner_response_*.json`
fixtures (§52). Module unit tests in `planning.rs` (20 tests) cover the
state machine, hash determinism, compiler determinism, context bounds, and
profiles. IPC round-trip test `socket_plan_approval_gate` added.

Gates: `cargo clippy --workspace --all-targets -- -D warnings` → 0 warnings
· `cargo fmt --check` clean · `cargo build --workspace --release` clean.

## H. Known limitations (explicit)

- **Desktop planning UI** — the §42 review screen exists in CLI form only;
  the desktop overlay is not built.
- **Real LLM provider** — no HTTP `PlannerProvider` implementation ships;
  the boundary is the trait. CI uses mocks. Real runs are opt-in
  (`--ignored`, §48) and require BYOK credentials.
- **Auto/Strict approval** — policy infrastructure exists
  (`PlannerApprovalMode`), but no automatic safe/risky classification;
  `Confirm` is the effective mode.
- **Plan parallelism is advisory** — the compiled policy is not applied to
  the scheduler; a plan can request *lower* parallelism but never raise the
  scheduler cap (by design, §33).
- **In-process resume** — resume is exercised in-process for tests and
  across `plan_restore` for restarts; there is no on-disk store for the
  persisted plan slice (callers own serialization).
- **Agent selection** — recommendations are validated, not auto-selected by
  capability/task-type heuristics (§12 keeps this for a later phase).
- Explicitly **not built** (3b.md §54): long-term memory, autonomous
  background planning, self-replanning loops, agent debate/handoffs/messaging,
  manager agents, distributed orchestration, visual workflow editor,
  fully autonomous execution.

## I. Decision

```text
READY FOR PHASE 3C
```
