//! Execution Identity and Metadata (Phase 2A)
//!
//! This module defines the provider-neutral abstraction that allows a Pane
//! to host different kinds of executable sessions (Terminal, Agent, etc.)
//! without the workspace engine knowing the implementation details.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// A stable, unique identifier for any execution session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<String> for ExecutionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// The kind of execution hosted in a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionKind {
    Terminal,
    Agent,
    // Future: Remote, Container, Log, Preview
}

/// Coarse-grained lifecycle state for any execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Created,
    Running,
    Stopped,
    Failed,
    Exited(i32),
}

/// Lightweight metadata about an execution session.
/// Does NOT contain live state, secrets, or large buffers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionMetadata {
    pub id: ExecutionId,
    pub kind: ExecutionKind,
    pub title: String,
    pub cwd: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub state: ExecutionState,
    #[serde(default)]
    pub agent_definition_id: Option<String>, // Only populated if kind == Agent
}

impl ExecutionMetadata {
    pub fn new(kind: ExecutionKind, cwd: impl Into<String>) -> Self {
        Self {
            id: ExecutionId::new(),
            kind,
            title: String::new(),
            cwd: cwd.into(),
            created_at: chrono::Utc::now(),
            state: ExecutionState::Created,
            agent_definition_id: None,
        }
    }
}

/// Capability markers. Not every execution type implements every capability.
/// These are used by the UI/orchestration to discover what an execution supports.
pub trait CanInput {}
pub trait CanResize {}
pub trait CanObserve {}
pub trait CanStop {}

/// Unified event type for the application event bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApplicationEvent {
    WorkspaceChanged,
    PaneCreated {
        pane_id: String,
        execution_id: ExecutionId,
    },
    PaneClosed {
        pane_id: String,
    },
    SessionExited {
        execution_id: ExecutionId,
        code: Option<i32>,
    },
    AgentEvent {
        execution_id: ExecutionId,
        event: AgentEvent,
    },
}

/// Semantic events specific to agent sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Started,
    StateChanged {
        new_state: AgentState,
        #[serde(default)]
        provenance: Option<StateProvenance>,
    },
    Output {
        text: String,
    },
    Error {
        message: String,
    },
    PermissionRequested {
        action: String,
        context: String,
    },
    Completed,
    Exited {
        code: Option<i32>,
    },
    UsageUpdated {
        tokens: u64,
    },
}

/// Fine-grained lifecycle states for agent sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Created,
    Starting,
    Working,
    Waiting,
    NeedsApproval,
    Blocked,
    Completed,
    Failed,
    Crashed,
    Stopped,
    Disconnected,
}

/// Where an agent state observation came from (Phase 2B.1 §13–14).
///
/// Every state transition is tagged with its source so internal code can
/// distinguish authoritative transitions from heuristic guesses. This is
/// NOT shown in the primary UI yet — it becomes load-bearing for reliable
/// orchestration in Phase 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    /// A structured protocol/event from the agent itself (none of the
    /// current adapters provide this — `structured_events` is false
    /// everywhere until verified).
    Structured,
    /// A hook the agent defines for the terminal (not yet implemented).
    EventHook,
    /// The terminal heuristic in the adapter's `detect_activity`.
    TerminalHeuristic,
    /// Process lifecycle: spawn, user stop, exit code classification.
    ProcessLifecycle,
}

/// Confidence in a state observation (Phase 2B.1 §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateConfidence {
    Low,
    Medium,
    High,
}

/// A state observation plus its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateProvenance {
    pub source: StateSource,
    pub confidence: StateConfidence,
}

impl StateProvenance {
    /// Process lifecycle events (spawn, stop, exit-code classification) are
    /// authoritative.
    pub const PROCESS: Self = Self {
        source: StateSource::ProcessLifecycle,
        confidence: StateConfidence::High,
    };
    /// Adapter terminal heuristics refine the status line only.
    pub const HEURISTIC: Self = Self {
        source: StateSource::TerminalHeuristic,
        confidence: StateConfidence::Medium,
    };
    /// Detected approval prompts on the terminal stream (pattern-based).
    pub const HEURISTIC_APPROVAL: Self = Self {
        source: StateSource::TerminalHeuristic,
        confidence: StateConfidence::Low,
    };
}

/// Lightweight activity model (Phase 2B §26) shown in the UI. Keeps raw
/// terminal output available — this never replaces the stream, it only
/// refines the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivity {
    Starting,
    Working,
    Waiting,
    NeedsApproval,
    Completed,
    Failed,
    Stopped,
}

impl From<AgentState> for AgentActivity {
    fn from(s: AgentState) -> Self {
        match s {
            AgentState::Created | AgentState::Starting => AgentActivity::Starting,
            AgentState::Working => AgentActivity::Working,
            AgentState::Waiting | AgentState::Blocked | AgentState::Disconnected => {
                AgentActivity::Waiting
            }
            AgentState::NeedsApproval => AgentActivity::NeedsApproval,
            AgentState::Completed => AgentActivity::Completed,
            AgentState::Failed | AgentState::Crashed => AgentActivity::Failed,
            AgentState::Stopped => AgentActivity::Stopped,
        }
    }
}

impl std::fmt::Display for AgentActivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AgentActivity::Starting => "Starting",
            AgentActivity::Working => "Working",
            AgentActivity::Waiting => "Waiting",
            AgentActivity::NeedsApproval => "Needs approval",
            AgentActivity::Completed => "Completed",
            AgentActivity::Failed => "Failed",
            AgentActivity::Stopped => "Stopped",
        };
        write!(f, "{s}")
    }
}
