//! Command abstraction (§15–16): an action registry decoupled from input
//! handlers so the command palette / customization / platform bindings can
//! be layered on later without touching the engine.

use serde::{Deserialize, Serialize};

use crate::model::{PaneId, SplitDirection, WorkspaceId};

/// FlashTerminal commands. Phase 1 covers terminal/workspace actions;
/// Phase 2C (§37) adds the agent command palette + keyboard actions (§36).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    FocusNext,
    FocusPrevious,
    ResizePaneLeft,
    ResizePaneRight,
    ResizePaneUp,
    ResizePaneDown,
    ZoomPane,
    NewTab,
    CloseTab,
    NextTab,
    PreviousTab,
    NewWorkspace,
    SwitchWorkspace(WorkspaceId),
    CloseWorkspace,
    // --- Phase 2C: agent commands (§36–§37) ---
    ShowAgents,
    ShowAgentsNeedingAttention,
    ShowFailedAgents,
    ShowCompletedAgents,
    FocusNextAgent,
    FocusPreviousAgent,
    FocusAgent(PaneId),
    ToggleAgentWorkView,
    ReviewAgentChanges,
    OpenAgentLogs,
    StopAgent,
    RestartAgent,
    ResumeAgent,
    Approve,
    Deny,
    ToggleQuietMode,
    ToggleCommandPalette,
    // --- Phase 3A: task orchestration (§43, §55 — minimal UI) ---
    /// `task.run` — schedules the whole workflow graph.
    RunTasks,
    /// Opens the task dashboard overlay.
    ToggleTasks,
    /// Opens the create-task form (3A.1 §6 palette completeness).
    CreateTask,
    /// Dashboard filtered to blocked tasks.
    ShowBlockedTasks,
    /// Dashboard filtered to tasks needing review.
    ShowTasksNeedingReview,
    /// Selects the first task and opens its detail panel.
    OpenTask,
    /// Cancels the task selected in the task dashboard.
    CancelSelectedTask,
    /// Retries the task selected in the task dashboard.
    RetrySelectedTask,
    /// Approves the NeedsReview task selected in the dashboard.
    ApproveSelectedTask,
    /// Rejects the NeedsReview task selected in the dashboard.
    RejectSelectedTask,
    /// Attaches a live agent pane to the selected task's execution.
    OpenSelectedTaskAgent,
    // --- Phase 3F: global controls + auditability (3f.md §28–§34) ---
    /// STOP ALL — stops agents, active workflows and pending execution.
    StopAll,
    /// PAUSE ALL — blocks new work from starting, preserves state.
    PauseAll,
    /// Resume from PAUSE ALL.
    ResumeAll,
    /// Toggles the "NEEDS YOU" right panel (approval center, §31).
    ToggleApprovalCenter,
    /// Opens the workflow timeline overlay (audit trail, §28–§30).
    Timeline,
    /// Opens the workflow state summary overlay (§34).
    WorkflowSummary,
}

/// A key chord: optional modifiers + a printable key name (e.g. "d", "w",
/// "F2", "Tab", "ArrowRight"). Kept platform-neutral; the desktop maps its
/// winit key events onto these names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyChord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: String,
}

impl Command {
    /// Palette/UI label (Phase 2C §37). Parameterized variants use a static
    /// label — the palette runs the action against the focused target.
    pub fn to_label(&self) -> &'static str {
        match self {
            Command::SplitHorizontal => "Split Horizontal",
            Command::SplitVertical => "Split Vertical",
            Command::ClosePane => "Close Pane",
            Command::FocusNext => "Focus Next Pane",
            Command::FocusPrevious => "Focus Previous Pane",
            Command::ResizePaneLeft => "Resize Pane Left",
            Command::ResizePaneRight => "Resize Pane Right",
            Command::ResizePaneUp => "Resize Pane Up",
            Command::ResizePaneDown => "Resize Pane Down",
            Command::ZoomPane => "Zoom Pane",
            Command::NewTab => "New Tab",
            Command::CloseTab => "Close Tab",
            Command::NextTab => "Next Tab",
            Command::PreviousTab => "Previous Tab",
            Command::NewWorkspace => "New Workspace",
            Command::SwitchWorkspace(_) => "Switch Workspace",
            Command::CloseWorkspace => "Close Workspace",
            Command::ShowAgents => "Show Agents",
            Command::ShowAgentsNeedingAttention => "Show Agents Needing Attention",
            Command::ShowFailedAgents => "Show Failed Agents",
            Command::ShowCompletedAgents => "Show Completed Agents",
            Command::FocusNextAgent => "Focus Next Agent",
            Command::FocusPreviousAgent => "Focus Previous Agent",
            Command::FocusAgent(_) => "Focus Agent",
            Command::ToggleAgentWorkView => "Toggle Agent Work View",
            Command::ReviewAgentChanges => "Review Agent Changes",
            Command::OpenAgentLogs => "Open Agent Logs",
            Command::StopAgent => "Stop Agent",
            Command::RestartAgent => "Restart Agent",
            Command::ResumeAgent => "Resume Agent",
            Command::Approve => "Approve",
            Command::Deny => "Deny",
            Command::ToggleQuietMode => "Toggle Quiet Mode",
            Command::ToggleCommandPalette => "Command Palette",
            Command::RunTasks => "Run All Tasks",
            Command::ToggleTasks => "Show Tasks",
            Command::CreateTask => "Create Task",
            Command::ShowBlockedTasks => "Show Blocked Tasks",
            Command::ShowTasksNeedingReview => "Show Tasks Needing Review",
            Command::OpenTask => "Open Task",
            Command::CancelSelectedTask => "Cancel Selected Task",
            Command::RetrySelectedTask => "Retry Selected Task",
            Command::ApproveSelectedTask => "Approve Selected Task",
            Command::RejectSelectedTask => "Reject Selected Task",
            Command::OpenSelectedTaskAgent => "Open Selected Task Agent",
            Command::StopAll => "STOP ALL — stop agents and workflows",
            Command::PauseAll => "PAUSE ALL — block new work",
            Command::ResumeAll => "Resume ALL — unblock new work",
            Command::ToggleApprovalCenter => "Approval Center (NEEDS YOU)",
            Command::Timeline => "Show Workflow Timeline",
            Command::WorkflowSummary => "Show Workflow Summary",
        }
    }
}

impl KeyChord {
    pub fn plain(key: &str) -> Self {
        Self {
            ctrl: false,
            alt: false,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl_shift(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: false,
            shift: true,
            key: key.to_string(),
        }
    }
    pub fn alt(key: &str) -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl_alt(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: false,
            key: key.to_string(),
        }
    }
    pub fn ctrl_alt_shift(key: &str) -> Self {
        Self {
            ctrl: true,
            alt: true,
            shift: true,
            key: key.to_string(),
        }
    }
}

/// Default Phase 1 bindings (macOS-style: Cmd = ctrl here since winit on
/// macOS reports Cmd via `super_key`; the desktop adapts).
pub fn default_bindings() -> Vec<(KeyChord, Command)> {
    vec![
        (KeyChord::ctrl("d"), Command::SplitHorizontal),
        (KeyChord::ctrl_shift("d"), Command::SplitVertical),
        (KeyChord::ctrl("w"), Command::ClosePane),
        (KeyChord::ctrl("]"), Command::FocusNext),
        (KeyChord::ctrl("["), Command::FocusPrevious),
        (KeyChord::ctrl("t"), Command::NewTab),
        (KeyChord::ctrl_shift("t"), Command::CloseTab),
        (KeyChord::ctrl("Tab"), Command::NextTab),
        (KeyChord::ctrl_shift("Tab"), Command::PreviousTab),
        (KeyChord::ctrl("n"), Command::NewWorkspace),
        (KeyChord::ctrl("z"), Command::ZoomPane),
        (KeyChord::alt("ArrowLeft"), Command::ResizePaneLeft),
        (KeyChord::alt("ArrowRight"), Command::ResizePaneRight),
        (KeyChord::alt("ArrowUp"), Command::ResizePaneUp),
        (KeyChord::alt("ArrowDown"), Command::ResizePaneDown),
        // --- Phase 2C: agent keyboard actions (§36) ---
        (KeyChord::ctrl_alt("a"), Command::FocusNextAgent),
        (KeyChord::ctrl_alt("b"), Command::FocusPreviousAgent),
        (KeyChord::ctrl_alt("v"), Command::ToggleAgentWorkView),
        (KeyChord::ctrl_alt("y"), Command::Approve),
        (KeyChord::ctrl_alt("n"), Command::Deny),
        (KeyChord::ctrl_alt("s"), Command::StopAgent),
        (KeyChord::ctrl_alt("r"), Command::RestartAgent),
        (KeyChord::ctrl_alt("q"), Command::ToggleQuietMode),
        (KeyChord::ctrl("k"), Command::ToggleCommandPalette),
        // --- Phase 3A: task dashboard + workflow actions (§43) ---
        (KeyChord::ctrl_alt("t"), Command::ToggleTasks),
        (KeyChord::ctrl_alt("Enter"), Command::RunTasks),
        (KeyChord::ctrl_alt("c"), Command::CancelSelectedTask),
        (KeyChord::ctrl_alt("x"), Command::RetrySelectedTask),
        (KeyChord::ctrl_alt("p"), Command::OpenSelectedTaskAgent),
        // --- Phase 3F: global controls (§32–§33) + auditability (§28–§34).
        // ctrl+alt+shift to stay clear of the Phase 2C/3A chords above. ---
        (KeyChord::ctrl_alt_shift("s"), Command::StopAll),
        (KeyChord::ctrl_alt_shift("p"), Command::PauseAll),
        (KeyChord::ctrl_alt_shift("r"), Command::ResumeAll),
        (KeyChord::ctrl_alt_shift("c"), Command::ToggleApprovalCenter),
        (KeyChord::ctrl_alt_shift("t"), Command::Timeline),
        (KeyChord::ctrl_alt_shift("w"), Command::WorkflowSummary),
    ]
}

/// Maps chords to commands. Later phases add palette entries and
/// per-user rebinding here.
#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    bindings: Vec<(KeyChord, Command)>,
}

impl CommandRegistry {
    pub fn with_defaults() -> Self {
        Self {
            bindings: default_bindings(),
        }
    }

    pub fn bind(&mut self, chord: KeyChord, cmd: Command) {
        self.bindings.retain(|(c, _)| c != &chord);
        self.bindings.push((chord, cmd));
    }

    pub fn lookup(&self, chord: &KeyChord) -> Option<&Command> {
        self.bindings
            .iter()
            .find(|(c, _)| c == chord)
            .map(|(_, cmd)| cmd)
    }

    pub fn all_commands(&self) -> impl Iterator<Item = &Command> {
        self.bindings.iter().map(|(_, cmd)| cmd)
    }

    /// Every palette-offerable command (Phase 2C §37): bound commands plus
    /// the remaining parameterless commands, so "Show Agents", "Review
    /// Agent Changes", … are reachable even without a default key binding.
    /// Parameterized commands run against the focused target — the desktop
    /// treats the empty-pid `FocusAgent` as "focus the first agent pane".
    pub fn palette(&self) -> Vec<Command> {
        let mut out: Vec<Command> = self.bindings.iter().map(|(_, cmd)| cmd.clone()).collect();
        const EXTRA: &[Command] = &[
            Command::ShowAgents,
            Command::ShowAgentsNeedingAttention,
            Command::ShowFailedAgents,
            Command::ShowCompletedAgents,
            Command::FocusAgent(String::new()),
            Command::ReviewAgentChanges,
            Command::OpenAgentLogs,
            Command::ResumeAgent,
            Command::Approve,
            Command::Deny,
            Command::ToggleQuietMode,
            Command::ToggleCommandPalette,
            // Phase 3A task dashboard + workflow actions (§43, §55).
            Command::RunTasks,
            Command::ToggleTasks,
            Command::CreateTask,
            Command::ShowBlockedTasks,
            Command::ShowTasksNeedingReview,
            Command::OpenTask,
            Command::CancelSelectedTask,
            Command::RetrySelectedTask,
            Command::ApproveSelectedTask,
            Command::RejectSelectedTask,
            Command::OpenSelectedTaskAgent,
        ];
        for cmd in EXTRA {
            if !out.contains(cmd) {
                out.push(cmd.clone());
            }
        }
        out
    }
}

/// Helper for pane-split targets resolved by the desktop/CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitTarget {
    pub pane_id: PaneId,
    pub direction: SplitDirection,
}
