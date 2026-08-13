# ADR 0001: Initial Architecture and Technology Stack

## Status
Accepted

## Context
We are building a high-performance, AI-native terminal. The foundational requirement is that the terminal core must be exceptionally fast, memory-efficient, and capable of serving as the execution engine for future AI agent orchestration. 

We must avoid the pitfalls of existing terminals that become slow and memory-heavy due to web-based rendering engines (Electron) or tightly coupled architectures.

## Decision

### 1. Language: Rust
- **Rationale**: Rust provides memory safety without garbage collection, ensuring predictable performance and low memory overhead. Its ecosystem has mature, high-quality crates for PTY management (`portable-pty`), terminal parsing (`vte`), and GPU rendering (`wgpu`).

### 2. Platform Target: macOS (Apple Silicon) First
- **Rationale**: macOS is the primary developer platform for our target audience. Apple Silicon provides excellent performance characteristics that we can leverage. Linux and Windows support will follow, facilitated by Rust's cross-compilation capabilities and `portable-pty`.

### 3. Windowing and Rendering: `winit` + `wgpu`
- **Rationale**: 
  - `winit` provides a lightweight, cross-platform windowing abstraction without the overhead of a full UI framework.
  - `wgpu` provides a modern, cross-platform GPU API (WebGPU native). It allows us to implement dirty-region rendering and glyph atlas rendering efficiently, targeting the < 16ms render latency budget.
  - **Rejected**: Electron, Tauri (for the core render path). While Tauri is lightweight, embedding a webview for the terminal render path introduces unnecessary overhead and makes it harder to guarantee the < 8ms input latency and < 40MB idle RAM budgets.

### 4. PTY Management: `portable-pty`
- **Rationale**: Provides a safe, cross-platform abstraction over native OS PTY APIs (`posix_openpt` on Unix, `ConPTY` on Windows).

### 5. Parsing: `vte`
- **Rationale**: A well-maintained, zero-allocation VT100/ANSI parser. We will implement the `vte::Perform` trait to update our custom, optimized terminal state grid.

### 6. Architecture Separation
The system is strictly layered:
```text
UI (winit)
  │
  ▼
Renderer (wgpu) ← Consumes Terminal State (Read-Only during render)
  │
  ▼
Terminal Parser (vte) → Updates Terminal State
  │
  ▼
PTY Manager (portable-pty) → Raw Bytes
```
- **Rule**: The renderer must *never* parse terminal protocols. The parser must *never* know about the GPU. This separation ensures the render path remains pure and optimizable.

## Consequences

### Positive
- Predictable, low-latency performance.
- Strong memory safety guarantees.
- Clear separation of concerns makes the codebase maintainable and testable.
- Easy to benchmark individual components (e.g., parser speed independent of rendering).

### Negative
- Steeper learning curve for contributors unfamiliar with Rust or GPU programming.
- Building a custom glyph renderer is more complex initially than embedding a webview. (Mitigation: Start with a simple colored-quad baseline, then incrementally add text rendering).

## References
- [Master Build Prompt](../Master%20Build%20Prompt_%20AI-Native%20High-Performance%20Terminal.md)
- [Build Phase 0 Prompt](../Build%20Phase%200_%20Terminal%20Foundation%20Bootstrap%20Prompt.md)