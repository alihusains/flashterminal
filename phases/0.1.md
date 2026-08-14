# Build Phase 0: Terminal Foundation

You are starting implementation of the high-performance AI-native terminal described in the master product specification.

Your job in this phase is **NOT to build AI agents, orchestration, BYOK, browser functionality, plugins, or a complete terminal product**.

Your job is to establish the engineering foundation on which everything else will be built.

The two goals of this phase are:

1. Build a technically sound native terminal foundation.
2. Build objective performance benchmarks that prevent the product from becoming slow or memory-heavy as features are added.

---

# 1. Start With Repository Inspection

Before writing implementation code:

- inspect the repository
- inspect the current git status
- identify existing files
- identify existing language/toolchain configuration
- identify installed dependencies
- identify the OS and architecture
- determine whether the repository is empty or partially implemented
- do not delete existing work without understanding it

Create:

```text
/docs
/docs/adr
/benchmarks
/tests
/scripts
```

Create the initial documentation:

```text
/docs/architecture.md
/docs/performance.md
/docs/development.md
/docs/adr/0001-initial-architecture.md
```

---

# 2. Make the First Architecture Decision

The initial target platform is:

> macOS on Apple Silicon

The initial implementation language for the core should be:

> Rust

The architecture should separate:

```text
UI
│
Rust Application Core
│
├── Terminal Core
├── PTY / Process Manager
├── Renderer
├── Workspace
├── Event Bus
└── Persistence
```

AI functionality must not be introduced into the render path.

Do not build a cloud backend.

Do not create microservices.

The initial application must work entirely locally.

---

# 3. Establish the Performance Philosophy

Create explicit performance budgets.

Initial targets:

```yaml
cold_start_ms: 250
warm_start_ms: 100
idle_ram_mb: 40
ten_panes_ram_mb: 80
twenty_panes_ram_mb: 120
idle_cpu_percent: 1
input_latency_p95_ms: 8
render_latency_p95_ms: 16
binary_size_mb: 15
```

These are engineering budgets.

They must be measurable.

Do not simply document them.

Build automated benchmarks for them.

---

# 4. Build the Benchmark Harness FIRST

Before implementing complex terminal functionality, create a benchmark system capable of measuring:

## Startup

Measure:

```text
process launch
window creation
terminal initialization
first usable frame
```

Record:

```text
min
median
p95
p99
```

---

## Memory

Measure:

```text
startup
idle
one pane
five panes
ten panes
twenty panes
fifty panes
```

Record:

```text
RSS
heap
resident GPU memory where measurable
```

---

## CPU

Measure:

```text
startup
idle
typing
scrolling
heavy output
multiple panes
```

---

## Rendering

Create tests for:

```text
10,000 lines
100,000 lines
1,000,000 lines
10,000,000 lines
```

Measure:

```text
frames per second
render latency
input latency
CPU
memory
```

---

# 5. Create a Baseline

The application does not need to beat competitors immediately.

First establish:

```text
baseline-v0
```

Every future version will be compared against this.

Create machine-readable benchmark output such as:

```json
{
  "commit": "abc123",
  "startup_ms": 82,
  "idle_ram_mb": 31,
  "ten_panes_ram_mb": 67,
  "input_latency_p95_ms": 4.7
}
```

Do not rely on screenshots or subjective judgments.

---

# 6. Build the Smallest Possible Terminal

Implement only:

```text
window
+
PTY
+
shell
+
terminal state
+
renderer
```

The first successful milestone should simply be:

```text
Open application
    ↓
Terminal appears
    ↓
User gets shell
    ↓
User can type
    ↓
Commands execute
    ↓
Output renders
```

For example:

```bash
echo "hello"
pwd
ls
git status
```

must work.

---

# 7. PTY Layer

Create a dedicated abstraction.

Conceptually:

```text
TerminalSession
 ├── session_id
 ├── process_id
 ├── shell
 ├── cwd
 ├── stdin
 ├── stdout
 ├── stderr
 └── dimensions
```

It must support:

- spawn
- resize
- input
- output
- terminate
- restart
- cleanup

Do not tightly couple PTY management to UI code.

---

# 8. Terminal Parser

Create a separate terminal state engine.

Conceptually:

```text
PTY bytes
    ↓
VT parser
    ↓
Terminal state
```

The renderer must consume terminal state.

The renderer must NOT parse terminal protocols itself.

---

# 9. Terminal State Model

Create a structure representing:

```text
cells
cursor
selection
scroll position
alternate screen
colors
attributes
hyperlinks
```

Keep the model optimized for:

- incremental updates
- low allocation
- fast scrolling
- large output

Do not use expensive UI component objects for individual terminal cells.

---

# 10. Renderer

Implement GPU-accelerated rendering.

The renderer should support:

- text
- colors
- cursor
- selection
- scrolling
- resizing
- basic Unicode

Implement dirty-region rendering.

Example:

```text
PTY output
     ↓
VT parser
     ↓
changed rows
     ↓
render scheduler
     ↓
GPU
```

Do not redraw the entire terminal every time a single character changes.

---

# 11. First Benchmark Gate

At this point stop feature development.

Run:

```text
startup benchmark
idle benchmark
typing benchmark
10k output benchmark
100k output benchmark
1m output benchmark
```

Generate a report.

The AI agent must explicitly compare:

```text
actual
vs
budget
```

Example:

```text
Startup: 91 ms / 250 ms budget       PASS
Idle RAM: 34 MB / 40 MB              PASS
Input p95: 4.2 ms / 8 ms             PASS
1M output: 5.1 sec                   PASS
```

If any critical metric fails badly, fix the architecture before proceeding.

---

# 12. Shell Integration

Once the core renderer is stable, add:

- shell detection
- current working directory
- command boundaries
- process boundaries
- exit status
- shell integration

Support initially:

```text
zsh
bash
fish
```

Do not add every shell yet.

---

# 13. Clipboard

Implement:

- copy
- paste
- selection
- bracketed paste
- large paste handling

Test:

```text
1 line
100 lines
10,000 lines
1 MB
10 MB
```

The UI must not freeze during large paste operations.

---

# 14. Resize Testing

Test:

```text
tiny terminal
normal terminal
very wide terminal
very tall terminal
rapid resize
window maximize
window restore
```

Ensure no:

- crash
- rendering corruption
- deadlock
- excessive CPU
- memory spike

---

# 15. First Real Milestone

The first milestone is complete when the application satisfies:

```text
✓ Native macOS application
✓ Rust core
✓ PTY
✓ shell
✓ VT parsing
✓ GPU rendering
✓ keyboard input
✓ clipboard
✓ resize
✓ Unicode
✓ scrolling
✓ ANSI colors
✓ massive output
✓ benchmark harness
✓ automated performance report
```

And:

```text
✓ No AI
✓ No agent orchestration
✓ No browser
✓ No editor
✓ No cloud backend
```

That is intentional.

---

# 16. Only Then Build Multiplexing

Once the single-terminal implementation is stable, create:

```text
TerminalSession
      ↓
Pane
      ↓
Tab
      ↓
Workspace
```

Start with:

```text
2 panes
```

Then:

```text
5
10
20
50
```

Run the memory benchmark at every stage.

---

# 17. Pane Architecture

Do not make a pane a UI-only construct.

Create a real model:

```text
Pane {
    id
    session_id
    type
    cwd
    title
    state
}
```

The UI renders the model.

The core manages the lifecycle.

---

# 18. Workspace Architecture

Create:

```text
Workspace {
    id
    name
    project_root
    panes
    layout
    active_pane
}
```

The first workspace version should only support:

- create
- rename
- switch
- close
- persist
- restore

---

# 19. Persistence

Use a local persistence layer.

Prefer SQLite or another lightweight embedded store.

Do not create a remote database.

Persist:

```text
workspace
tabs
panes
layout
project path
recent state
```

Do NOT attempt arbitrary process checkpointing.

---

# 20. Then Build the Agent Abstraction

Only after the terminal and workspace engine are stable should you create:

```text
Agent
AgentAdapter
AgentSession
AgentState
AgentCapabilities
AgentMetrics
```

Start with **generic CLI agents**.

The first test agent should actually be a fake deterministic agent.

Create:

```text
fake-agent
```

It should simulate:

```text
starting
working
thinking
tool call
waiting
approval
success
failure
long-running task
large output
crash
```

This lets you test the agent platform without coupling development to Claude or OpenAI.

---

# 21. Then Integrate Real Agents

After `fake-agent` is stable:

Implement:

```text
Claude Code
Codex
OpenCode
Pi
```

One at a time.

For each agent validate:

```text
launch
output
status
stop
restart
approval
completion
error
resume where supported
```

Do not build orchestration yet.

---

# 22. Then Build Agent UX

Add:

```text
agent sidebar
agent status
notifications
activity
summary
logs
diff
cost
token usage
```

The user should be able to run:

```text
Claude
Codex
OpenCode
Pi
```

simultaneously without confusion.

---

# 23. Then Build BYOK

Only after agent abstraction works.

Implement:

```text
Provider
Credential
Endpoint
Model
```

with secure OS credential storage.

---

# 24. Then Build Orchestration

Only after individual agents are reliable.

Build:

```text
Task
TaskGraph
Scheduler
Dependency
AgentAssignment
Artifact
Handoff
```

Then:

```text
parallel
sequential
hierarchical
review
race
```

Then worktrees.

---

# 25. Critical Rule for Every Future Feature

Before implementing any major feature, answer:

```text
What user problem does this solve?

How does it affect:
- RAM?
- CPU?
- startup?
- rendering?
- latency?
- security?
- complexity?
```

If a feature significantly increases complexity or resource usage, redesign it.

---

# 26. Your Immediate Objective

Do not attempt to build the whole product from the master specification.

Your immediate goal is:

> **Build the smallest incredibly fast native terminal that can later become the execution engine for everything else.**

The architecture should make this possible:

```text
             Current
                │
                ▼
          Fast Terminal
                │
          ┌─────┴─────┐
          ▼           ▼
      Workspace     Agent Runtime
                        │
                 ┌──────┼──────┐
                 ▼      ▼      ▼
              Claude  Codex   Pi
                        │
                        ▼
                  Orchestrator
```

Do not skip the terminal foundation.

Do not start with the UI polish.

Do not start with AI.

Do not start with multi-agent orchestration.

**Start with:**

> **Rust + PTY + VT parser + GPU renderer + benchmark harness.**

That foundation determines whether the final product actually deserves to be called the world's fastest terminal.