# Architecture Proposal

## 1. Component Diagram

```text
                    ┌─────────────────────────────────────────────────────────┐
                    │                      Native UI                          │
                    │  (Window Management, Command Palette, Agent Dashboard)  │
                    └────────────────────────────┬────────────────────────────┘
                                                 │ IPC / FFI
                    ┌────────────────────────────▼────────────────────────────┐
                    │                      Rust Core                          │
                    │                                                         │
                    │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
                    │  │   Terminal   │  │    Agent     │  │  Workspace   │  │
                    │  │    Engine    │  │   Runtime    │  │    Engine    │  │
                    │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
                    │         │                 │                 │          │
                    │  ┌──────▼───────┐  ┌──────▼───────┐  ┌──────▼───────┐  │
                    │  │     PTY      │  │   Adapters   │  │ Persistence  │  │
                    │  │  Management  │  │ (Claude, etc)│  │  (SQLite)    │  │
                    │  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
                    │         │                 │                 │          │
                    └─────────┼─────────────────┼─────────────────┼──────────┘
                              │                 │                 │
                    ┌─────────▼─────────────────▼─────────────────▼──────────┐
                    │                       Event Bus                        │
                    │           (Async, Non-blocking, Local-first)           │
                    └────────────────────────────────────────────────────────┘
```

## 2. Threading Model

The application employs a multi-threaded, message-passing architecture to ensure the UI remains responsive at all times.

- **Main Thread (UI/Render)**: Handles window events, input processing, and GPU draw calls. It must never block. Target: <16ms frame time.
- **PTY/Process Thread(s)**: Dedicated thread(s) per PTY/session to read `stdout`/`stderr` without blocking the main thread. Uses asynchronous I/O.
- **Agent Runtime Thread(s)**: Isolated async runtime (e.g., `tokio`) for handling agent lifecycle, API calls, and tool executions. Completely decoupled from the render loop.
- **Workspace/Persistence Thread**: Background thread for debounced saving of workspace state, Git metadata, and session history to SQLite.
- **Orchestrator Thread**: Manages task graphs, dependencies, and multi-agent coordination. Communicates via the Event Bus.

## 3. Event Model

All cross-component communication flows through a central, typed **Event Bus** to prevent tight coupling.

- **Terminal Events**: `OutputReceived`, `TitleChanged`, `ProcessExited`, `ResizeRequested`.
- **Agent Events**: `AgentSpawned`, `StatusChanged`, `ApprovalRequired`, `TaskCompleted`, `ToolCallExecuted`.
- **Workspace Events**: `WorkspaceOpened`, `PaneCreated`, `LayoutChanged`, `GitStatusUpdated`.
- **Orchestration Events**: `TaskQueued`, `TaskStarted`, `TaskBlocked`, `HandoffRequested`.

Events are published asynchronously. Subscribers (e.g., UI, persistence) react without blocking the publisher.

## 4. Terminal Renderer Architecture

The renderer is built for maximum performance, avoiding heavy browser runtimes.

1. **PTY Output**: Raw bytes received from the PTY.
2. **VT Parser**: Incremental parsing of ANSI/VT escape sequences into semantic operations (e.g., `WriteChar`, `MoveCursor`, `SetColor`).
3. **Terminal State**: A grid of cells representing the current screen state. Only *dirty* regions are flagged.
4. **Dirty-Region Tracking**: Minimizes the data passed to the GPU.
5. **Render Scheduler**: Batches updates and synchronizes with the display's refresh rate (VSync).
6. **GPU (wgpu)**: Renders glyphs using a pre-computed atlas, applying colors and effects via shaders.

## 5. PTY Architecture

- Uses native OS PTY APIs (`posix_openpt` on macOS/Linux, `ConPTY` on Windows).
- Abstracted behind a `PtyManager` that handles spawning, resizing, and I/O streaming.
- Supports shell integration protocols (e.g., iTerm2 shell integration, FinalTerm) to track command boundaries and exit codes.

## 6. Workspace Model

The `Workspace` is the root organizational unit.

```rust
struct Workspace {
    id: WorkspaceId,
    name: String,
    root_path: PathBuf,
    git_metadata: Option<GitMetadata>,
    panes: Vec<Pane>,
    layout: LayoutTree,
    active_agents: Vec<AgentSession>,
    environment: HashMap<String, String>,
}

struct Pane {
    id: PaneId,
    pane_type: PaneType, // Terminal, Agent, Logs, Diff
    process_id: Option<Pid>,
    cwd: PathBuf,
    title: String,
    state: PaneState,
    agent_id: Option<AgentId>,
}
```

## 7. Agent Runtime

Agents are treated as first-class citizens, abstracted behind a uniform `Agent` trait.

- **Agent Core**: Manages lifecycle (spawn, pause, resume, stop), state tracking, and resource limits.
- **Agent Adapters**: Implement the `Agent` trait for specific tools (Claude Code, Codex, generic CLI). They translate generic commands into agent-specific invocations and parse their output/state.
- **Protocol Layer**: Supports standard protocols like ACP (Agent-Client Protocol) via JSON-RPC over stdio or sockets.

## 8. Orchestration Model

A native task graph engine manages multi-agent workflows.

- **Task Graph**: Directed Acyclic Graph (DAG) of tasks with dependencies.
- **Scheduler**: Evaluates task readiness, assigns agents, and handles parallel/sequential execution.
- **Isolation**: Automatically provisions Git worktrees for parallel coding agents to prevent conflicts.
- **Handoff**: If an agent fails or hits a limit, the orchestrator can serialize the context and assign the task to an alternative agent.

## 9. Persistence

- **Storage Engine**: SQLite for structured, relational data (workspaces, tasks, agent history).
- **Session State**: Serialized to disk periodically and on graceful exit. Restored on launch.
- **Scrollback**: Hot buffer in memory, compressed chunks for recent history, optional disk-backed history for unlimited scrollback.
- **Credentials**: OS-native keychain (via `keyring` crate). Never stored in plaintext.

## 10. Security

- **Permission Layer**: Intercepts and evaluates agent actions (filesystem, network, process execution) against user-defined policies.
- **Security Modes**: `Normal`, `Safe` (restricted to project directory, no network/secrets), `Autonomous` (with explicit risk acknowledgment).
- **Sandboxing**: Agents run with minimal privileges. Dangerous operations require explicit user approval via a universal approval UI.

## 11. IPC (Inter-Process Communication)

- **Socket API**: Unix domain sockets (macOS/Linux) or named pipes (Windows) for external control.
- **CLI**: A `flashterminal` CLI binary that communicates with the running instance via the socket API to execute commands like `workspace create`, `agent spawn`, etc.
- **Protocol**: JSON-RPC or a lightweight custom binary protocol for high-throughput internal communication.

## 12. Plugin Architecture

- **Process Boundary**: Plugins run as separate processes to prevent crashes or memory leaks from affecting the core terminal.
- **Plugin Host**: Manages plugin lifecycle, IPC communication, and capability granting.
- **API**: Plugins can subscribe to events, read terminal output (with permission), create panes, and spawn agents via the IPC API.