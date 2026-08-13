# UX Specification

This document defines the user experience principles and specifications for the AI-Native High-Performance Terminal. The goal is progressive disclosure: simple for beginners, infinitely powerful for experts.

## 1. Core UX Principles

1. **Human-First**: The UI must answer "What is happening?" and "What needs my attention?" without requiring the user to read raw logs.
2. **Progressive Disclosure**: Default UI is simple. Advanced controls (pane management, agent orchestration, raw PTY) appear only when needed or requested.
3. **Terminal-Native**: Despite the friendly UX, it must remain a real terminal. No fake abstractions that break existing CLI tools, TUIs, or shell workflows.
4. **Zero Unnecessary Complexity**: Hide infrastructure (PTYs, worktrees, SSH tunnels) unless the user explicitly chooses to manage them.

## 2. Onboarding Experience

- **First Launch**: 
  - Clean, minimal window. No overwhelming configuration wizards.
  - A single, friendly prompt: "What would you like to do?" with options: `Open a project`, `Run a command`, `Ask AI`.
  - Optional "Learn Mode" toggle, which briefly explains shell commands as they are typed (e.g., hovering over `git checkout -b` shows a tooltip: "Creates and switches to a new branch").
- **Provider Setup**: When the user first invokes an AI feature, prompt for API keys. Use OS-native keychain storage. Offer a "Use Local Model (Ollama)" alternative immediately to reinforce local-first privacy.

## 3. Workspace & Layout

- **Primary Unit**: The `Workspace` (tied to a project directory). 
- **Sidebar**: Shows a list of workspaces, with visual indicators for active agents (🟢 Working, 🟡 Waiting, 🔴 Needs Approval).
- **Main Area**: Tabbed pane layout. Users can split panes horizontally/vertically via drag-and-drop or keyboard shortcuts (`Cmd+D`, `Cmd+Shift+D`).
- **State Persistence**: On relaunch, the terminal restores the workspace, pane layout, and agent states (where the agent supports resumption).

## 4. The Terminal Pane

- **Rendering**: GPU-accelerated, truecolor, emoji, and ligature support. Zero visible tearing or lag during massive output.
- **Shell Integration**: Automatically detects command boundaries. Successful commands get a subtle green checkmark in the gutter; failed commands get a red indicator. Clicking the indicator shows a summary of the error.
- **Context Menu**: Right-clicking selected text offers: `Copy`, `Explain this error`, `Generate command from this`, `Search in project`.

## 5. Agent Dashboard & Visibility

When agents are running, they are first-class citizens in the UI, not just text in a terminal.

- **Agent Header**: Each agent pane has a persistent header showing:
  - Agent Name & Icon (e.g., Claude, Codex)
  - Current State (Working, Thinking, Waiting, Needs Approval)
  - Duration, Estimated Cost, Tokens Used
  - Quick Actions: `Pause`, `Stop`, `View Logs`, `View Diff`
- **Activity Summary**: Instead of forcing the user to watch streaming tokens, provide a collapsible "Summary" view:
  ```text
  ✓ Read 14 files
  ✓ Modified 8 files
  ✓ Added OAuth integration
  → Running integration tests...
  ```
- **Global Agent Status Bar**: A bottom bar summarizing the entire workspace: "14 agents running | 2 need you | 5 completed". Clicking it filters the view to only show agents requiring attention.

## 6. Command Palette (`Cmd+K` / `Ctrl+K`)

The universal entry point for all actions. Supports natural language and precise commands.

- **Examples**:
  - `> Open payments workspace`
  - `> Start Claude on backend`
  - `> Show agents that need me`
  - `> Split pane and run tests`
  - `> Explain the last error`
- **Fuzzy Matching**: Fast, client-side fuzzy search across workspaces, files, commands, and agent actions.

## 7. Task Graph & Orchestration UI

For multi-agent workflows, the UI shifts from a simple terminal to a task management view.

- **Visual Graph**: Nodes represent tasks; edges represent dependencies.
- **States**: Queued (gray), Ready (blue), Running (animated blue), Blocked (yellow), Review (orange), Completed (green), Failed (red).
- **Interaction**: Users can drag to reorder, right-click to assign a specific agent to a task, or click a node to open its dedicated terminal/diff pane.

## 8. Universal Approval Layer

Security and control are paramount. Dangerous operations trigger a non-dismissible, clear approval modal.

- **Modal Content**:
  - **Agent**: "Claude wants to execute:"
  - **Command**: `rm -rf ./build` (highlighted in red if high risk)
  - **Risk Assessment**: "High Risk: Deletes directory contents."
  - **Context**: Which file/workspace this affects.
- **Actions**: `[Approve Once]`, `[Approve for this Workspace]`, `[Deny]`, `[View Full Diff]`.

## 9. Notifications

Intelligent, non-intrusive, and actionable.

- **Notify When**: Agent completes, agent fails, agent needs approval, long-running command (>30s) finishes, remote connection drops.
- **Do Not Notify**: Every time terminal output changes, every tool call an agent makes (unless in verbose debug mode).
- **Format**: Native OS notifications that, when clicked, bring the specific agent pane to the foreground.

## 10. Project Memory & Continuity

- **Morning Experience**: When opening a workspace the next day, show a brief, locally-generated summary:
  ```text
  Good morning.
  Yesterday: ✓ Authentication complete, ✓ 37 tests passed.
  Today: 3 agents running, 1 review needed.
  [Continue Working]
  ```
- **Context Retention**: The terminal remembers recent commands, active branches, and agent tasks, allowing the user to say, "Continue what we were doing yesterday," without re-explaining the context.

## 11. Progressive Disclosure in Action

| User Level | Experience |
|------------|------------|
| **Beginner** | Types "Run tests" in Cmd+K. Terminal runs `npm test` and explains the output in plain English. |
| **Developer** | Types `claude` in the terminal. An agent pane opens, tied to the current directory, ready to code. |
| **Power User** | Uses `Cmd+D` to split panes, runs `terminal agent spawn claude --workspace payments`, and monitors the task graph. |
| **Orchestrator** | Defines a YAML task graph, hits "Run", and watches 5 agents collaborate across isolated worktrees, only intervening for approvals. |

## 12. Emergency Controls

Always visible, always accessible:
- **`Cmd+.` (or a dedicated red button)**: "STOP ALL AGENTS". Immediately pauses all agent processes and revokes their permissions.
- **Raw Terminal Toggle**: A button to instantly strip away all AI/UX chrome and show the pure, raw PTY output for debugging.