# ADR 0006: Agent Runtime Architecture

## Problem
FlashTerminal needs to support AI agents natively within panes, but the current architecture is tightly coupled to PTY-based terminal sessions. We must introduce agent support without violating the core performance budgets, breaking existing terminal behavior, or coupling the workspace engine to specific vendor implementations.

## Goal
Establish a provider-neutral, capability-oriented execution abstraction that allows `Pane` to host different kinds of sessions (Terminal, Agent, and future types like Remote or Log) while preserving the Phase 0/1 performance guarantees.

## Options Considered
1. **Giant Trait (`trait ExecutionSession`)**: Rejected due to interface bloat and violation of the Interface Segregation Principle.
2. **Enum Dispatch (`enum Execution { Terminal, Agent }`)**: Rejected because it violates the Open/Closed Principle; adding a new session type requires modifying all match arms across the codebase.
3. **Capability-Oriented Composition with Unified Identity**: Selected. Uses a stable `ExecutionId` and `ExecutionKind`, with separate registries for different session types.

## Decision
- Introduce `ExecutionId` and `ExecutionKind` as the primary identifiers for pane content.
- Update `Pane` to hold `execution_id` and `execution_kind` instead of `session_id`.
- Create `AgentRuntime` and `AgentRegistry` to manage agent lifecycles independently of the terminal PTY manager.
- Define the `AgentAdapter` trait to isolate vendor-specific logic (e.g., Claude, Codex) from the core engine.
- Implement a `FakeAgent` for deterministic, network-free automated testing.

## Consequences
- **Positive**: 
  - Clean separation of concerns; the workspace engine remains agnostic to agent internals.
  - Extensibility: New session types can be added without modifying `Pane` or `Multiplexer` core logic.
  - Testability: `FakeAgent` allows comprehensive integration testing of the agent lifecycle.
- **Negative**: 
  - Slight increase in complexity when resolving an `ExecutionId` (requires checking the appropriate registry based on `ExecutionKind`).
  - Persistence must store `ExecutionKind` to correctly restore sessions on startup (agent sessions are best-effort restored in Phase 2A).

## Migration Plan
1. Update `Pane` model to use `ExecutionId` and `ExecutionKind`.
2. Refactor `Multiplexer` to maintain separate maps for `terminal_sessions` and `agent_runtime`.
3. Update all routing logic (input, resize, drain, persistence) to dispatch based on `ExecutionKind`.
4. Add IPC commands (`agent list`, `agent spawn`, `agent status`, `agent stop`) for CLI control.