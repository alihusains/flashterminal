# ADR 0005: Execution Session Architecture

## Problem
The `Pane` model currently assumes a direct dependency on `TerminalSession` (via `SessionId`). This tightly couples the workspace engine to PTY-based terminal sessions, making it impossible to host AI agents, remote sessions, or log viewers in a pane without redesigning the core multiplexer.

## Goal
Allow terminal and agent sessions to share the same workspace infrastructure (layout, focus, rendering, persistence) without the `Pane` or `Multiplexer` knowing the implementation details of any specific session type.

## Options Considered
1. **Giant Trait (`trait ExecutionSession`)**: A single trait with all possible methods (`input`, `resize`, `output`, `pause`, etc.). *Rejected*: Leads to a bloated interface where many methods are unimplemented or panic for certain session types (e.g., agents don't need `resize`).
2. **Enum Dispatch (`enum Execution { Terminal(TerminalSession), Agent(AgentSession) }`)**: *Rejected*: Violates the Open/Closed Principle. Adding a new session type (e.g., `RemoteSession`) requires modifying the enum and all match arms across the codebase.
3. **Capability-Oriented Composition**: Define a minimal core identity and separate capability traits/interfaces. *Selected*: Allows sessions to expose only what they support, keeping the interface clean and extensible.
4. **Separate Pane Types**: *Rejected*: Breaks the uniform pane tree layout engine and complicates focus/input routing.

## Decision
We will use a **Capability-Oriented Composition** model centered around a stable `ExecutionId` and `ExecutionKind`.

- `Pane` holds an `ExecutionId` and `ExecutionKind`, plus lightweight metadata (title, cwd).
- The `Multiplexer` maintains registries for different session types, keyed by `ExecutionId`.
- Sessions expose capabilities via explicit traits (e.g., `CanInput`, `CanResize`, `CanObserve`).
- Agent-specific logic is entirely encapsulated within the `AgentRuntime` and `AgentAdapter` boundary, keeping the workspace engine provider-neutral.

## Consequences
- **Positive**: 
  - Extensibility: New session types can be added without modifying `Pane` or `Multiplexer` core logic.
  - Performance: No dynamic dispatch overhead in the hot render path; the multiplexer resolves `ExecutionId` to the correct session map.
  - Testing: Fake agents can be injected via the adapter interface without network dependencies.
- **Negative**: 
  - Slight increase in complexity when resolving an `ExecutionId` (requires checking multiple registries or a unified `ExecutionHandle` wrapper).
  - Persistence must store `ExecutionKind` to correctly restore sessions on startup.

## Migration Plan
1. Introduce `ExecutionId` and `ExecutionKind`.
2. Update `Pane` to use `execution_id` and `execution_kind` instead of `session_id`.
3. Update `Multiplexer` to route input/resize/drain based on `ExecutionKind`.
4. Maintain backward compatibility by treating existing `SessionId` as `ExecutionId` with `ExecutionKind::Terminal`.