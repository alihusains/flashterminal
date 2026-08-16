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
    /// Phase 3A: orchestration lifecycle events (§21 — single event bus).
    TaskEvent {
        event: crate::orchestration::TaskEvent,
    },
    /// Phase 3B: planning lifecycle events (3b.md §24 — published on the
    /// same ApplicationEvent bus).
    PlannerEvent {
        event: crate::planning::PlannerEvent,
    },
    // --- Phase 3D (3d.md §28): collaboration events. Metadata only —
    // large payloads never ride the event bus (§27). ---
    /// An artifact was registered in the artifact store.
    ArtifactCreated {
        artifact_id: String,
        task_id: Option<String>,
        kind: String,
        description: String,
    },
    /// A review finding was recorded.
    ReviewFindingCreated {
        finding: crate::collaboration::ReviewFinding,
    },
    /// Deterministic synthesis started.
    SynthesisStarted {
        synthesis_id: String,
        task_ids: Vec<String>,
    },
    /// Synthesis completed (metadata + result, never private reasoning).
    /// Boxed: `SynthesisResult` is the largest Phase 3D payload and the
    /// event enum must stay small (clippy `large_enum_variant`).
    SynthesisCompleted {
        result: Box<crate::collaboration::SynthesisResult>,
    },
    /// A task consumed an artifact from a dependency.
    ArtifactConsumed {
        task_id: String,
        artifact_id: String,
    },
    /// A workflow needs human replanning (3d.md §44) — a structured
    /// signal, never an autonomous replan.
    WorkflowNeedsReplan {
        workflow_id: String,
        cause: String,
        detail: String,
    },
    // --- Phase 3E (3e.md §39): adaptive orchestration events. Metadata
    // only — large payloads never ride the event bus. ---
    /// A replanning request was raised (signal recorded).
    ReplanRequested {
        signal_id: String,
        workflow_id: String,
        trigger: String,
        severity: String,
    },
    /// The planner proposed a revised plan (awaits human approval).
    ReplanProposed {
        replan_id: String,
        workflow_id: String,
        version: u32,
        reason: String,
    },
    /// A human edited the proposed replan (revalidation follows).
    ReplanEdited {
        replan_id: String,
        workflow_id: String,
        version: u32,
    },
    /// A replan was approved and applied to the graph.
    ReplanApproved {
        replan_id: String,
        workflow_id: String,
        version: u32,
    },
    /// A replan was rejected — the original workflow remains intact.
    ReplanRejected {
        replan_id: String,
        workflow_id: String,
        reason: String,
    },
    /// A plan version was superseded by a newer one (v1 → v2).
    PlanSuperseded {
        superseded_version: u32,
        new_version: u32,
    },
    /// A completed task was explicitly invalidated (human-approved).
    TaskInvalidated {
        task_id: String,
        reason: String,
    },
    /// An artifact was invalidated (old record preserved for lineage).
    ArtifactInvalidated {
        artifact_id: String,
        reason: String,
    },
    /// Projected workflow cost approaches/exceeds the budget.
    BudgetRisk {
        workflow_id: String,
        spent_cents: u64,
        budget_cents: Option<u64>,
        estimated_remaining_cents: Option<u64>,
    },
    /// Automation could not safely continue — human attention required.
    HumanEscalation {
        escalation_id: String,
        workflow_id: String,
        reason: String,
    },
    // --- Phase 3F (§32–§33): global human controls. Metadata only. ---
    /// STOP ALL was executed (agents stopped, pending execution cancelled,
    /// state preserved).
    WorkflowStopped {
        workflow_id: String,
    },
    /// PAUSE ALL / resume — new work blocked / unblocked.
    WorkflowPaused {
        paused: bool,
        workflow_id: String,
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
    /// A normalized activity observation (Phase 2C §4–§5). Coalesced at
    /// the pump so high-frequency output never floods the UI (§23).
    Activity {
        kind: crate::work::ActivityKind,
        source: crate::work::ActivitySource,
        confidence: u8,
        detail: String,
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
