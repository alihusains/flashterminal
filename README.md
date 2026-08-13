# FlashTerminal

> **The fastest terminal for humans and AI agents. One workspace. Any agent. Zero unnecessary terminal complexity.**

FlashTerminal is a high-performance, human-friendly AI-native terminal and agent workspace. It combines the speed of a native terminal, the persistence of a multiplexer, the visibility of a multi-agent control plane, and the simplicity of a modern desktop application.

## 🚀 Product Vision

AI agents are turning the terminal from a single-process command interface into an operating environment for multiple autonomous workers. FlashTerminal hides the infrastructure (PTYs, tmux, worktrees, SSH, agent states) while preserving complete power and transparency.

**Complexity must be available, never mandatory.**

## ✨ Key Features

- **Native Performance**: GPU-accelerated rendering, <250ms cold start, <40MB idle RAM. Built in Rust with `wgpu` and `winit`.
- **Agent-Native**: First-class support for Claude Code, OpenAI Codex, OpenCode, Pi, Gemini CLI, Aider, Cline, and arbitrary custom CLI agents.
- **Multi-Agent Orchestration**: Run, monitor, and coordinate multiple agents simultaneously with task graphs, dependencies, and Git worktree isolation.
- **Provider-Neutral & BYOK**: Use Anthropic, OpenAI, Google, OpenRouter, or local models (Ollama, LM Studio). You own your credentials.
- **Local-First Privacy**: No mandatory accounts, no default telemetry, OS-native credential storage.
- **Progressive Disclosure**: Simple for beginners, infinitely powerful for experts.

## 🏗️ Architecture

The project is structured as a Rust workspace:

```text
/
├── apps/desktop/          # Main application binary
├── crates/
│   ├── terminal-core/     # Core terminal state and logic
│   ├── terminal-parser/   # VT/ANSI escape sequence parser
│   ├── terminal-renderer/ # GPU-accelerated rendering (wgpu)
│   ├── pty/               # PTY management
│   ├── agent-core/        # Agent lifecycle and state management
│   ├── agent-adapters/    # Integrations for specific agents
│   ├── orchestration/     # Multi-agent task graph and scheduling
│   ├── workspace/         # Workspace and persistence model
│   └── ...                # Other specialized crates
├── benchmarks/            # Performance testing harness
├── docs/                  # Architecture, UX, and API documentation
└── .github/workflows/     # CI/CD with strict performance gates
```

## 📋 Getting Started (Development)

### Prerequisites
- Rust (latest stable)
- macOS (primary development target), Linux, or Windows

### Build & Run
```bash
# Clone the repository
git clone https://github.com/flashterminal/flashterminal.git
cd flashterminal

# Build the project
cargo build

# Run the desktop application
cargo run -p desktop

# Run tests
cargo test --workspace

# Run performance benchmarks
cargo run --release -p benchmarks
```

## 📚 Documentation

- [Architecture Proposal](docs/architecture.md)
- [Technology Decision Record (ADR 0001)](docs/adr/0001-technology-stack.md)
- [Performance Plan](docs/performance.md)
- [UX Specification](docs/ux-specification.md)

## 🛡️ Performance Budgets

We treat performance as a core feature. Our hard constraints:
- **Cold Start**: < 250 ms
- **Idle RAM**: < 40 MB
- **Input Latency (p95)**: < 8 ms
- **Binary Size**: < 15 MB

Every PR is validated against these budgets in CI.

## 🤝 Contributing

We welcome contributions! Please read our [Contributing Guidelines](CONTRIBUTING.md) and ensure all changes pass the performance gates.

## 📄 License

MIT License. See [LICENSE](LICENSE) for details.