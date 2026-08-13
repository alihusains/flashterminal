# Master Build Prompt: AI-Native High-Performance Terminal

## Role

You are the **principal engineer, systems architect, product architect, UX architect, performance engineer, security engineer, and technical program manager** responsible for designing and building a new terminal application from first principles.

You are not being asked to create a simple terminal emulator.

You are building a new product category:

> **A high-performance, human-friendly AI-native terminal and agent workspace that combines the speed of a native terminal, the persistence of a multiplexer, the visibility of a multi-agent control plane, and the simplicity of a modern desktop application.**

The product must support both:

1. **Non-technical users** who should be able to accomplish useful tasks without understanding shells, PTYs, tmux, worktrees, processes, or AI infrastructure.
2. **Technical power users** who need a fast, programmable, composable terminal capable of running many concurrent shells and AI agents.

The product must eventually support:

- Claude Code
- OpenAI Codex
- OpenCode
- Pi
- Gemini CLI
- Aider
- Cline
- future agents
- arbitrary custom CLI agents
- ACP-compatible agents
- BYOK providers
- local models
- remote agents
- SSH
- containers
- worktrees
- multi-agent orchestration

Current market research indicates that products such as Ghostty, Alacritty, WezTerm, Kitty, iTerm2, Warp, tmux, cmux, herdr, Terax, Zed, Claude Desktop and Codex each solve portions of this problem, while users continue to report complexity around configuration, multi-agent visibility, pane management, persistence, remote environments, provider lock-in, and understanding what agents are doing. The product must specifically attack those gaps. 

---

# 1. Product Vision

The product vision is:

> **The fastest terminal for humans and AI agents. One workspace. Any agent. Zero unnecessary terminal complexity.**

The deeper product thesis is:

> AI agents are turning the terminal from a single-process command interface into an operating environment for multiple autonomous workers. Existing workflows force users to manually coordinate terminals, panes, sessions, worktrees, models, permissions, Git branches, SSH sessions and agent states. This product should hide that infrastructure while preserving complete power and transparency.

The product must follow this principle:

> **Complexity must be available, never mandatory.**

A beginner should be able to click:

> Run Claude

An experienced developer should be able to type:

```bash
claude
```

A power user should be able to execute:

```bash
terminal agent spawn claude --workspace payments
```

An automated system should be able to call:

```text
workspace.create()
agent.spawn()
pane.create()
agent.wait()
```

These are all different interfaces over the same underlying platform.

---

# 2. Product Principles

These principles are mandatory and must guide every technical and UX decision.

## 2.1 Performance is a product feature

The terminal itself must consume very little CPU, memory and battery.

AI workloads may consume significant resources. The terminal must not.

Never allow:

- AI processing on the renderer's hot path
- unnecessary background polling
- full-screen redraws for small changes
- memory growth proportional to infinite terminal history
- permanently embedded heavy browser runtimes
- unnecessary JavaScript processes in core execution
- blocking UI operations for Git, SSH, indexing or AI events

---

## 2.2 Human-first

The UI must be usable without terminal expertise.

Users should be able to understand:

- what is happening
- what needs their attention
- which agent is working
- what an agent changed
- what failed
- what requires approval
- what is currently running

without reading raw logs.

---

## 2.3 Terminal-native

Despite the friendly UX, this must still be a real terminal.

It must support:

- shell interaction
- PTYs
- ANSI/VT protocols
- SSH
- pipelines
- stdin/stdout/stderr
- interactive CLI programs
- ncurses/TUI applications
- shell control sequences
- Git
- standard Unix workflows

Do not create a fake terminal abstraction that breaks existing CLI applications.

---

## 2.4 Agent-neutral

Never design around one AI vendor.

Claude, Codex, OpenCode, Pi, Gemini and future agents must be peers.

Do not write architecture such as:

```text
if claude ...
else if codex ...
```

Instead create:

```text
Agent
AgentAdapter
AgentCapabilities
AgentLifecycle
AgentState
AgentProtocol
```

---

## 2.5 Provider-neutral

Support:

- Anthropic
- OpenAI
- Google
- OpenRouter
- xAI
- Mistral
- Groq
- Together
- DeepSeek
- Cerebras
- Ollama
- LM Studio
- arbitrary OpenAI-compatible endpoints
- arbitrary future providers

The user owns their credentials and chooses their provider.

---

## 2.6 Local-first privacy

The architecture should assume that user data is sensitive.

Prefer:

- local processing
- local storage
- local agent state
- local history
- OS credential stores
- BYOK
- optional local models

Do not require an account for core terminal functionality.

Do not introduce telemetry by default.

---

## 2.7 Progressive disclosure

The default UI must be simple.

Advanced controls should appear only when needed.

The same application should satisfy:

```text
Beginner
   ↓
Developer
   ↓
Power User
   ↓
AI Power User
   ↓
Agent Orchestrator
```

without forcing every user into the most complex UI.

---

# 3. Product Category

Do not position this internally as simply:

> "Terminal with AI."

The intended category is:

> **AI Development Workspace / Agent Terminal**

The terminal is the execution substrate.

The workspace is the user interface.

The agent runtime is the intelligence layer.

The orchestrator is the coordination layer.

---

# 4. Primary User Jobs

The product must support the following jobs.

## Job A: Traditional terminal work

User wants:

> "Run commands quickly."

This must feel better than existing terminals.

---

## Job B: Project workspace

User wants:

> "Open my project and see everything related to it."

The workspace should understand:

- project root
- Git repository
- current branch
- running processes
- active agents
- worktrees
- recent activity
- relevant terminal sessions

---

## Job C: AI-assisted terminal work

User wants:

> "Explain this error."

or:

> "Generate the command for this."

or:

> "Fix this."

---

## Job D: Single AI agent

User wants:

> "Run Claude against this project."

The terminal must provide:

- launch
- status
- output
- approval
- pause
- stop
- resume
- restore

---

## Job E: Multiple AI agents

User wants:

> "Run several agents simultaneously."

The terminal must solve:

- pane organization
- status visibility
- agent identification
- context
- isolation
- notification
- failure recovery

---

## Job F: Agent orchestration

User wants:

> "Let several agents collaborate on one task."

The system must support:

- task decomposition
- dependencies
- parallel execution
- sequential pipelines
- handoffs
- reviews
- worktree isolation
- agent-to-agent communication
- final synthesis

---

# 5. Product UX North Star

The application should feel like:

> **Finder + modern terminal + ChatGPT + task manager + multi-agent control center**

without becoming a conventional IDE.

The terminal remains central.

The UI should not become an IDE clone.

---

# 6. Primary UI

The core layout should resemble:

```text
┌──────────────────────────────────────────────────────────────┐
│ 🔍 Search / Ask / Run anything...                     ⌘K    │
├────────────────────┬─────────────────────────────────────────┤
│ WORKSPACES         │ ACTIVE WORK                            │
│                    │                                         │
│ 🟢 My App          │ Authentication                          │
│                    │                                         │
│   Authentication   │ ┌────────────┬────────────┐             │
│   Payments         │ │ Claude     │ Codex      │             │
│   Infrastructure   │ │ 🟢 Working │ 🟢 Working │             │
│                    │ ├────────────┼────────────┤             │
│ 🟢 Website         │ │ Pi         │ OpenCode   │             │
│                    │ │ 🟡 Waiting │ ✅ Done    │             │
│                    │ └────────────┴────────────┘             │
├────────────────────┴─────────────────────────────────────────┤
│ Authentication                                               │
│                                                             │
│ ✓ 14 files changed                                          │
│ ✓ 37 tests passed                                           │
│ ⚠ 1 approval required                                       │
│                                                             │
│ [Review Changes] [Approve] [Ask Agent]                     │
└──────────────────────────────────────────────────────────────┘
```

The user must always be able to reveal the raw terminal.

---

# 7. Attention Model

When many agents are running, the application should not make all activity equally prominent.

Every agent should have a state:

```text
Idle
Starting
Working
Thinking
Waiting
NeedsApproval
Blocked
Error
Completed
Crashed
Disconnected
Paused
```

Use clear visual hierarchy.

The workspace should summarize:

```text
14 agents running
2 need you
5 completed
7 running normally
```

The user should not have to inspect 20 panes to discover that one agent needs intervention.

---

# 8. The Core Terminal Engine

Implement a native terminal engine.

Primary language:

> **Rust**

The initial implementation should target:

> **macOS first**

Then Linux.

Then Windows.

Do not attempt full cross-platform parity in the first milestone.

---

# 9. Core Architecture

Use this conceptual architecture:

```text
                    Native UI
                       │
                       ▼
                  Rust Core
                       │
      ┌────────────────┼─────────────────┐
      │                │                 │
      ▼                ▼                 ▼
 Terminal Engine   Agent Runtime     Workspace Engine
      │                │                 │
      ▼                ▼                 ▼
    PTYs          Agent Adapters      Persistence
      │                │                 │
      └────────────────┼─────────────────┘
                       ▼
                    Event Bus
                       │
          ┌────────────┼────────────┐
          ▼            ▼            ▼
         UI       Orchestrator    Storage
```

AI and orchestration must not block terminal rendering.

---

# 10. Terminal Requirements

Support:

- PTY
- ANSI
- VT100
- VT220
- truecolor
- 256 colors
- Unicode
- emoji
- combining characters
- wide characters
- RTL text where practical
- mouse reporting
- bracketed paste
- alternate screen
- OSC sequences
- hyperlinks
- clipboard integration
- cursor movement
- synchronized output
- Kitty graphics where practical
- sixel where practical
- shell integration
- shell detection
- window resizing

Support:

- bash
- zsh
- fish
- sh
- PowerShell
- cmd
- WSL
- SSH

---

# 11. Terminal Renderer

GPU accelerate terminal rendering.

Rendering pipeline:

```text
PTY output
    ↓
VT parser
    ↓
Terminal state
    ↓
Dirty-region tracking
    ↓
Render scheduler
    ↓
GPU
```

Never redraw the entire terminal if only a few rows changed.

Implement:

- dirty rows
- dirty cells where useful
- batched updates
- frame scheduling
- efficient glyph caching
- efficient scroll operations
- incremental rendering

The UI must remain responsive while a process produces extremely large output.

---

# 12. Scrollback

Do not store unlimited terminal output as expensive per-cell objects.

Design:

```text
Hot buffer
   ↓
compressed chunks
   ↓
optional disk-backed history
```

Scrollback memory usage must remain bounded.

---

# 13. Initial Performance Targets

Treat these as engineering budgets.

```yaml
cold_start_ms: 250
warm_start_ms: 100
idle_ram_mb: 40
ten_panes_ram_mb: 80
twenty_panes_ram_mb: 120
idle_cpu_percent: 1
input_latency_ms_p95: 8
render_latency_ms_p95: 16
binary_size_mb: 15
```

These are targets, not marketing claims.

Benchmark continuously.

If a feature violates the budget, redesign it instead of simply increasing the budget.

---

# 14. Multiplexer

Build a native multiplexer rather than requiring tmux for normal workflows.

Support:

- tabs
- splits
- panes
- pane resizing
- pane movement
- workspace switching
- pane focus
- detached sessions
- session restore

A pane must be a first-class object.

Example:

```rust
Pane {
    id
    workspace_id
    type
    process_id
    cwd
    title
    state
    agent_id
    activity
}
```

---

# 15. Pane Types

Support these abstractions:

```text
Terminal
Agent
Browser/Preview
Logs
Diff
Editor
```

Do not implement every pane type immediately.

V1 should focus on:

```text
Terminal
Agent
Logs
Diff
```

---

# 16. Workspace Model

Workspace should be the primary unit of organization.

Example:

```text
Workspace: Payments

├── API
│   ├── Claude
│   └── Terminal
│
├── Frontend
│   └── Codex
│
├── Tests
│   └── Pi
│
└── Infrastructure
    └── SSH
```

Workspace metadata should include:

- project path
- Git repository
- branch
- panes
- pane layout
- processes
- agents
- worktrees
- environment
- activity
- notifications
- task state

---

# 17. Workspace Persistence

Persist:

- workspace structure
- tabs
- pane geometry
- pane metadata
- project root
- Git metadata
- agent identity
- agent state where supported
- recent context
- tasks

Separate:

### UI restoration

Always attempt.

### Process restoration

Best effort.

### Agent restoration

Only when the underlying agent supports resume/recovery.

Do not falsely claim arbitrary processes can be checkpointed.

---

# 18. Agent Abstraction

Create a generic abstraction:

```text
Agent
AgentAdapter
AgentCapabilities
AgentProtocol
AgentSession
AgentLifecycle
AgentState
AgentPermissions
AgentMetrics
```

Every agent must expose a normalized model.

Example:

```json
{
  "id": "agent-123",
  "name": "Claude",
  "status": "working",
  "workspace": "payments",
  "task": "Implement checkout",
  "cwd": "/repo/payments",
  "runtime": "cli",
  "protocol": "terminal",
  "capabilities": {
    "resume": true,
    "pause": false,
    "approval": true
  }
}
```

---

# 19. Agent Integration Levels

Support three levels.

## Level 1: Generic CLI

Any executable can run as an agent.

Example:

```bash
my-agent
```

The terminal manages:

- process
- stdin
- stdout
- stderr
- lifecycle
- pane
- notifications

---

## Level 2: Native adapter

For major agents:

- Claude Code
- Codex
- OpenCode
- Pi
- Gemini CLI

Use agent-specific hooks or APIs where available.

---

## Level 3: Protocol integration

Support ACP where appropriate.

Use standards rather than inventing proprietary communication for capabilities that the ecosystem already standardizes. ACP is explicitly designed to standardize communication between editors and coding agents. 

---

# 20. Agent Registry

Create a configurable registry.

Example:

```yaml
name: Claude Code
command: claude
protocol: terminal
supports_resume: true
supports_hooks: true
supports_approval: true
```

Do not require a code change to add every future agent.

---

# 21. Agent State Detection

Agents must expose normalized state:

```text
Starting
Working
Thinking
RunningTool
Waiting
NeedsApproval
Blocked
Error
Completed
Crashed
Disconnected
```

Detection should be event-based whenever possible.

Do not implement inefficient polling loops.

---

# 22. Agent Observability

For every agent expose:

```text
Agent
Model
Task
Status
Duration
Tokens
Estimated cost
Files changed
Commands executed
Tool calls
Errors
Last activity
```

Example:

```text
Claude

Architecture

🟢 Working
14m 22s
$0.91

Files changed: 17
Tools: 42
Commands: 19
Tokens: 81k
```

---

# 23. Agent Activity Summary

Never require users to read raw output.

Generate concise local or model-assisted summaries:

```text
Claude is working

✓ Read 14 files
✓ Modified 8 files
✓ Added OAuth
✓ Added 37 tests
→ Running integration tests
```

The raw terminal remains available.

---

# 24. Multi-Agent Orchestration

Implement a native task graph.

Example:

```text
Implement authentication
│
├── Architecture
├── Backend
├── Frontend
├── Tests
└── Security
```

Task states:

```text
Queued
Ready
Running
Blocked
Review
Completed
Failed
Cancelled
```

Tasks must support dependencies.

---

# 25. Orchestration Modes

Implement:

## Parallel

```text
Task
├── Agent A
├── Agent B
└── Agent C
```

## Sequential

```text
Research
 ↓
Implementation
 ↓
Tests
 ↓
Review
```

## Hierarchical

```text
Manager
├── Backend
├── Frontend
├── QA
└── Security
```

## Race

```text
Agent A
Agent B
Agent C
  ↓
Evaluator
```

## Debate / Review

```text
Agent A proposes
Agent B critiques
Agent A revises
```

---

# 26. Worktree Isolation

For parallel coding agents, create isolated Git worktrees.

Example:

```text
repo/
worktrees/
  agent-claude-auth/
  agent-codex-ui/
  agent-pi-tests/
```

Each agent receives:

- branch
- worktree
- task
- workspace
- environment
- permissions

The user can review and merge results.

---

# 27. Agent-to-Agent Communication

Create an internal event system.

Events:

```text
TASK_CREATED
TASK_STARTED
TASK_COMPLETED
TASK_FAILED
BLOCKED
REQUEST_REVIEW
ARTIFACT_CREATED
PERMISSION_REQUESTED
AGENT_HANDOFF
```

Prefer structured communication over scraping terminal output.

---

# 28. Agent Handoff

If one agent fails, reaches a limit or becomes unavailable, allow another agent to continue.

Example:

```text
Claude failed.

Reason:
Authentication limit reached.

Available alternatives:

Codex
OpenCode
Pi

Context can be transferred.

[Continue with Codex]
```

The objective is to make the system agent-independent.

---

# 29. Agent Failover

Implement graceful failover for:

- auth failure
- network failure
- agent crash
- agent timeout
- model unavailable
- rate limit
- provider outage

Never silently change providers.

Require explicit policy or user approval for automatic handoff.

---

# 30. Universal Approval Layer

Normalize dangerous operations across agents.

Example:

```text
Claude wants to execute:

rm -rf ./build

Risk: High

[Approve once]
[Approve for workspace]
[Deny]
```

Permissions should cover:

```text
Filesystem
Network
Processes
Environment variables
Secrets
Shell
System directories
```

---

# 31. Security Modes

Create:

### Normal

Full user environment subject to standard permissions.

### Safe

Restrict agent access.

Example:

```text
Filesystem: project only
Network: blocked unless approved
Secrets: blocked
sudo: blocked
System paths: blocked
```

### Autonomous

Higher automation with explicit risk acknowledgment and strong sandbox boundaries.

Do not conflate "autonomous" with "unrestricted."

---

# 32. BYOK

Create a provider abstraction:

```text
Provider
Credential
Endpoint
Model
Agent
```

Example:

```text
Claude → Anthropic
Codex → OpenAI
OpenCode → OpenRouter
Custom Agent → localhost
```

Credentials must use native OS secret storage.

Never write plaintext API keys to configuration files.

---

# 33. Local Model Support

Eventually support:

- Ollama
- LM Studio
- llama.cpp
- MLX
- vLLM endpoints
- OpenAI-compatible local endpoints

Do not bundle a heavyweight inference engine into the terminal in V1.

The terminal should remain lightweight.

---

# 34. AI Command Assistance

Support:

```text
Generate command
Explain command
Explain error
Fix command
Suggest next step
Summarize logs
Search terminal history
```

Examples:

User:

> Find large files modified this week.

Output:

```bash
find . -type f -mtime -7 -size +100M
```

User:

> Why did this build fail?

Provide:

```text
Primary error
Affected component
Relevant command
Suggested fix
```

---

# 35. Context Engine

The AI context engine should understand:

- current directory
- project
- Git branch
- Git state
- active processes
- pane state
- recent commands
- recent output
- active agents
- errors
- ports
- environment
- worktrees

Do not continuously send all context to an LLM.

Use:

```text
Raw Event
 ↓
Local relevance filter
 ↓
Relevant context
 ↓
AI request only when needed
```

---

# 36. Project Memory

Store project-level context:

```text
Project
├── commands
├── agents
├── sessions
├── tasks
├── worktrees
├── preferences
├── environment
└── AI memory
```

User should eventually be able to say:

> Continue what we were doing yesterday.

The application should understand project continuity.

---

# 37. Command Palette

Implement a universal command palette.

Shortcut:

```text
⌘K
Ctrl+K
```

Capabilities:

```text
Run
Ask
Open
Find
Connect
Create workspace
Spawn agent
Switch workspace
Search history
Search files
Search tasks
```

Examples:

> Open payment project.

> Show agents that need me.

> Start Claude on backend.

> Connect to staging.

> Explain this error.

> Create three agents for this task.

---

# 38. Non-Technical Experience

The product must work even when a user does not understand shells.

Provide an optional intent layer:

```text
What would you like to do?

Open a project
Run something
Connect to a server
Build something
Ask AI
```

Do not force users through this layer.

It should disappear naturally as users become proficient.

---

# 39. Learn Mode

Optional.

When enabled, explain commands briefly.

Example:

```bash
git checkout -b feature/login
```

UI:

```text
Git
checkout = switch branch
-b = create a new branch

Creates:
feature/login
```

Advanced users can disable this completely.

---

# 40. Notifications

Notifications should be intelligent.

Notify when:

- agent completes
- agent fails
- agent needs approval
- agent is blocked
- long-running command finishes
- remote connection drops
- task pipeline completes

Do not notify every time terminal output changes.

---

# 41. Git Integration

Expose:

- branch
- status
- diff
- stage
- commit
- checkout
- stash
- worktree
- history

Git operations must execute asynchronously.

Git must never block the UI.

---

# 42. Remote Support

Eventually support:

- SSH
- remote workspaces
- containers
- dev containers
- remote agent execution
- remote session attachment

The mental model should be:

```text
Workspace
 ├── Local
 ├── SSH server
 ├── Container
 └── Cloud VM
```

The user should not need to understand where the process is physically executing.

---

# 43. CLI / Socket API

Expose a programmable CLI:

```bash
terminal workspace create payments
terminal workspace list
terminal pane split
terminal pane focus
terminal agent spawn claude
terminal agent list
terminal agent stop claude-1
terminal agent pause claude-1
terminal notify "Build complete"
```

Also expose an internal IPC/socket API.

Design the API for both:

- humans
- agents
- plugins
- automation

---

# 44. Plugin Architecture

Plugins should communicate through a process boundary.

Do not initially load arbitrary plugin native code into the terminal process.

Concept:

```text
Terminal
   │
   └── Plugin Host
         ├── Plugin A
         ├── Plugin B
         └── Plugin C
```

Potential APIs:

```text
workspace.create()
workspace.list()
pane.create()
pane.split()
agent.spawn()
agent.stop()
agent.send_message()
terminal.read_output()
notification.send()
```

---

# 45. Performance Engineering Rules

These are hard constraints.

Never:

1. Put AI inference into the render path.
2. Use Electron for the terminal core.
3. Use a browser engine permanently for basic terminal functionality.
4. Poll agent state aggressively.
5. Keep unlimited scrollback in expensive memory structures.
6. Redraw unchanged terminal regions.
7. Block the UI thread on subprocesses.
8. Block rendering on Git.
9. Block rendering on SSH.
10. Block rendering on indexing.
11. Assume more RAM is an acceptable solution.
12. add features that silently breach performance budgets.

---

# 46. Testing Strategy

Create five testing layers.

## Layer 1: Terminal correctness

Test:

- ANSI
- Unicode
- emoji
- combining characters
- wide characters
- colors
- cursor movement
- resizing
- clipboard
- mouse
- alternate screen
- TUI applications
- shell control sequences

Create golden rendering tests.

---

## Layer 2: Performance

Benchmark:

```text
Startup
10k output
100k output
1M output
10M output
Scrolling
Search
Pane creation
Workspace creation
Workspace restore
```

Test:

```text
1 pane
5 panes
10 panes
20 panes
50 panes
100 panes
```

Record:

- RAM
- CPU
- frame latency
- input latency
- startup time
- battery impact

---

## Layer 3: Agent compatibility

Every supported agent must pass:

```text
Launch
Output
Status detection
Pause where supported
Stop
Restart
Resume where supported
Approval
Notification
Restore
```

Create an automated compatibility matrix.

---

## Layer 4: Multi-agent stress

Run:

```text
20 agents
10 agents streaming
5 agents writing
multiple worktrees
long-running tasks
simultaneous approvals
agent failures
network failure
sleep/wake
```

Validate:

- no deadlock
- no crash
- no memory leak
- no lost session
- no state corruption
- UI remains responsive

---

## Layer 5: Reliability / Soak

Run 24-hour, 48-hour and 7-day tests.

Look for:

- memory leaks
- process leaks
- PTY leaks
- GPU memory leaks
- file descriptor leaks
- event bus growth
- deadlocks
- zombie processes

---

# 47. Benchmark Command

Provide:

```bash
terminal benchmark
```

Example:

```text
Terminal Benchmark

Startup                91 ms
Renderer                2.4 ms
10k output              0.13 s
100k output             0.71 s
1M output               5.42 s

1 pane                  42 MB
10 panes                61 MB
20 panes                93 MB

Idle CPU                0.4%
Input latency p95       4.2 ms
Frame latency p95       8.7 ms
```

The benchmark should be reproducible.

---

# 48. CI Performance Gates

Create:

```yaml
performance_budget:
  startup_ms: 250
  idle_ram_mb: 40
  ten_panes_ram_mb: 80
  twenty_panes_ram_mb: 120
  idle_cpu_percent: 1
  input_latency_p95_ms: 8
  render_latency_p95_ms: 16
```

CI must fail when important budgets regress.

Every significant pull request should report:

```text
Binary size delta
RAM delta
Startup delta
CPU delta
Tests
```

Example:

```text
PR #182

Binary: +0.4 MB
RAM: +1.8 MB
Startup: +6 ms
CPU: +0.1%
Tests: 3,421 passed
```

---

# 49. Product Analytics Without Default Telemetry

Do not require analytics in the initial version.

Design the application so a user can generate a local diagnostics package:

```text
terminal diagnostics
```

including:

- version
- OS
- architecture
- renderer stats
- performance stats
- crash metadata
- agent compatibility data
- configuration summary

Never include secrets.

---

# 50. Development Order

Follow this order unless there is a compelling technical dependency requiring deviation.

## Phase 0: Architecture

Deliver:

- architecture document
- repository structure
- ADRs
- benchmark harness
- CI
- performance budgets
- testing strategy

Do not build significant UI before architecture and benchmarks exist.

---

## Phase 1: Terminal Core

Implement:

- Rust core
- PTY
- VT parser
- GPU renderer
- keyboard
- clipboard
- scrollback
- resize
- shell support

Exit criteria:

The terminal must already demonstrate excellent performance.

---

## Phase 2: Multiplexer

Implement:

- tabs
- splits
- workspaces
- sidebar
- persistence
- Git metadata
- CLI control

Exit criteria:

20 panes remain responsive and memory remains within budget.

---

## Phase 3: Agent Runtime

Implement:

- Agent abstraction
- agent registry
- generic CLI agents
- lifecycle
- state detection
- notifications
- Claude
- Codex
- OpenCode
- Pi

Exit criteria:

All four agents can run simultaneously and be independently controlled.

---

## Phase 4: BYOK

Implement:

- provider abstraction
- secure credential storage
- Anthropic
- OpenAI
- Google
- OpenRouter
- local endpoints

Exit criteria:

Users can switch provider/model without changing the workspace model.

---

## Phase 5: Agent UX

Implement:

- status
- summaries
- approvals
- activity
- metrics
- cost
- files changed
- notifications
- session restoration

Exit criteria:

A user can manage 10+ agents without manually inspecting every pane.

---

## Phase 6: Orchestration

Implement:

- task graph
- scheduler
- dependencies
- parallel agents
- sequential pipelines
- worktrees
- handoffs
- reviews
- agent communication

Exit criteria:

A multi-agent task can execute from planning to review without manual pane management.

---

## Phase 7: Security

Implement:

- permissions
- safe mode
- filesystem policies
- network policies
- secret restrictions
- process restrictions
- audit log

Exit criteria:

Dangerous commands cannot bypass the permission system.

---

## Phase 8: Remote

Implement:

- SSH workspaces
- remote sessions
- remote agents
- reconnection
- session attachment

---

## Phase 9: Ecosystem

Implement:

- ACP
- plugin SDK
- socket API
- agent marketplace
- integrations

Do not prioritize marketplace before the core product is exceptional.

---

# 51. MVP Scope

The first publicly testable version should contain only:

### Terminal

- GPU rendering
- PTY
- tabs
- panes
- workspaces
- search
- clipboard
- shell support
- SSH

### Agent system

- Claude
- Codex
- OpenCode
- Pi
- generic CLI agents

### Agent UX

- state
- notifications
- pause/stop
- activity
- summaries
- session restoration

### BYOK

- Anthropic
- OpenAI
- OpenRouter
- local OpenAI-compatible endpoint

### Automation

- CLI
- IPC/socket API

Do not attempt to ship a full IDE.

---

# 52. Explicitly Do NOT Build Yet

Do not prioritize:

- full code editor
- debugging environment
- Kubernetes UI
- cloud infrastructure management
- built-in inference engine
- full browser
- full GitHub client
- team collaboration
- SaaS backend
- complex analytics
- marketplace
- social features

These can come later.

---

# 53. Competitive Strategy

The product should intentionally combine strengths from existing products while avoiding their weaknesses.

### From Ghostty

Take:

- native performance
- clean UX
- efficient rendering

### From Alacritty

Take:

- minimalism
- performance discipline

### From WezTerm

Take:

- programmable architecture
- multiplexer capabilities

But do not require Lua/configuration expertise.

### From Kitty

Take:

- advanced terminal capabilities

But avoid excessive complexity.

### From iTerm2

Take:

- mature terminal workflow features

But modernize the architecture.

### From tmux

Take:

- persistence
- multiplexing
- remote workflow

But make them native and discoverable.

### From Warp

Take:

- approachable UX
- command-oriented interaction
- AI assistance

But avoid forcing users into a proprietary AI workflow.

### From cmux

Take:

- workspaces
- agent visibility
- notifications
- browser/automation concepts
- terminal-native agent workflow

### From herdr

Take:

- multi-agent visibility
- persistent agent sessions
- agent state
- agent socket control

### From Terax

Take:

- extreme lightweight philosophy
- BYOK
- local-first
- no unnecessary backend dependency

### From Zed / Codex / Claude

Take:

- agent threads
- parallel agents
- worktrees
- task-oriented UX
- agent-native workflows

The final product must unify these ideas into one coherent system instead of appearing as a collection of copied features.

---

# 54. The Core Differentiator

The application must solve this problem:

Today:

```text
Developer
   ↓
Terminal
   ↓
tmux
   ↓
multiple panes
   ↓
Claude
Codex
Pi
OpenCode
   ↓
Git worktrees
   ↓
SSH
   ↓
manual tracking
   ↓
manual orchestration
```

Target:

```text
Developer
      ↓
  One Workspace
      ↓
   Intent / Task
      ↓
   Orchestrator
      ↓
 ┌────┼────┐
 ▼    ▼    ▼
AI   AI    AI
 │    │    │
 └────┼────┘
      ▼
Terminal / Worktrees / SSH
      ▼
Results
      ▼
Human Review
```

The user should manage **work**, not infrastructure.

---

# 55. UX North Star

Every interaction should answer:

### "What am I trying to accomplish?"

rather than:

### "What terminal technology do I need to understand?"

The product should gradually expose technical concepts only when they become useful.

---

# 56. The Core "Addictive" Loop

The product should create a strong workflow loop:

```text
Open workspace
     ↓
See project status
     ↓
See agents
     ↓
See what needs attention
     ↓
Approve / review / intervene
     ↓
See progress
     ↓
Complete work
     ↓
Resume tomorrow
```

The terminal should become the user's **work dashboard**, not just a command window.

---

# 57. Morning Experience

When returning to a project, show:

```text
Good morning.

Yesterday:
✓ Authentication complete
✓ 37 tests passed
✓ PR merged

Today:
3 agents running
2 tasks waiting
1 review needed

[Continue]
```

This should be based on local workspace state.

---

# 58. Universal Agent Handoff

Users should never be trapped by an individual provider or agent.

Example:

```text
Claude reached a limit.

Continue this task with:

Codex
OpenCode
Pi

Context preserved.

[Continue with Codex]
```

This creates long-term product resilience against provider changes.

---

# 59. Universal Agent Control

Every agent should expose a common set of actions where supported:

```text
Start
Stop
Pause
Resume
Restart
Approve
Deny
Send instruction
Fork
Clone
Handoff
Open logs
View diff
Open workspace
```

Unavailable actions should be represented honestly.

Never fake capabilities.

---

# 60. Human Control Must Always Win

The system may automate aggressively, but the human should always have:

```text
Stop everything
Pause all
Approve all
Deny
View exact command
View changed files
View diff
View raw logs
```

One global emergency action:

> **STOP ALL AGENTS**

must exist.

---

# 61. Reliability Standards

The application must prefer:

- explicit failure
- recoverable state
- transparent errors
- safe defaults

over:

- silent retries
- hidden automation
- unexplained provider switching
- pretending recovery succeeded

Every background subsystem should have:

```text
health
state
retry policy
timeout
failure handling
```

---

# 62. Documentation Requirements

Create documentation alongside implementation.

The repository must contain:

```text
/docs
  architecture.md
  product-principles.md
  ux-principles.md
  performance.md
  agent-architecture.md
  orchestration.md
  security.md
  persistence.md
  plugin-system.md
  cli-api.md
  testing.md
  benchmarks.md
  compatibility.md
  adr/
```

Every major architectural decision requires an ADR.

Each ADR must include:

```text
Problem
Context
Decision
Alternatives
Trade-offs
Consequences
```

---

# 63. Repository Structure

Use a modular repository approximately like:

```text
/
├── apps/
│   └── desktop/
│
├── crates/
│   ├── terminal-core/
│   ├── terminal-parser/
│   ├── terminal-renderer/
│   ├── pty/
│   ├── process-manager/
│   ├── workspace/
│   ├── multiplexer/
│   ├── agent-core/
│   ├── agent-adapters/
│   ├── orchestration/
│   ├── permissions/
│   ├── providers/
│   ├── persistence/
│   ├── ssh/
│   ├── ipc/
│   └── plugins/
│
├── benchmarks/
├── tests/
├── docs/
└── scripts/
```

Adapt this structure where necessary based on the chosen UI framework.

---

# 64. Engineering Process

For every feature:

1. Define user problem.
2. Define UX behavior.
3. Define architecture.
4. Define performance impact.
5. Define security implications.
6. Define failure modes.
7. Implement.
8. Add unit tests.
9. Add integration tests.
10. Add performance benchmark.
11. Validate manually.
12. Update documentation.
13. Record ADR if architecture changed.

Do not ship features without tests.

Do not make architectural changes without documenting the reasoning.

---

# 65. How You Should Work Autonomously

Do not ask me routine clarification questions.

Make reasonable decisions based on this specification.

When something is ambiguous:

1. Prefer simplicity.
2. Prefer local-first.
3. Prefer native performance.
4. Prefer extensibility.
5. Prefer agent neutrality.
6. Prefer user control.
7. Prefer an architecture that can evolve.
8. Prefer the smallest implementation that satisfies the requirement.

Only ask for clarification when proceeding would create irreversible architectural risk.

Otherwise choose a sensible default and document the assumption.

---

# 66. Do Not Overengineer

Avoid building abstractions before their necessity is demonstrated.

Do not implement:

- distributed microservices
- unnecessary databases
- cloud infrastructure
- remote control planes
- complex service meshes
- heavyweight dependency graphs

The initial product should be capable of running completely locally.

---

# 67. Do Not Fake Functionality

Never implement placeholder behavior that looks complete.

Especially do not fake:

- agent state
- approval
- security
- persistence
- resume
- orchestration
- cost
- provider support

When a capability is unavailable, represent it as unavailable.

---

# 68. Definition of Done

A feature is only complete when:

```text
Implementation
✓
Unit tests
✓
Integration tests
✓
Performance tests
✓
Failure tests
✓
UX validation
✓
Documentation
✓
Security review where relevant
✓
No unexplained performance regression
✓
No memory leak
✓
No UI freeze
```

---

# 69. First Task

Do not immediately implement the entire product.

Start by producing:

## Deliverable 1: Architecture Proposal

Include:

- component diagram
- threading model
- event model
- terminal renderer architecture
- PTY architecture
- workspace model
- agent runtime
- orchestration model
- persistence
- security
- IPC
- plugin architecture

## Deliverable 2: Technology Decision Record

Evaluate:

- Rust
- native UI
- Tauri
- libghostty / terminal rendering libraries
- GPU APIs
- SQLite
- IPC mechanisms
- credential storage
- PTY libraries
- SSH libraries
- ACP integration

Select a stack.

Explain why.

## Deliverable 3: Performance Plan

Define:

- startup benchmark
- render benchmark
- memory benchmark
- multi-pane benchmark
- massive-output benchmark
- agent stress benchmark
- soak test
- CI performance gates

## Deliverable 4: UX Specification

Define:

- onboarding
- workspace
- terminal
- panes
- agent dashboard
- command palette
- task graph
- approvals
- notifications
- summaries
- project memory
- progressive disclosure

## Deliverable 5: Repository Scaffold

Create:

- repository
- modules
- tests
- benchmark harness
- CI
- documentation structure

Do not build large features before these five deliverables exist.

---

# 70. First Prototype Milestone

After architecture is approved internally, build:

```text
Milestone 1

✓ Native terminal
✓ PTY
✓ GPU renderer
✓ One workspace
✓ Tabs
✓ Splits
✓ 20 pane test
✓ Session persistence
✓ CLI control
```

Then benchmark it.

Only once this is stable:

```text
Milestone 2

✓ Generic agent support
✓ Claude
✓ Codex
✓ OpenCode
✓ Pi
✓ Agent status
```

Then:

```text
Milestone 3

✓ BYOK
✓ Provider abstraction
✓ Orchestration
✓ Worktrees
✓ Task graph
```

---

# 71. Strategic Objective

Do not optimize for having the largest feature list.

Optimize for this experience:

> **A user opens the application and immediately understands what is happening, what they can do, and what needs their attention.**

A technical user should think:

> "This is incredibly powerful."

A non-technical user should think:

> "I understand what this is doing."

An AI power user should think:

> "I cannot go back to managing agents manually."

A performance-conscious developer should think:

> "How is this using so little RAM?"

That combination is the product goal.

---

# 72. Final Product Definition

The finished product should be:

> **A native, extremely lightweight terminal and AI workspace where humans and AI agents can work together across local and remote environments. It provides the speed and reliability of a high-performance terminal, the persistence and composability of a multiplexer, the transparency and control of an agent dashboard, and the simplicity of a modern desktop application. It is agent-neutral, model-neutral, provider-neutral and local-first.**

The terminal is not merely an interface to the shell.

It is the **execution layer**.

The workspace is not merely a window manager.

It is the **human control layer**.

The agent runtime is not merely a chatbot.

It is the **work execution layer**.

The orchestrator is not merely a prompt runner.

It is the **coordination layer**.

The final mental model is:

```text
                    HUMAN
                      │
                      ▼
               INTENT / TASK
                      │
                      ▼
                 WORKSPACE
                      │
          ┌───────────┼───────────┐
          ▼           ▼           ▼
      TERMINAL      AGENTS      REMOTE
          │           │           │
          └───────────┼───────────┘
                      ▼
                 ORCHESTRATOR
                      │
              ┌───────┼───────┐
              ▼       ▼       ▼
           Claude   Codex     Pi
              │       │       │
              └───────┼───────┘
                      ▼
               RESULTS / DIFF
                      │
                      ▼
                 HUMAN REVIEW
                      │
                      ▼
                     SHIP
```

## Non-negotiable product promise

> **You manage the work. The terminal manages the complexity.**

Build toward that relentlessly.

Do not sacrifice performance for feature count.

Do not sacrifice simplicity for power.

Do not sacrifice user control for automation.

Do not sacrifice openness for vendor lock-in.

Do not sacrifice transparency for AI magic.

Those five trade-offs define the product.