//! Pure domain model for the workspace engine (Phase 1 & 2A).
//!
//! These types are fully serializable and hold **no live resources** — a
//! `Workspace` is layout + metadata only. Live `Session`s and
//! `TerminalState`s live in the engine (see [`crate::engine`]) keyed by
//! [`ExecutionId`].

pub use crate::pane_tree::PaneNode;
use serde::{Deserialize, Serialize};
use terminal_session::execution::{ExecutionId, ExecutionKind};
use uuid::Uuid;

pub type WorkspaceId = String;
pub type TabId = String;
pub type PaneId = String;
// Backward compatibility alias
pub type SessionId = String;

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Split direction inside a pane tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirection {
    /// Children stacked left/right (columns).
    Horizontal,
    /// Children stacked top/bottom (rows).
    Vertical,
}

/// A leaf pane. References an execution by id; the engine owns the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub execution_kind: ExecutionKind,
    pub execution_id: ExecutionId,
    pub title: String,
    pub cwd: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Pane {
    pub fn new(
        execution_kind: ExecutionKind,
        execution_id: ExecutionId,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            id: new_id(),
            execution_kind,
            execution_id,
            title: String::new(),
            cwd: cwd.into(),
            metadata: serde_json::Value::Null,
        }
    }

    /// Backward compatibility constructor for Terminal sessions.
    pub fn new_terminal(session_id: SessionId, cwd: impl Into<String>) -> Self {
        Self::new(ExecutionKind::Terminal, ExecutionId(session_id), cwd)
    }
}

/// A tab: workspace + a pane tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub id: TabId,
    pub workspace_id: WorkspaceId,
    pub root: PaneNode,
    pub title: String,
    pub active_pane: Option<PaneId>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Tab {
    pub fn new(workspace_id: &str, root: PaneNode) -> Self {
        Self {
            id: new_id(),
            workspace_id: workspace_id.to_string(),
            root,
            title: String::new(),
            active_pane: None,
            metadata: serde_json::Value::Null,
        }
    }
}

/// A workspace: project + tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    #[serde(default)]
    pub project_root: String,
    pub tabs: Vec<Tab>,
    pub active_tab: Option<TabId>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl Workspace {
    pub fn new(name: impl Into<String>, project_root: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            project_root: project_root.into(),
            tabs: Vec::new(),
            active_tab: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        let id = self.active_tab.as_ref()?;
        self.tabs.iter().find(|t| &t.id == id)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let id = self.active_tab.clone()?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }
}

/// The serialized on-disk format (versioned for migrations).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    pub workspaces: Vec<Workspace>,
    #[serde(default)]
    pub active_workspace: Option<WorkspaceId>,
    /// Phase 3A §52: versioned orchestration state (bounded, no secrets).
    #[serde(default)]
    pub tasks: Option<terminal_session::orchestration::PersistedSchedulerState>,
}
