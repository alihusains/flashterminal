# ADR 0001: Technology Stack Selection

## Problem
We need to select a technology stack for a high-performance, AI-native terminal that meets strict performance budgets (cold start < 250ms, idle RAM < 40MB, input latency p95 < 8ms) while providing a modern, extensible architecture for multi-agent orchestration.

## Context
The product must be native, avoid heavy browser runtimes in the core execution path, and support macOS first, then Linux, then Windows. It requires GPU-accelerated rendering, robust PTY management, and seamless integration with various AI agents.

## Decision

We will use a **Pure Rust** stack with the following components:

| Component | Technology | Rationale |
|-----------|------------|-----------|
| **Core Language** | Rust | Memory safety, zero-cost abstractions, excellent concurrency model, and top-tier performance. |
| **Windowing** | `winit` | Cross-platform, lightweight, and provides raw access to OS window events without UI framework overhead. |
| **GPU Rendering** | `wgpu` | Cross-platform GPU API (Vulkan, Metal, DX12). Allows custom, highly optimized terminal rendering without embedding a browser engine. |
| **VT Parser** | `vte` (or custom fork) | Proven, high-performance ANSI/VT escape sequence parser. We will fork and optimize it for our specific dirty-tracking needs. |
| **PTY Management** | `portable-pty` | Abstracts OS-specific PTY creation (posix_openpt, ConPTY) into a safe Rust API. |
| **Async Runtime** | `tokio` | Industry standard for async Rust. Required for non-blocking agent API calls, IPC, and concurrent task orchestration. |
| **Persistence** | `sqlite` (via `rusqlite`) | Lightweight, embedded, relational database. Perfect for workspace state, task graphs, and agent history without requiring a separate server. |
| **Credential Storage** | `keyring` | Securely interfaces with macOS Keychain, Linux Secret Service, and Windows Credential Manager. |
| **SSH Client** | `russh` | Pure Rust SSH implementation, avoiding C-dependencies like `libssh2`. |
| **IPC** | Unix Domain Sockets / Named Pipes + `serde_json` | Low-latency, secure local communication between the CLI, plugins, and the main daemon. |
| **UI Framework (Chrome)** | Custom `wgpu` + `taffy` (or `egui` for rapid prototyping) | To avoid "unnecessary JavaScript processes", we will build the UI chrome (sidebar, command palette) using the same `wgpu` renderer or a lightweight immediate-mode GUI like `egui` that integrates well with `wgpu`. |
| **Agent Protocol** | JSON-RPC over stdio/sockets | Aligns with emerging standards like ACP (Agent-Client Protocol) for editor/agent communication. |

### Alternatives Considered

1. **Tauri (Rust + Web Frontend)**
   - *Pros*: Rapid UI development, access to web ecosystem.
   - *Cons*: Embeds a WebView (WebKit/Chromium), which violates the "no permanently embedded heavy browser runtimes" principle and makes hitting the <40MB idle RAM budget extremely difficult.
   - *Decision*: Rejected for the core terminal. May be evaluated later *only* for non-performance-critical auxiliary dashboards, but pure native is preferred.

2. **Electron**
   - *Pros*: Massive ecosystem.
   - *Cons*: Fails all performance budgets (RAM, startup time, CPU). Explicitly forbidden by product principles.
   - *Decision*: Rejected.

3. **Zig (like Ghostty)**
   - *Pros*: Excellent performance, great C interoperability.
   - *Cons*: Smaller ecosystem for async networking, SQLite, and complex agent orchestration compared to Rust. Our team has stronger Rust expertise.
   - *Decision*: Rejected in favor of Rust for the broader ecosystem and `tokio`.

4. **Existing Terminal Emulators (Alacritty, Kitty) as a library**
   - *Pros*: Don't reinvent the wheel.
   - *Cons*: Alacritty is explicitly designed *not* to be a library. Kitty's codebase is highly complex and Python/C mixed. 
   - *Decision*: We will use individual crates (like `vte`, `portable-pty`) but build the integration and orchestration layer ourselves to maintain the "agent-native" focus.

## Consequences

- **Development Velocity**: Initial UI development will be slower than using a web framework, as we must build custom layout and rendering for the UI chrome.
- **Performance**: We will easily meet and exceed the strict performance budgets (RAM, CPU, startup time).
- **Maintainability**: A unified Rust codebase simplifies cross-platform reasoning and eliminates FFI boundaries for core logic.
- **Extensibility**: The plugin system will require careful design to ensure plugins cannot compromise the host's performance or security.

## Next Steps
1. Initialize the Rust workspace with the selected crates.
2. Build a minimal `winit` + `wgpu` + `vte` prototype to validate render latency and dirty-region tracking.
3. Establish the CI performance benchmarking pipeline.