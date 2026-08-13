# Development Guide

This document outlines the development workflow, tooling, and guidelines for contributing to FlashTerminal.

## Prerequisites

- **Rust**: Latest stable version (`rustup update`)
- **macOS**: Primary development target (Apple Silicon recommended)
- **Linux/Windows**: Supported, but may require additional platform-specific dependencies

## Building and Running

### Initial Setup
```bash
# Clone the repository
git clone https://github.com/flashterminal/flashterminal.git
cd flashterminal

# Build all crates
cargo build

# Run the desktop application
cargo run -p desktop
```

### Development Workflow
1. **Make changes**: Edit code in the relevant crate.
2. **Format**: `cargo fmt --all`
3. **Lint**: `cargo clippy --workspace -- -D warnings`
4. **Test**: `cargo test --workspace`
5. **Benchmark**: `cargo bench -p benchmarks`

## Crate Structure

- `apps/desktop`: Main application binary tying together the UI, PTY, and renderer.
- `crates/terminal-core`: Core terminal state, grid management, and data structures. Optimized for low allocation.
- `crates/terminal-parser`: VT/ANSI escape sequence parser using the `vte` crate.
- `crates/terminal-renderer`: GPU-accelerated rendering using `wgpu` and `winit`.
- `crates/pty`: PTY management abstraction using `portable-pty`.
- `benchmarks`: Performance testing harness measuring startup, memory, and parsing latency.

## Performance Guidelines

**Performance is a core feature, not an afterthought.** Every contribution must respect the performance budgets:

- Cold Start: < 250 ms
- Idle RAM: < 40 MB
- Input Latency (p95): < 8 ms

### Rules for Contributors
1. **No unnecessary allocations**: Prefer pre-allocated buffers and `VecDeque` for the terminal grid.
2. **No blocking the main thread**: PTY I/O and agent communication must happen on background threads or async runtimes.
3. **Measure before optimizing**: Use `cargo bench` and `cargo-flamegraph` to identify actual bottlenecks.
4. **Keep the render path pure**: The renderer should only consume terminal state, never parse protocols or manage PTYs.

## Debugging

- Enable verbose logging: `RUST_LOG=debug cargo run -p desktop`
- Profile CPU usage: `cargo flamegraph --bin desktop`
- Profile memory: Use `valgrind --tool=massif` (Linux) or Xcode Instruments (macOS)

## CI/CD

All PRs are automatically checked against:
- `cargo check`
- `cargo test`
- `cargo clippy`
- `cargo fmt --check`
- Performance regression gates (via `benchmarks` crate)