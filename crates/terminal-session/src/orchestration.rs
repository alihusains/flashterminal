//! Phase 3A — deterministic multi-agent task orchestration (3a.md §1–§33).
//!
//! Domain-only module: no PTY, no engine, no UI. Everything here is pure
//! data + a deterministic decision engine, so the same graph and initial
//! state always produce the same schedule under the same policy (§10).
//!
//! Layering (3a.md §54 — the scheduler never spawns processes):
//!
//! ```text
//! Task
//!  ↓
//! TaskScheduler (decision engine → SchedulerCommand outbox)
//!  ↓
//! engine (Multiplexer) executes commands via AgentRuntime
//!  ↓
//! AgentSession (Pane when the user attaches one)
//! ```
//!
//! Explicit non-goals for 3A (§44–§46, §56): LLM planning, automatic agent
//! selection, agent-to-agent messaging.

use crate::execution::{AgentState, ExecutionId};
use crate::work::{now_ms, AgentWork, ErrorKind, WorkError};
use crate::worktrees::{DiffSummary, DirtyPolicy, IsolationMode, RetryWorktreePolicy};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Unique identifier of a task in an orchestration plan.
pub type TaskId = String;

pub fn new_task_id() -> TaskId {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// §5 TaskStatus + explicit transitions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Blocked,
    Waiting,
    NeedsReview,
    Completed,
    Failed,
    Cancelled,
    Skipped,
    /// Restore-only state (3a.md §24): the task was Running/Waiting when the
    /// application went offline. It never resumes silently.
    Interrupted,
}

impl TaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Pending => "Pending",
            TaskStatus::Ready => "Ready",
            TaskStatus::Running => "Running",
            TaskStatus::Blocked => "Blocked",
            TaskStatus::Waiting => "Waiting",
            TaskStatus::NeedsReview => "Needs review",
            TaskStatus::Completed => "Completed",
            TaskStatus::Failed => "Failed",
            TaskStatus::Cancelled => "Cancelled",
            TaskStatus::Skipped => "Skipped",
            TaskStatus::Interrupted => "Interrupted",
        }
    }

    /// User-facing status vocabulary (3a.md §38) — graph-theory terms stay
    /// internal; users see "Working", "Waiting", "Needs your attention"…
    pub fn user_label(self) -> &'static str {
        match self {
            TaskStatus::Pending | TaskStatus::Ready => "Waiting",
            TaskStatus::Running => "Working",
            TaskStatus::Blocked => "Blocked",
            TaskStatus::Waiting => "Needs your attention",
            TaskStatus::NeedsReview => "Ready for review",
            TaskStatus::Completed => "Completed",
            TaskStatus::Failed => "Failed",
            TaskStatus::Cancelled => "Cancelled",
            TaskStatus::Skipped => "Skipped",
            TaskStatus::Interrupted => "Interrupted",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Skipped
        )
    }

    pub fn is_running(self) -> bool {
        self == TaskStatus::Running
    }

    /// The only sanctioned way to move a task between states (§5). Every
    /// scheduler transition goes through this check; invalid moves are
    /// typed errors, never silent no-ops.
    pub fn can_transition(self, to: TaskStatus) -> Result<(), TaskTransitionError> {
        use TaskStatus::*;
        let ok = match (self, to) {
            (Pending, Ready | Blocked | Cancelled | Skipped) => true,
            (Ready, Running | Cancelled | Skipped | Blocked) => true,
            (Running, NeedsReview | Completed | Failed | Cancelled | Waiting | Blocked) => true,
            // §25/§26 retry arcs: a failed task re-arms (auto-retry queues
            // it, `task.retry` does the same for manual retries).
            (Failed, Ready) => true,
            (Waiting, Running | Completed | Failed | Cancelled) => true,
            (NeedsReview, Completed | Failed | Cancelled) => true,
            (Blocked, Ready | Cancelled | Skipped) => true,
            (Interrupted, Ready | Cancelled) => true,
            // Restore-only arcs (§24): the app was offline mid-run.
            (Running | Waiting, Interrupted) => true,
            (Completed | Failed | Cancelled | Skipped, _) => false,
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(TaskTransitionError::forbidden(self, to))
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Typed transition errors (§5: no arbitrary state mutations).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskTransitionError {
    Invalid { from: TaskStatus, to: TaskStatus },
}

impl TaskTransitionError {
    fn forbidden(from: TaskStatus, to: TaskStatus) -> Self {
        Self::Invalid { from, to }
    }
}

impl std::fmt::Display for TaskTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { from, to } => {
                write!(f, "invalid task transition {from:?} → {to:?}")
            }
        }
    }
}

impl std::error::Error for TaskTransitionError {}

// ---------------------------------------------------------------------------
// §17 Artifact + §16 TaskResult + §26 failure classes
// ---------------------------------------------------------------------------

/// A produced/declared unit of task output (3a.md §17). Extensible —
/// `metadata` carries free-form key/value extras.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactType {
    CodeChange,
    File,
    Diff,
    TestReport,
    Log,
    Document,
    Binary,
    Url,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub id: String,
    pub kind: ArtifactType,
    /// Absent for Url artifacts.
    pub path: Option<String>,
    pub description: String,
    pub created_by_task: Option<TaskId>,
    #[serde(default)]
    pub metadata: Vec<(String, String)>,
    // --- Phase 3D (3d.md §4): artifact identity + provenance. Every field
    // is engine-stamped (never agent-controlled) and secret-free (§35). ---
    /// The agent definition that produced this artifact.
    #[serde(default)]
    pub created_by_agent: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Producer worktree path (for repository artifacts).
    #[serde(default)]
    pub worktree: Option<String>,
    /// Revision (commit) the artifact was produced at.
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
}

pub fn new_artifact_id() -> String {
    format!("artifact:{}", uuid::Uuid::new_v4())
}

/// Lawful task failure causes (§26) — the retry policy decides which of
/// these may be retried; classification is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    TransientProviderFailure,
    NetworkFailure,
    AgentCrash,
    AuthenticationFailure,
    TaskFailure,
    PermissionFailure,
    Unknown,
}

impl FailureClass {
    /// Default retryable set (§25/§26): transient infrastructure + crashes.
    /// Authentication/Permission/Task failures need a human — never retried
    /// by default.
    pub fn retryable_default(self) -> bool {
        matches!(
            self,
            FailureClass::TransientProviderFailure | FailureClass::NetworkFailure
        ) || self == FailureClass::AgentCrash
    }

    /// Deterministic classification from runtime observations + work errors.
    pub fn classify(
        state: AgentState,
        exit_code: Option<i32>,
        work_errors: &[WorkError],
    ) -> FailureClass {
        // Work errors are heuristic observations; exit codes are
        // authoritative — but a classified error beats nothing.
        if let Some(e) = work_errors.last() {
            match e.kind {
                ErrorKind::PermissionFailure => return FailureClass::PermissionFailure,
                ErrorKind::NetworkFailure => return FailureClass::NetworkFailure,
                ErrorKind::ProviderFailure => return FailureClass::TransientProviderFailure,
                ErrorKind::AgentFailure | ErrorKind::CommandFailure => {}
            }
        }
        match state {
            AgentState::Crashed => FailureClass::AgentCrash,
            AgentState::Blocked => FailureClass::PermissionFailure,
            _ => match exit_code {
                // Deterministic exit-code contract (documented for the fake
                // agent; real adapters classify via work errors instead).
                Some(2) => FailureClass::AuthenticationFailure,
                Some(3) => FailureClass::TransientProviderFailure,
                Some(139) | Some(134) => FailureClass::AgentCrash,
                _ => FailureClass::TaskFailure,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FailureClass::TransientProviderFailure => "transient provider failure",
            FailureClass::NetworkFailure => "network failure",
            FailureClass::AgentCrash => "agent crash",
            FailureClass::AuthenticationFailure => "authentication failure",
            FailureClass::TaskFailure => "task failure",
            FailureClass::PermissionFailure => "permission failure",
            FailureClass::Unknown => "unknown",
        }
    }
}

/// Retry policy (§25–§26). `max_retries` = retries *beyond* the first
/// attempt; only `retry_classes` failures may be retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_classes: Vec<FailureClass>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 1,
            retry_classes: vec![
                FailureClass::TransientProviderFailure,
                FailureClass::NetworkFailure,
                FailureClass::AgentCrash,
            ],
        }
    }
}

impl RetryPolicy {
    /// `attempts_so_far` = attempts already made including the one that
    /// just failed. `max_retries` = number of retries allowed beyond the
    /// first attempt (3a.md §25/§33).
    pub fn may_retry(&self, class: FailureClass, attempts_so_far: u32) -> bool {
        attempts_so_far <= self.max_retries && self.retry_classes.contains(&class)
    }
}

/// Structured task error (§8: typed errors, never bare strings where a type
/// fits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskErrorKind {
    AgentSpawnFailed,
    AgentFailed,
    AgentCrashed,
    ProviderFailure,
    NetworkFailure,
    AuthenticationFailure,
    PermissionFailure,
    BudgetExceeded,
    /// Phase 3D §9: a declared input artifact is unavailable — the task is
    /// Blocked until a human resolves it (retry/replan).
    ArtifactMissing,
    MissingAgentDefinition,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskError {
    pub kind: TaskErrorKind,
    pub class: FailureClass,
    pub message: String,
}

impl TaskError {
    pub fn new(kind: TaskErrorKind, class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            kind,
            class,
            message: message.into(),
        }
    }

    pub fn budget(message: impl Into<String>) -> Self {
        Self::new(
            TaskErrorKind::BudgetExceeded,
            FailureClass::TaskFailure,
            message,
        )
    }
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.class.label(), self.message)
    }
}

/// The deterministic record of one task execution (3a.md §16). No
/// LLM-generated summary — everything here is observed/bounded data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResult {
    pub status: TaskStatus,
    pub summary: String,
    pub artifacts: Vec<Artifact>,
    pub files_changed: Vec<String>,
    pub commands: Vec<String>,
    /// Cumulative wall time across attempts (ms).
    pub duration_ms: u64,
    pub error: Option<TaskError>,
    pub agent_execution_id: Option<ExecutionId>,
    pub attempt_count: u32,
    pub estimated_cost_cents: Option<u64>,
    // --- Phase 3C (3c.md §17): worktree provenance of this execution ---
    /// Base commit the isolated worktree was created from (§8).
    #[serde(default)]
    pub base_revision: Option<String>,
    /// Worktree HEAD when the agent finished (§17).
    #[serde(default)]
    pub result_revision: Option<String>,
    /// Feature branch the worktree ran on.
    #[serde(default)]
    pub branch: Option<String>,
    /// Worktree path the agent ran in.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Deterministic diff vs the base revision (§18 — never the agent's
    /// own summary). Boxed: the summary is large and this value rides in
    /// `TaskEvent`/`ApplicationEvent`, which must stay small by value
    /// (clippy::large_enum_variant).
    #[serde(default)]
    pub diff_summary: Option<Box<DiffSummary>>,
    // --- Phase 3D (3d.md §13): structured results. Deterministic where
    // possible; never requires an LLM to produce them. ---
    /// Bounded key/value metrics (e.g. tests passed, duration).
    #[serde(default)]
    pub metrics: Vec<(String, String)>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub recommendations: Vec<String>,
}

impl TaskResult {
    pub fn deterministic_summary(files: usize, commands: usize, attempt: u32) -> String {
        format!(
            "completed in {attempt} attempt(s): {files} file(s) changed, {commands} command(s) run"
        )
    }
}

impl Default for TaskResult {
    fn default() -> Self {
        Self {
            status: TaskStatus::Pending,
            summary: String::new(),
            artifacts: Vec::new(),
            files_changed: Vec::new(),
            commands: Vec::new(),
            duration_ms: 0,
            error: None,
            agent_execution_id: None,
            attempt_count: 0,
            estimated_cost_cents: None,
            base_revision: None,
            result_revision: None,
            branch: None,
            worktree: None,
            diff_summary: None,
            metrics: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            recommendations: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// §3 Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    /// Declared dependencies (directed edges, §6). Only `Completed`
    /// dependencies release a task (§9).
    pub dependencies: Vec<TaskId>,
    /// Explicit agent assignment (3a.md §13, §45) — definition id,
    /// never auto-selected.
    pub assigned_agent: String,
    /// Set once the scheduler has spawned the agent session.
    pub agent_execution_id: Option<ExecutionId>,
    pub workspace_id: String,
    /// Artifact ids consumed from dependencies (§18).
    pub input_artifacts: Vec<String>,
    /// Artifacts this task declares as its outputs (§17).
    pub output_artifacts: Vec<Artifact>,
    /// Deterministic launch overrides (fixture scenarios etc.; the adapter
    /// still builds the vendor instruction, 3a.md §20).
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub review_required: bool,
    // --- Phase 3C (3c.md §2, §4, §10, §28, §45) ---
    /// Execution isolation mode. Coding tasks default to GitWorktree.
    #[serde(default)]
    pub isolation: IsolationMode,
    /// Explicit opt-in for shared-workspace execution (§28) — never the
    /// default for coding tasks.
    #[serde(default)]
    pub requires_shared_workspace: bool,
    /// The worktree this task currently owns (at most one active, §10).
    #[serde(default)]
    pub worktree_id: Option<String>,
    /// Slug used for the deterministic branch name (§7).
    #[serde(default)]
    pub slug: String,
    pub attempt_count: u32,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub completed_at_ms: Option<u64>,
    pub duration_ms: u64,
    pub error: Option<TaskError>,
    pub result: Option<TaskResult>,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        // Wall-clock truth isn't deterministic; compare the schedule, not
        // the clock.
        self.id == other.id
            && self.title == other.title
            && self.status == other.status
            && self.dependencies == other.dependencies
            && self.assigned_agent == other.assigned_agent
            && self.workspace_id == other.workspace_id
            && self.input_artifacts == other.input_artifacts
            && self.output_artifacts == other.output_artifacts
            && self.review_required == other.review_required
            && self.attempt_count == other.attempt_count
            && self.error == other.error
            && self.result == other.result
    }
}

impl Task {
    pub fn new(
        title: impl Into<String>,
        description: impl Into<String>,
        assigned_agent: impl Into<String>,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            id: new_task_id(),
            title: title.into(),
            description: description.into(),
            status: TaskStatus::Pending,
            dependencies: Vec::new(),
            assigned_agent: assigned_agent.into(),
            agent_execution_id: None,
            workspace_id: workspace_id.into(),
            input_artifacts: Vec::new(),
            output_artifacts: Vec::new(),
            arguments: Vec::new(),
            environment: Vec::new(),
            review_required: false,
            isolation: IsolationMode::default(),
            requires_shared_workspace: false,
            worktree_id: None,
            slug: String::new(),
            attempt_count: 0,
            created_at_ms: now_ms(),
            started_at_ms: None,
            completed_at_ms: None,
            duration_ms: 0,
            error: None,
            result: None,
        }
    }

    pub fn transition(&mut self, to: TaskStatus) -> Result<(), TaskTransitionError> {
        TaskStatus::can_transition(self.status, to)?;
        self.status = to;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// §6–§8 TaskGraph
// ---------------------------------------------------------------------------

/// Typed graph validation errors (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskGraphError {
    DuplicateTaskId(TaskId),
    UnknownTask(TaskId),
    SelfDependency(TaskId),
    DuplicateDependency {
        task: TaskId,
        dependency: TaskId,
    },
    MissingDependency {
        task: TaskId,
        dependency: TaskId,
    },
    /// A dependency edge that would close a cycle (path = task → … → task).
    Cycle {
        path: Vec<TaskId>,
    },
    UnknownArtifact {
        task: TaskId,
        artifact: String,
    },
    UnknownWorkspace(String),
    UnknownAgentDefinition(String),
    InvalidTransition(TaskTransitionError),
}

impl std::fmt::Display for TaskGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateTaskId(id) => write!(f, "duplicate task id `{id}`"),
            Self::UnknownTask(id) => write!(f, "unknown task `{id}`"),
            Self::SelfDependency(id) => write!(f, "task `{id}` depends on itself"),
            Self::DuplicateDependency { task, dependency } => {
                write!(f, "task `{task}` already depends on `{dependency}`")
            }
            Self::MissingDependency { task, dependency } => {
                write!(f, "task `{task}` has unknown dependency `{dependency}`")
            }
            Self::Cycle { path } => write!(f, "dependency cycle: {}", path.join(" → ")),
            Self::UnknownArtifact { task, artifact } => {
                write!(f, "task `{task}` references unknown artifact `{artifact}`")
            }
            Self::UnknownWorkspace(id) => write!(f, "unknown workspace `{id}`"),
            Self::UnknownAgentDefinition(id) => {
                write!(f, "unknown agent definition `{id}`")
            }
            Self::InvalidTransition(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TaskGraphError {}

/// Directed task graph (3a.md §6). Insertion-ordered for determinism;
/// edges are the `dependencies` lists declared on tasks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskGraph {
    tasks: Vec<Task>,
    index: HashMap<TaskId, usize>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_task(&mut self, task: Task) -> Result<(), TaskGraphError> {
        if self.index.contains_key(&task.id) {
            return Err(TaskGraphError::DuplicateTaskId(task.id));
        }
        self.index.insert(task.id.clone(), self.tasks.len());
        self.tasks.push(task);
        Ok(())
    }

    pub fn remove_task(&mut self, id: &TaskId) -> Result<(), TaskGraphError> {
        let Some(&idx) = self.index.get(id) else {
            return Err(TaskGraphError::UnknownTask(id.clone()));
        };
        self.tasks.remove(idx);
        self.index.remove(id);
        for (i, t) in self.tasks.iter_mut().enumerate() {
            *self.index.get_mut(&t.id).unwrap() = i;
            t.dependencies.retain(|d| d != id);
        }
        Ok(())
    }

    pub fn add_dependency(&mut self, task: &TaskId, dep: &TaskId) -> Result<(), TaskGraphError> {
        if task == dep {
            return Err(TaskGraphError::SelfDependency(task.clone()));
        }
        if !self.index.contains_key(dep) {
            return Err(TaskGraphError::UnknownTask(dep.clone()));
        }
        let entry = self
            .index
            .get(task)
            .copied()
            .ok_or_else(|| TaskGraphError::UnknownTask(task.clone()))?;
        if self.tasks[entry].dependencies.contains(dep) {
            return Err(TaskGraphError::DuplicateDependency {
                task: task.clone(),
                dependency: dep.clone(),
            });
        }
        self.tasks[entry].dependencies.push(dep.clone());
        if let Some(path) = self.find_cycle() {
            self.tasks[entry].dependencies.retain(|d| d != dep);
            return Err(TaskGraphError::Cycle { path });
        }
        Ok(())
    }

    pub fn remove_dependency(&mut self, task: &TaskId, dep: &TaskId) -> Result<(), TaskGraphError> {
        let entry = self
            .index
            .get(task)
            .copied()
            .ok_or_else(|| TaskGraphError::UnknownTask(task.clone()))?;
        self.tasks[entry].dependencies.retain(|d| d != dep);
        Ok(())
    }

    pub fn get_task(&self, id: &TaskId) -> Option<&Task> {
        let idx = self.index.get(id)?;
        self.tasks.get(*idx)
    }

    pub fn get_task_mut(&mut self, id: &TaskId) -> Option<&mut Task> {
        let idx = self.index.get(id)?;
        self.tasks.get_mut(*idx)
    }

    /// Insertion order — the deterministic enumeration order everywhere.
    pub fn list_tasks(&self) -> Vec<&Task> {
        self.tasks.iter().collect()
    }

    pub fn list_task_ids(&self) -> Vec<TaskId> {
        self.tasks.iter().map(|t| t.id.clone()).collect()
    }

    pub fn task_ids(&self) -> Vec<TaskId> {
        self.list_task_ids()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn dependencies_of(&self, id: &TaskId) -> Vec<TaskId> {
        self.get_task(id)
            .map(|t| t.dependencies.clone())
            .unwrap_or_default()
    }

    /// Tasks that directly depend on `id` (insertion order).
    pub fn dependents_of(&self, id: &TaskId) -> Vec<TaskId> {
        self.tasks
            .iter()
            .filter(|t| t.dependencies.contains(id))
            .map(|t| t.id.clone())
            .collect()
    }

    /// §9: Ready ⟺ every declared dependency is Completed.
    pub fn dependencies_satisfied(&self, id: &TaskId) -> bool {
        let Some(task) = self.get_task(id) else {
            return false;
        };
        task.dependencies.iter().all(|d| {
            self.get_task(d)
                .map(|t| t.status == TaskStatus::Completed)
                .unwrap_or(false)
        })
    }

    /// First dependency in a terminal non-completed state (Failed,
    /// Cancelled, Skipped) — drives §9 Blocked/Skipped policy.
    pub fn failed_dependency(&self, id: &TaskId) -> Option<TaskId> {
        let task = self.get_task(id)?;
        task.dependencies
            .iter()
            .find(|d| {
                self.get_task(d)
                    .map(|t| {
                        matches!(
                            t.status,
                            TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Skipped
                        )
                    })
                    .unwrap_or(false)
            })
            .cloned()
    }

    /// Tasks currently in `Ready` (scheduler has marked them).
    pub fn ready_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.status == TaskStatus::Ready)
            .collect()
    }

    pub fn tasks_with_status(&self, status: TaskStatus) -> Vec<&Task> {
        self.tasks.iter().filter(|t| t.status == status).collect()
    }

    /// Kahn topological order, ties broken by insertion order — the
    /// deterministic execution order for serial scheduling.
    pub fn topological_order(&self) -> Result<Vec<TaskId>, TaskGraphError> {
        let mut in_degree: HashMap<TaskId, usize> = self
            .tasks
            .iter()
            .map(|t| (t.id.clone(), t.dependencies.len()))
            .collect();
        let mut order = Vec::with_capacity(self.tasks.len());
        let mut ready: Vec<TaskId> = self
            .tasks
            .iter()
            .filter(|t| t.dependencies.is_empty())
            .map(|t| t.id.clone())
            .collect();
        while !ready.is_empty() {
            let id = ready.remove(0);
            order.push(id.clone());
            for dep in self.dependents_of(&id) {
                let d = in_degree.get_mut(&dep).unwrap();
                *d -= 1;
                if *d == 0 {
                    ready.push(dep);
                }
            }
        }
        if order.len() != self.tasks.len() {
            return Err(TaskGraphError::Cycle {
                path: self.find_cycle().unwrap_or_default(),
            });
        }
        Ok(order)
    }

    /// DFS cycle detection with the offending path (deterministic start
    /// from the first insertion-order task).
    pub fn find_cycle(&self) -> Option<Vec<TaskId>> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Color {
            White,
            Gray,
            Black,
        }

        fn visit(
            id: &TaskId,
            tasks: &[Task],
            color: &mut HashMap<TaskId, Color>,
            stack: &mut Vec<TaskId>,
        ) -> Option<Vec<TaskId>> {
            match color.get(id) {
                Some(Color::Black) => return None,
                Some(Color::Gray) => {
                    let pos = stack.iter().position(|t| t == id)?;
                    let mut path = stack[pos..].to_vec();
                    path.push(id.clone());
                    return Some(path);
                }
                _ => {}
            }
            color.insert(id.clone(), Color::Gray);
            stack.push(id.clone());
            let task = tasks.iter().find(|t| &t.id == id)?;
            for dep in &task.dependencies {
                if let Some(path) = visit(dep, tasks, color, stack) {
                    return Some(path);
                }
            }
            stack.pop();
            color.insert(id.clone(), Color::Black);
            None
        }

        let mut color: HashMap<TaskId, Color> = self
            .tasks
            .iter()
            .map(|t| (t.id.clone(), Color::White))
            .collect();
        let mut stack: Vec<TaskId> = Vec::new();
        for task in &self.tasks {
            if let Some(path) = visit(&task.id, &self.tasks, &mut color, &mut stack) {
                return Some(path);
            }
        }
        None
    }

    /// §8 structural validation — returns every issue, not just the first.
    /// Agent/workspace references need external context (registry), so the
    /// engine checks those; artifact references are checked structurally.
    pub fn validate(&self) -> Vec<TaskGraphError> {
        let mut issues = Vec::new();
        if let Some(path) = self.find_cycle() {
            issues.push(TaskGraphError::Cycle { path });
        }
        let mut artifact_sources: HashMap<String, TaskId> = HashMap::new();
        for task in &self.tasks {
            for a in &task.output_artifacts {
                artifact_sources.insert(a.id.clone(), task.id.clone());
            }
        }
        for task in &self.tasks {
            for dep in &task.dependencies {
                if dep == &task.id {
                    issues.push(TaskGraphError::SelfDependency(task.id.clone()));
                } else if !self.index.contains_key(dep) {
                    issues.push(TaskGraphError::MissingDependency {
                        task: task.id.clone(),
                        dependency: dep.clone(),
                    });
                } else {
                    let count = task.dependencies.iter().filter(|d| *d == dep).count();
                    if count > 1 {
                        issues.push(TaskGraphError::DuplicateDependency {
                            task: task.id.clone(),
                            dependency: dep.clone(),
                        });
                    }
                }
            }
            for art in &task.input_artifacts {
                let looks_like_path =
                    art.starts_with('/') || art.starts_with("./") || art.starts_with("../");
                if !artifact_sources.contains_key(art) && !looks_like_path {
                    issues.push(TaskGraphError::UnknownArtifact {
                        task: task.id.clone(),
                        artifact: art.clone(),
                    });
                }
            }
        }
        issues
    }

    // ---- context helpers for the engine (3a.md §18–§19) ----

    /// Dependency info for a task's TaskContext (titles + status + outputs).
    pub fn dependency_summaries(&self, id: &TaskId) -> Vec<TaskDependencyInfo> {
        let Some(task) = self.get_task(id) else {
            return Vec::new();
        };
        task.dependencies
            .iter()
            .filter_map(|d| {
                let dep = self.get_task(d)?;
                Some(TaskDependencyInfo {
                    task_id: dep.id.clone(),
                    title: dep.title.clone(),
                    status: dep.status.label().to_string(),
                    artifacts: dep
                        .output_artifacts
                        .iter()
                        .filter_map(|a| a.path.clone())
                        .collect(),
                })
            })
            .collect()
    }

    /// Resolves declared input artifacts + auto artifacts to paths.
    pub fn input_artifact_paths(&self, id: &TaskId) -> Vec<String> {
        let Some(task) = self.get_task(id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for art in &task.input_artifacts {
            if let Some(producer) = self.producer_of(art) {
                if let Some(t) = self.get_task(&producer) {
                    for a in &t.output_artifacts {
                        if &a.id == art {
                            if let Some(p) = &a.path {
                                out.push(p.clone());
                            }
                        }
                    }
                }
            } else {
                out.push(art.clone());
            }
        }
        out
    }

    /// Relevant files from completed dependencies (bounded §18 — no
    /// unbounded context bloat).
    pub fn relevant_files(&self, id: &TaskId) -> Vec<String> {
        let Some(task) = self.get_task(id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for d in &task.dependencies {
            if let Some(dep) = self.get_task(d) {
                if let Some(r) = &dep.result {
                    for f in &r.files_changed {
                        if !out.contains(f) {
                            out.push(f.clone());
                        }
                        if out.len() >= 64 {
                            return out;
                        }
                    }
                }
            }
        }
        out
    }

    pub fn producer_of(&self, artifact_id: &str) -> Option<TaskId> {
        self.tasks
            .iter()
            .find(|t| t.output_artifacts.iter().any(|a| a.id == artifact_id))
            .map(|t| t.id.clone())
    }

    /// True when `task` (transitively) depends on `dep` — the access grant
    /// for artifact consumption (3d.md §40: dependency grants access).
    pub fn is_dependency(&self, task: &TaskId, dep: &TaskId) -> bool {
        if task == dep {
            return false;
        }
        // Walk the dependency edges forward from `task`; reaching `dep`
        // proves the transitive relationship.
        let mut stack: Vec<TaskId> = vec![task.clone()];
        let mut seen: std::collections::HashSet<TaskId> = std::collections::HashSet::new();
        while let Some(current) = stack.pop() {
            if !seen.insert(current.clone()) {
                continue;
            }
            let Some(&idx) = self.index.get(&current) else {
                continue;
            };
            for d in &self.tasks[idx].dependencies {
                if d == dep {
                    return true;
                }
                stack.push(d.clone());
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------
// §19 TaskContext + §20 PreparedTask (adapter boundary)
// ---------------------------------------------------------------------------

/// Structured context handed from the scheduler to the adapter (3a.md §19).
/// Pure data; no engine dependencies. No LLM context summarizer in 3A.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskContext {
    pub workspace_id: String,
    pub workspace_name: String,
    pub project_root: String,
    pub task_id: TaskId,
    pub task_title: String,
    pub task_description: String,
    pub dependencies: Vec<TaskDependencyInfo>,
    pub artifact_paths: Vec<String>,
    pub relevant_files: Vec<String>,
    pub environment: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskDependencyInfo {
    pub task_id: TaskId,
    pub title: String,
    pub status: String,
    pub artifacts: Vec<String>,
}

/// What the adapter wants to execute a task (3a.md §20) — the scheduler
/// never constructs vendor-specific prompts; only the adapter does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreparedTask {
    pub instructions: String,
    pub arguments: Vec<String>,
    pub environment: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// §10–§12 scheduler policies
// ---------------------------------------------------------------------------

/// What happens to dependent tasks when a dependency fails (§9, §25).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyFailurePolicy {
    /// Fail fast: dependents move to `Blocked`.
    BlockDownstream,
    /// Dependents are marked `Skipped`.
    SkipDownstream,
}

/// Workspace-level orchestration configuration (§12, §36–§37). Hard limits:
/// the scheduler never exceeds them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPolicy {
    /// Global cap on concurrently running task agents.
    pub max_agents: usize,
    /// Cap on concurrently running tasks.
    pub max_parallel_tasks: usize,
    pub failure: DependencyFailurePolicy,
    /// New tasks require review before final completion.
    pub review_required: bool,
    pub retry: RetryPolicy,
    /// Hard cost budget in cents (0/None = unlimited). When the scheduler
    /// knows the remaining budget is exhausted it blocks further starts
    /// (§37 — never overspend automatically).
    pub max_cost_cents: Option<u64>,
    // --- Phase 3C (3c.md §10, §14, §47) ---
    /// Retry worktree policy: fresh or reused on retry (explicit choice).
    #[serde(default)]
    pub retry_worktree: RetryWorktreePolicy,
    /// Dirty-workspace policy for isolated work (never discards user work).
    #[serde(default)]
    pub dirty: DirtyPolicy,
    /// Hard cap on worktrees the engine may create.
    #[serde(default = "default_max_worktrees")]
    pub max_worktrees: usize,
}

fn default_max_worktrees() -> usize {
    32
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            max_agents: 5,
            max_parallel_tasks: 2,
            failure: DependencyFailurePolicy::BlockDownstream,
            review_required: false,
            retry: RetryPolicy::default(),
            max_cost_cents: None,
            retry_worktree: RetryWorktreePolicy::Fresh,
            dirty: DirtyPolicy::default(),
            max_worktrees: default_max_worktrees(),
        }
    }
}

/// Snapshot view the scheduler reads (built by the engine from the live
/// AgentRuntime). Keeps the scheduler a pure function of its inputs.
#[derive(Debug, Clone, Default)]
pub struct SchedulerView {
    /// Only tasks the scheduler considers Running appear here.
    pub running: HashMap<TaskId, RuntimeAgentView>,
    /// Phase 3D §9: tasks whose declared input artifacts are unavailable
    /// (task id → reason). Populated by the engine from the artifact
    /// store; the scheduler blocks such tasks instead of starting them.
    pub artifact_blocked: HashMap<TaskId, String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeAgentView {
    pub state: AgentState,
    pub exit_code: Option<i32>,
    pub work: Option<AgentWork>,
    pub estimated_cost_cents: Option<u64>,
    /// True when the underlying process has exited (reader thread observed
    /// EOF). The pump thread transitions `state` to a terminal state on its
    /// own schedule, so `exited == true` with a non-terminal `state` means
    /// the transition is still in flight (3a §48 determinism).
    pub exited: bool,
}

/// True for states the scheduler treats as settled terminal outcomes.
fn is_terminal_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Completed | AgentState::Failed | AgentState::Crashed | AgentState::Stopped
    )
}

/// Deterministic commands the scheduler issues; the engine executes them
/// (never the scheduler — §14, §54).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerCommand {
    SpawnTask {
        task_id: TaskId,
    },
    StopTask {
        task_id: TaskId,
        execution_id: ExecutionId,
    },
}

/// window into scheduler state for `scheduler.status()` (§43).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerStatus {
    pub policy: TaskPolicy,
    pub queued: Vec<TaskId>,
    pub running: Vec<(TaskId, ExecutionId)>,
    pub states: Vec<(TaskId, TaskStatus)>,
    pub started_count: u32,
    pub completed_count: u32,
    pub failed_count: u32,
    pub cancelled_count: u32,
    pub retried_count: u32,
    pub actual_cost_cents: u64,
}

// ---------------------------------------------------------------------------
// §10, §21–§22 TaskScheduler + TaskEvent
// ---------------------------------------------------------------------------

/// Task lifecycle events (§21) — published through the existing
/// ApplicationEvent bus (single bus, §21/§53). Same-task events are emitted
/// in transition order by the engine tick (§22).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEvent {
    TaskCreated {
        task_id: TaskId,
        title: String,
    },
    TaskReady {
        task_id: TaskId,
    },
    TaskStarted {
        task_id: TaskId,
        execution_id: ExecutionId,
    },
    TaskBlocked {
        task_id: TaskId,
        reason: String,
    },
    TaskWaiting {
        task_id: TaskId,
        reason: String,
    },
    TaskNeedsReview {
        task_id: TaskId,
    },
    TaskCompleted {
        task_id: TaskId,
        /// Boxed: `TaskResult` carries the worktree diff provenance and
        /// this event rides the `ApplicationEvent` bus, which must stay
        /// small by value (clippy::large_enum_variant). No consumer
        /// destructures the result from the event — the engine reads it
        /// from the task graph.
        result: Box<TaskResult>,
    },
    TaskFailed {
        task_id: TaskId,
        error: TaskError,
    },
    TaskCancelled {
        task_id: TaskId,
    },
    TaskRetrying {
        task_id: TaskId,
        attempt: u32,
        error: TaskError,
    },
    TaskArtifactCreated {
        task_id: TaskId,
        artifact: Artifact,
    },
    TaskInterrupted {
        task_id: TaskId,
        reason: String,
    },
}

impl TaskEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            TaskEvent::TaskCreated { .. } => "created",
            TaskEvent::TaskReady { .. } => "ready",
            TaskEvent::TaskStarted { .. } => "started",
            TaskEvent::TaskBlocked { .. } => "blocked",
            TaskEvent::TaskWaiting { .. } => "waiting",
            TaskEvent::TaskNeedsReview { .. } => "needs_review",
            TaskEvent::TaskCompleted { .. } => "completed",
            TaskEvent::TaskFailed { .. } => "failed",
            TaskEvent::TaskCancelled { .. } => "cancelled",
            TaskEvent::TaskRetrying { .. } => "retrying",
            TaskEvent::TaskArtifactCreated { .. } => "artifact_created",
            TaskEvent::TaskInterrupted { .. } => "interrupted",
        }
    }

    pub fn task_id(&self) -> &TaskId {
        match self {
            TaskEvent::TaskCreated { task_id, .. }
            | TaskEvent::TaskReady { task_id }
            | TaskEvent::TaskStarted { task_id, .. }
            | TaskEvent::TaskBlocked { task_id, .. }
            | TaskEvent::TaskWaiting { task_id, .. }
            | TaskEvent::TaskNeedsReview { task_id }
            | TaskEvent::TaskCompleted { task_id, .. }
            | TaskEvent::TaskFailed { task_id, .. }
            | TaskEvent::TaskCancelled { task_id }
            | TaskEvent::TaskRetrying { task_id, .. }
            | TaskEvent::TaskArtifactCreated { task_id, .. }
            | TaskEvent::TaskInterrupted { task_id, .. } => task_id,
        }
    }
}

/// Deterministic orchestration engine (3a.md §10). Owns the graph, policy,
/// and queue; `step()` reads a [`SchedulerView`] and emits commands + events.
/// The same graph plus the same view always produce the same commands in
/// the same order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskScheduler {
    graph: TaskGraph,
    policy: TaskPolicy,
    queue: VecDeque<TaskId>,
    running: Vec<(TaskId, ExecutionId)>,
    started_count: u32,
    completed_count: u32,
    failed_count: u32,
    cancelled_count: u32,
    retried_count: u32,
    actual_cost_cents: u64,
    /// Per-task event order (task_id, kind) — the determinism trace (§48).
    trace: Vec<(TaskId, String)>,
    /// Event outbox — drained by the engine each tick.
    #[serde(skip)]
    outbox: Vec<TaskEvent>,
    /// When the settle-deferral first became active (3a §48 determinism),
    /// bounding how long terminal transitions wait on a still-`Starting`
    /// sibling. None while no deferral is in flight.
    #[serde(skip)]
    defer_since_ms: Option<u64>,
}

impl TaskScheduler {
    pub fn new(graph: TaskGraph, policy: TaskPolicy) -> Self {
        Self {
            graph,
            policy,
            queue: VecDeque::new(),
            running: Vec::new(),
            started_count: 0,
            completed_count: 0,
            failed_count: 0,
            cancelled_count: 0,
            retried_count: 0,
            actual_cost_cents: 0,
            trace: Vec::new(),
            outbox: Vec::new(),
            defer_since_ms: None,
        }
    }

    pub fn graph(&self) -> &TaskGraph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut TaskGraph {
        &mut self.graph
    }

    pub fn policy(&self) -> &TaskPolicy {
        &self.policy
    }

    pub fn set_policy(&mut self, policy: TaskPolicy) {
        self.policy = policy;
    }

    pub fn actual_cost_cents(&self) -> u64 {
        self.actual_cost_cents
    }

    /// §43 `scheduler.status()` — full scheduler snapshot.
    pub fn status(&self) -> SchedulerStatus {
        SchedulerStatus {
            policy: self.policy.clone(),
            queued: self.queue.iter().cloned().collect(),
            running: self.running.clone(),
            states: self
                .graph
                .list_task_ids()
                .into_iter()
                .map(|id| {
                    (
                        id.clone(),
                        self.graph
                            .get_task(&id)
                            .map(|t| t.status)
                            .unwrap_or(TaskStatus::Pending),
                    )
                })
                .collect(),
            started_count: self.started_count,
            completed_count: self.completed_count,
            failed_count: self.failed_count,
            cancelled_count: self.cancelled_count,
            retried_count: self.retried_count,
            actual_cost_cents: self.actual_cost_cents,
        }
    }

    /// Determinism trace: (task_id, event-kind) in emission order.
    pub fn trace(&self) -> &[(TaskId, String)] {
        &self.trace
    }

    pub fn take_events(&mut self) -> Vec<TaskEvent> {
        std::mem::take(&mut self.outbox)
    }

    /// Marks every Pending task Ready/Blocked per §9 and queues it.
    pub fn submit_all(&mut self) {
        let ids: Vec<TaskId> = self.graph.list_task_ids();
        for id in ids {
            self.classify_pending(&id);
        }
    }

    fn classify_pending(&mut self, id: &TaskId) {
        let Some(task) = self.graph.get_task(id) else {
            return;
        };
        let status = task.status;
        if status != TaskStatus::Pending && status != TaskStatus::Blocked {
            return;
        }
        // Budget-blocked and artifact-blocked tasks stay blocked until a
        // human resolves them (§37, 3d.md §9 — never silently continued).
        if status == TaskStatus::Blocked
            && task
                .error
                .as_ref()
                .map(|e| {
                    e.kind == TaskErrorKind::BudgetExceeded
                        || e.kind == TaskErrorKind::ArtifactMissing
                })
                .unwrap_or(false)
        {
            return;
        }
        let satisfied = self.graph.dependencies_satisfied(id);
        let failed = self.graph.failed_dependency(id);
        if satisfied {
            let task = self.graph.get_task_mut(id).unwrap();
            if task.transition(TaskStatus::Ready).is_ok() {
                self.emit(TaskEvent::TaskReady {
                    task_id: id.clone(),
                });
            }
            self.queue.push_back(id.clone());
        } else if let Some(failed) = failed {
            let to = match self.policy.failure {
                DependencyFailurePolicy::BlockDownstream => TaskStatus::Blocked,
                DependencyFailurePolicy::SkipDownstream => TaskStatus::Skipped,
            };
            let task = self.graph.get_task_mut(id).unwrap();
            if task.transition(to).is_ok() {
                match to {
                    TaskStatus::Blocked => self.emit(TaskEvent::TaskBlocked {
                        task_id: id.clone(),
                        reason: format!("dependency `{failed}` did not complete"),
                    }),
                    _ => self.emit(TaskEvent::TaskCancelled {
                        task_id: id.clone(),
                    }),
                }
            }
        }
    }

    /// One deterministic scheduler pass.
    ///
    /// 1. observes running agents (completions, failures, waits);
    /// 2. re-classifies Pending/Blocked tasks;
    /// 3. starts queued tasks up to `max_parallel_tasks` ∩ `max_agents`,
    ///    gated by the cost budget.
    ///
    /// `allow_spawns == false` (Phase 3F §33 PAUSE ALL) runs the observations
    /// and classification but returns no spawn commands: queued tasks stay
    /// Ready inside the queue — no optimistic Running transition, no attempt
    /// or budget accounting — and Resume simply runs them.
    pub fn step(&mut self, view: &SchedulerView, allow_spawns: bool) -> Vec<SchedulerCommand> {
        let mut cmds = Vec::new();
        self.observe_running(view);
        let ids: Vec<TaskId> = self.graph.list_task_ids();
        for id in ids {
            self.classify_pending(&id);
        }
        if !allow_spawns {
            return cmds;
        }
        while self.running.len()
            + cmds
                .iter()
                .filter(|c| matches!(c, SchedulerCommand::SpawnTask { .. }))
                .count()
            < self.policy.max_parallel_tasks.min(self.policy.max_agents)
        {
            let Some(id) = self.queue.pop_front() else {
                break;
            };
            if let Some(task) = self.graph.get_task(&id) {
                if task.status != TaskStatus::Ready {
                    continue;
                }
            }
            if let Some(cap) = self.policy.max_cost_cents {
                if self.actual_cost_cents >= cap {
                    if let Some(task) = self.graph.get_task_mut(&id) {
                        let msg = format!(
                            "cost budget exceeded ({}¢ ≥ {}¢)",
                            self.actual_cost_cents, cap
                        );
                        if task.transition(TaskStatus::Blocked).is_ok() {
                            task.error = Some(TaskError::budget(&msg));
                            self.emit(TaskEvent::TaskBlocked {
                                task_id: id.clone(),
                                reason: msg,
                            });
                        }
                    }
                    continue;
                }
            }
            // Phase 3D §9: artifact readiness — a task with a declared
            // input artifact that is unavailable is Blocked, never silently
            // continued. The engine stamps `artifact_blocked` from the
            // artifact store; the scheduler enforces it (§8–§9).
            if let Some(reason) = view.artifact_blocked.get(&id) {
                if let Some(task) = self.graph.get_task_mut(&id) {
                    if task.transition(TaskStatus::Blocked).is_ok() {
                        task.error = Some(TaskError::new(
                            TaskErrorKind::ArtifactMissing,
                            FailureClass::TaskFailure,
                            reason.clone(),
                        ));
                        self.emit(TaskEvent::TaskBlocked {
                            task_id: id.clone(),
                            reason: reason.clone(),
                        });
                    }
                }
                continue;
            }
            let Some(task) = self.graph.get_task_mut(&id) else {
                continue;
            };
            task.attempt_count += 1;
            if task.started_at_ms.is_none() {
                task.started_at_ms = Some(now_ms());
            }
            task.error = None;
            if task.transition(TaskStatus::Running).is_err() {
                continue;
            }
            self.started_count += 1;
            cmds.push(SchedulerCommand::SpawnTask {
                task_id: id.clone(),
            });
        }
        cmds
    }

    /// How long terminal transitions wait for a co-started sibling that is
    /// still `Starting` (spawned but its process not yet observed exiting)
    /// before settling anyway. Generous vs. the ~25ms fork/exec jitter seen
    /// under load, but bounded so a genuinely slow-starting task can never
    /// stall completions indefinitely.
    const DEFER_SETTLE_BOUND_MS: u64 = 100;

    fn observe_running(&mut self, view: &SchedulerView) {
        let running = std::mem::take(&mut self.running);
        let now = now_ms();
        // 3a §48 determinism: two agents whose processes die in the same
        // instant can be observed in *different* engine frames — the reader
        // thread observes EOF on its own schedule and the pump thread
        // transitions the agent state on its own schedule, both of which
        // under load can lag a frame behind the actual exit — flipping the
        // schedule trace run-to-run. Defer ALL terminal transitions this
        // frame so near-simultaneous exits settle together, deterministically.
        // Two sub-cases:
        //   1. a task has exited (reader EOF *or* authoritative `try_wait`)
        //      but its pump state hasn't settled yet — the process is gone,
        //      so the pump settles within a frame;
        //   2. a task is freshly spawned (`Created`/`Starting`, not yet
        //      exited) while another running task is terminal-ready — under
        //      load the sibling's fork/exec can delay its exit by a frame,
        //      so hold for [`DEFER_SETTLE_BOUND_MS`] before settling.
        let any_exited_unsettled = running.iter().any(|(task_id, _)| {
            view.running
                .get(task_id)
                .map(|a| a.exited && !is_terminal_state(a.state))
                .unwrap_or(false)
        });
        let any_starting_unexited = running.iter().any(|(task_id, _)| {
            view.running
                .get(task_id)
                .map(|a| !a.exited && matches!(a.state, AgentState::Created | AgentState::Starting))
                .unwrap_or(false)
        });
        let any_terminal_ready = running.iter().any(|(task_id, _)| {
            view.running
                .get(task_id)
                .map(|a| is_terminal_state(a.state))
                .unwrap_or(false)
        });
        let defer = if any_exited_unsettled {
            true
        } else if any_starting_unexited && any_terminal_ready {
            // Bounded: only while the co-started sibling is still within
            // the settle window; a sibling that stays `Starting` (e.g. a
            // long-running silent process) must not stall completions.
            match self.defer_since_ms {
                Some(t) => now.saturating_sub(t) < Self::DEFER_SETTLE_BOUND_MS,
                None => true,
            }
        } else {
            false
        };
        if defer {
            self.defer_since_ms.get_or_insert(now);
            self.running.extend(running);
            return;
        }
        self.defer_since_ms = None;
        for (task_id, eid) in running {
            let Some(agent) = view.running.get(&task_id) else {
                // Session vanished underneath us — honest failure, never a
                // silent hang (§25).
                let err = TaskError::new(
                    TaskErrorKind::Unknown,
                    FailureClass::Unknown,
                    "task agent session disappeared",
                );
                let _ = self.fail_task(&task_id, err, Some(&eid));
                continue;
            };
            match agent.state {
                AgentState::Completed => {
                    self.complete_task(&task_id, &eid, agent);
                }
                AgentState::Failed | AgentState::Crashed | AgentState::Stopped => {
                    let class = FailureClass::classify(
                        agent.state,
                        agent.exit_code,
                        agent
                            .work
                            .as_ref()
                            .map(|w| w.errors.as_slice())
                            .unwrap_or(&[]),
                    );
                    let (kind, msg) = match agent.state {
                        AgentState::Crashed => (
                            TaskErrorKind::AgentCrashed,
                            format!("agent crashed ({:?})", agent.exit_code),
                        ),
                        AgentState::Stopped => (
                            TaskErrorKind::AgentFailed,
                            "agent was stopped without a task cancellation".to_string(),
                        ),
                        _ => (TaskErrorKind::AgentFailed, "agent failed".to_string()),
                    };
                    let err = TaskError::new(kind, class, msg);
                    let _ = self.fail_task(&task_id, err, Some(&eid));
                }
                AgentState::NeedsApproval | AgentState::Waiting | AgentState::Blocked => {
                    let Some(task) = self.graph.get_task_mut(&task_id) else {
                        continue;
                    };
                    if task.status == TaskStatus::Running {
                        let reason = match agent.state {
                            AgentState::NeedsApproval => "agent requires approval".to_string(),
                            AgentState::Blocked => "agent reported itself blocked".to_string(),
                            _ => "agent is waiting for input".to_string(),
                        };
                        if task.transition(TaskStatus::Waiting).is_ok() {
                            self.emit(TaskEvent::TaskWaiting {
                                task_id: task_id.clone(),
                                reason,
                            });
                        }
                    }
                    self.running.push((task_id, eid));
                }
                _ => {
                    // Still working (Created/Starting/Working/…) — stays.
                    self.running.push((task_id, eid));
                }
            }
        }
    }

    fn complete_task(&mut self, task_id: &TaskId, eid: &ExecutionId, agent: &RuntimeAgentView) {
        let (needs_review, result, artifact_events) = {
            let Some(task) = self.graph.get_task_mut(task_id) else {
                return;
            };
            if task.status != TaskStatus::Running && task.status != TaskStatus::Waiting {
                return;
            }
            let started = task.started_at_ms.unwrap_or(0);
            let dur = now_ms().saturating_sub(started);
            task.duration_ms += dur;
            task.completed_at_ms = Some(now_ms());
            task.agent_execution_id = Some(eid.clone());
            let work = match agent.work.clone() {
                Some(w) => w,
                None => AgentWork::new("orchestration", &task.title),
            };
            let mut artifacts: Vec<Artifact> = task.output_artifacts.clone();
            let mut artifact_events = Vec::new();
            for f in work.files_changed.iter().take(64) {
                let art = Artifact {
                    id: new_artifact_id(),
                    kind: ArtifactType::CodeChange,
                    path: Some(f.clone()),
                    description: "observed change".to_string(),
                    created_by_task: Some(task_id.clone()),
                    metadata: vec![("auto".to_string(), "true".to_string())],
                    created_by_agent: Some(task.assigned_agent.clone()),
                    workspace_id: Some(task.workspace_id.clone()),
                    worktree: task.worktree_id.clone(),
                    revision: task.result.as_ref().and_then(|r| r.result_revision.clone()),
                    created_at_ms: now_ms(),
                };
                artifacts.push(art.clone());
                task.output_artifacts.push(art.clone());
                artifact_events.push(TaskEvent::TaskArtifactCreated {
                    task_id: task_id.clone(),
                    artifact: art,
                });
            }
            let needs_review = task.review_required || self.policy.review_required;
            let result = TaskResult {
                status: if needs_review {
                    TaskStatus::NeedsReview
                } else {
                    TaskStatus::Completed
                },
                summary: TaskResult::deterministic_summary(
                    work.files_changed.len(),
                    work.commands.len(),
                    task.attempt_count,
                ),
                artifacts,
                files_changed: work.files_changed.iter().cloned().collect(),
                commands: work.commands.clone(),
                duration_ms: task.duration_ms,
                error: None,
                agent_execution_id: Some(eid.clone()),
                attempt_count: task.attempt_count,
                estimated_cost_cents: agent.estimated_cost_cents,
                // Phase 3C: worktree provenance is patched by the engine's
                // environment layer after completion (§17) — the scheduler
                // stays pure (no git).
                base_revision: None,
                result_revision: None,
                branch: None,
                worktree: None,
                diff_summary: None,
                // Phase 3D §13: structured fields — deterministic, bounded.
                // `metrics` carries observed counts; warnings/errors/
                // recommendations are populated by reviewers/synthesis or
                // left empty (never LLM-generated summaries).
                metrics: vec![
                    ("attempts".to_string(), task.attempt_count.to_string()),
                    (
                        "files_changed".to_string(),
                        work.files_changed.len().to_string(),
                    ),
                    ("commands_run".to_string(), work.commands.len().to_string()),
                ],
                warnings: Vec::new(),
                errors: Vec::new(),
                recommendations: Vec::new(),
            };
            task.result = Some(result.clone());
            (needs_review, result, artifact_events)
        };
        if let Some(c) = agent.estimated_cost_cents {
            self.actual_cost_cents = self.actual_cost_cents.saturating_add(c);
        }
        for ev in artifact_events {
            self.emit(ev);
        }
        if needs_review {
            if self
                .graph
                .get_task_mut(task_id)
                .map(|t| {
                    t.transition(TaskStatus::NeedsReview)
                        .map(|_| true)
                        .unwrap_or(false)
                })
                .unwrap_or(false)
            {
                self.emit(TaskEvent::TaskNeedsReview {
                    task_id: task_id.clone(),
                });
            }
        } else if self
            .graph
            .get_task_mut(task_id)
            .map(|t| {
                t.transition(TaskStatus::Completed)
                    .map(|_| true)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        {
            self.completed_count += 1;
            self.emit(TaskEvent::TaskCompleted {
                task_id: task_id.clone(),
                result: Box::new(result),
            });
        }
    }

    /// Shared failure handling: retry policy or terminal failure (§25–§26).
    /// Returns true when the task was re-queued for another attempt.
    fn fail_task(&mut self, task_id: &TaskId, err: TaskError, eid: Option<&ExecutionId>) -> bool {
        let Some(task) = self.graph.get_task_mut(task_id) else {
            return false;
        };
        if task.transition(TaskStatus::Failed).is_err() {
            return false;
        }
        task.error = Some(err.clone());
        if let Some(e) = eid {
            task.agent_execution_id = Some(e.clone());
        }
        task.completed_at_ms = Some(now_ms());
        let retryable = self.policy.retry.may_retry(err.class, task.attempt_count);
        let attempts = task.attempt_count;
        if retryable {
            if task.transition(TaskStatus::Ready).is_err() {
                return false;
            }
            self.retried_count += 1;
            self.queue.push_back(task_id.clone());
            self.emit(TaskEvent::TaskRetrying {
                task_id: task_id.clone(),
                attempt: attempts,
                error: err,
            });
            true
        } else {
            self.failed_count += 1;
            self.emit(TaskEvent::TaskFailed {
                task_id: task_id.clone(),
                error: err,
            });
            false
        }
    }

    pub fn emit(&mut self, event: TaskEvent) {
        let kind = event.kind().to_string();
        let id = event.task_id().clone();
        self.trace.push((id, kind));
        self.outbox.push(event);
    }

    /// Snapshot of running (task, execution) pairs for building the engine
    /// view (3a.md §22 — observation order == running order).
    pub fn running_snapshot(&self) -> Vec<(TaskId, ExecutionId)> {
        self.running.clone()
    }

    /// Spawn confirmation from the engine (command executed).
    pub fn note_spawned(&mut self, task_id: &TaskId, eid: &ExecutionId) {
        if let Some(task) = self.graph.get_task_mut(task_id) {
            task.agent_execution_id = Some(eid.clone());
        }
        self.running.push((task_id.clone(), eid.clone()));
        self.emit(TaskEvent::TaskStarted {
            task_id: task_id.clone(),
            execution_id: eid.clone(),
        });
    }

    pub fn note_spawn_failed(&mut self, task_id: &TaskId, err: anyhow::Error) {
        let err = TaskError::new(
            TaskErrorKind::AgentSpawnFailed,
            FailureClass::Unknown,
            format!("{err:#}"),
        );
        // Attempt was already counted at start; the failure occurred before
        // a process existed, so undo the attempt accounting for policy math.
        if let Some(task) = self.graph.get_task_mut(task_id) {
            task.attempt_count = task.attempt_count.saturating_sub(1);
        }
        let retried = self.fail_task(task_id, err, None);
        let _ = retried;
    }

    /// Engine confirms a StopTask command completed (process dead).
    pub fn note_stopped(&mut self, task_id: &TaskId, eid: &ExecutionId) {
        self.running.retain(|(t, e)| t != task_id || e != eid);
        let _ = eid;
    }

    /// §27 cancellation. Running tasks emit a StopTask command; the engine
    /// stops the process through AgentRuntime (never direct kills).
    pub fn cancel(&mut self, task_id: &TaskId) -> Result<Vec<SchedulerCommand>, TaskGraphError> {
        let Some(task) = self.graph.get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        match task.status {
            TaskStatus::Running | TaskStatus::Waiting => {
                let eid = task.agent_execution_id.clone().unwrap_or_default();
                self.running.retain(|(t, _)| t != task_id);
                if task.transition(TaskStatus::Cancelled).is_err() {
                    return Ok(Vec::new());
                }
                self.cancelled_count += 1;
                self.emit(TaskEvent::TaskCancelled {
                    task_id: task_id.clone(),
                });
                Ok(vec![SchedulerCommand::StopTask {
                    task_id: task_id.clone(),
                    execution_id: eid,
                }])
            }
            _ => {
                self.queue.retain(|t| t != task_id);
                if task.transition(TaskStatus::Cancelled).is_err() {
                    return Ok(Vec::new());
                }
                self.cancelled_count += 1;
                self.emit(TaskEvent::TaskCancelled {
                    task_id: task_id.clone(),
                });
                Ok(Vec::new())
            }
        }
    }

    /// §43 `scheduler.stop()` — cancels queued + running tasks.
    pub fn stop(&mut self) -> Vec<SchedulerCommand> {
        let mut cmds = Vec::new();
        let ids: Vec<TaskId> = self.graph.list_task_ids();
        for id in ids {
            if let Ok(mut c) = self.cancel(&id) {
                cmds.append(&mut c);
            }
        }
        cmds
    }

    /// Manual retry of a terminal/failed/blocked task (§41/§43).
    pub fn retry(&mut self, task_id: &TaskId) -> Result<(), TaskGraphError> {
        let Some(task) = self.graph.get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        match task.status {
            TaskStatus::Failed
            | TaskStatus::Cancelled
            | TaskStatus::Skipped
            | TaskStatus::Interrupted
            | TaskStatus::Blocked => {
                task.attempt_count = 0;
                task.error = None;
                task.result = None;
                task.completed_at_ms = None;
                task.started_at_ms = None;
                task.duration_ms = 0;
                task.agent_execution_id = None;
                task.transition(TaskStatus::Ready)
                    .map_err(TaskGraphError::InvalidTransition)?;
                self.queue.push_back(task_id.clone());
                self.emit(TaskEvent::TaskReady {
                    task_id: task_id.clone(),
                });
                self.retried_count += 1;
                Ok(())
            }
            other => Err(TaskGraphError::InvalidTransition(
                TaskTransitionError::forbidden(other, TaskStatus::Ready),
            )),
        }
    }

    /// §29 human review boundary: approve or reject a NeedsReview task.
    pub fn resolve_review(
        &mut self,
        task_id: &TaskId,
        approve: bool,
    ) -> Result<(), TaskGraphError> {
        let Some(task) = self.graph.get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        if task.status != TaskStatus::NeedsReview {
            return Err(TaskGraphError::InvalidTransition(
                TaskTransitionError::forbidden(task.status, TaskStatus::Completed),
            ));
        }
        if approve {
            let result = task.result.clone().unwrap_or_default();
            if task.transition(TaskStatus::Completed).is_err() {
                return Ok(());
            }
            self.completed_count += 1;
            self.emit(TaskEvent::TaskCompleted {
                task_id: task_id.clone(),
                result: Box::new(result),
            });
        } else {
            let err = TaskError::new(
                TaskErrorKind::Unknown,
                FailureClass::TaskFailure,
                "rejected in review",
            );
            if task.transition(TaskStatus::Failed).is_err() {
                return Ok(());
            }
            task.error = Some(err.clone());
            self.failed_count += 1;
            self.emit(TaskEvent::TaskFailed {
                task_id: task_id.clone(),
                error: err,
            });
        }
        Ok(())
    }

    /// §24 restore: Running/Waiting tasks are marked Interrupted — the app
    /// was offline; nothing resumes silently. NeedsReview survives.
    pub fn mark_interrupted(&mut self, reason: &str) {
        let ids: Vec<TaskId> = self.graph.list_task_ids();
        for id in ids {
            let Some(task) = self.graph.get_task_mut(&id) else {
                continue;
            };
            if matches!(task.status, TaskStatus::Running | TaskStatus::Waiting)
                && task.transition(TaskStatus::Interrupted).is_ok()
            {
                self.emit(TaskEvent::TaskInterrupted {
                    task_id: id.clone(),
                    reason: reason.to_string(),
                });
            }
        }
    }

    /// §52 persistence export — versioned, bounded, no secrets. Live queue /
    /// running state is never persisted; `import_persisted` rebuilds it
    /// deterministically. Arguments and environment values are redacted on
    /// the exported copy (§35): registered secrets never reach disk, while
    /// live records keep their original arguments for retry fidelity
    /// (mirrors `AgentLaunchConfig::redact`).
    pub fn export_persisted(&self) -> PersistedSchedulerState {
        let mut graph = self.graph.clone();
        for id in graph.list_task_ids() {
            let Some(task) = graph.get_task_mut(&id) else {
                continue;
            };
            task.arguments = task
                .arguments
                .iter()
                .map(|a| crate::redact::Redactor::redact(a))
                .collect();
            task.environment = task
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), crate::redact::Redactor::redact(v)))
                .collect();
        }
        PersistedSchedulerState {
            version: PERSISTED_SCHEDULER_VERSION,
            graph,
            policy: self.policy.clone(),
            actual_cost_cents: self.actual_cost_cents,
        }
    }

    /// Rebuilds a scheduler from disk state: Running/Waiting → Interrupted
    /// (§24); tasks that were Ready are re-queued in insertion order.
    pub fn import_persisted(state: PersistedSchedulerState) -> Self {
        let mut s = Self::new(state.graph, state.policy);
        s.actual_cost_cents = state.actual_cost_cents;
        s.mark_interrupted("application restarted — running tasks were interrupted");
        for id in s.graph.list_task_ids() {
            if s.graph
                .get_task(&id)
                .map(|t| t.status == TaskStatus::Ready)
                .unwrap_or(false)
            {
                s.queue.push_back(id);
            }
        }
        s
    }
}

/// Versioned on-disk slice of scheduler state (§52). `version` gates future
/// migrations; live state (queue/running) is intentionally absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedSchedulerState {
    pub version: u32,
    pub graph: TaskGraph,
    pub policy: TaskPolicy,
    pub actual_cost_cents: u64,
}

pub const PERSISTED_SCHEDULER_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, title: &str, agent: &str) -> Task {
        let mut t = Task::new(title, "", agent, "ws-1");
        t.id = id.to_string();
        t
    }

    #[test]
    fn transition_table_rejects_invalid_moves() {
        use TaskStatus::*;
        let valid = [
            (Pending, Ready),
            (Pending, Blocked),
            (Pending, Cancelled),
            (Pending, Skipped),
            (Ready, Running),
            (Ready, Cancelled),
            (Running, NeedsReview),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Waiting),
            (Waiting, Running),
            (Waiting, Completed),
            (NeedsReview, Completed),
            (NeedsReview, Failed),
            (Blocked, Ready),
            (Interrupted, Ready),
            // §25/§26 retry arc: Failed re-arms to Ready (auto-retry queues
            // it; `task.retry` uses the same arc for manual retries).
            (Failed, Ready),
        ];
        for (from, to) in valid {
            assert!(
                TaskStatus::can_transition(from, to).is_ok(),
                "{from:?} → {to:?}"
            );
        }
        let invalid = [
            (Completed, Ready),
            (Completed, Running),
            (Cancelled, Running),
            (Skipped, Completed),
            (Pending, NeedsReview),
            (NeedsReview, Running),
            (Waiting, NeedsReview),
            (Interrupted, Running),
            (Completed, Completed),
            // Terminal states never re-arm (except the explicit retry arc).
            (Cancelled, Ready),
            (Skipped, Ready),
            (Completed, Failed),
        ];
        for (from, to) in invalid {
            assert!(
                TaskStatus::can_transition(from, to).is_err(),
                "{from:?} → {to:?} must be rejected"
            );
        }
    }

    #[test]
    fn graph_rejects_duplicate_and_missing_dependencies() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", "A", "fake-agent")).unwrap();
        g.add_task(task("b", "B", "fake-agent")).unwrap();
        assert_eq!(
            g.add_dependency(&"a".to_string(), &"c".to_string()),
            Err(TaskGraphError::UnknownTask("c".to_string()))
        );
        assert_eq!(
            g.add_dependency(&"a".to_string(), &"a".to_string()),
            Err(TaskGraphError::SelfDependency("a".to_string()))
        );
        g.add_dependency(&"b".to_string(), &"a".to_string())
            .unwrap();
        assert_eq!(
            g.add_dependency(&"b".to_string(), &"a".to_string()),
            Err(TaskGraphError::DuplicateDependency {
                task: "b".to_string(),
                dependency: "a".to_string(),
            })
        );
    }

    #[test]
    fn graph_detects_cycles() {
        let mut g = TaskGraph::new();
        for id in ["a", "b", "c"] {
            g.add_task(task(id, id, "fake-agent")).unwrap();
        }
        g.add_dependency(&"a".to_string(), &"b".to_string())
            .unwrap();
        g.add_dependency(&"b".to_string(), &"c".to_string())
            .unwrap();
        // c → a closes the cycle: rejected with the path.
        let err = g
            .add_dependency(&"c".to_string(), &"a".to_string())
            .unwrap_err();
        match err {
            TaskGraphError::Cycle { path } => {
                assert!(path.len() >= 2, "cycle path: {path:?}");
                assert_eq!(path.first(), path.last());
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
        assert!(g.find_cycle().is_none(), "edge must have been rolled back");
        // Long cycle a → b → c → a built through one task.
        let mut g2 = TaskGraph::new();
        for id in ["a", "b", "c", "d"] {
            g2.add_task(task(id, id, "fake-agent")).unwrap();
        }
        g2.add_dependency(&"a".to_string(), &"b".to_string())
            .unwrap();
        g2.add_dependency(&"b".to_string(), &"c".to_string())
            .unwrap();
        g2.add_dependency(&"c".to_string(), &"d".to_string())
            .unwrap();
        let err = g2
            .add_dependency(&"d".to_string(), &"a".to_string())
            .unwrap_err();
        assert!(matches!(err, TaskGraphError::Cycle { path } if path.len() >= 2));
    }

    #[test]
    fn graph_validate_reports_structural_issues() {
        let mut g = TaskGraph::new();
        g.add_task(task("a", "A", "fake-agent")).unwrap();
        g.add_task(task("b", "B", "fake-agent")).unwrap();
        g.get_task_mut(&"b".to_string())
            .unwrap()
            .dependencies
            .push("ghost".to_string());
        g.get_task_mut(&"a".to_string())
            .unwrap()
            .input_artifacts
            .push("hopefully-missing-artifact".to_string());
        let issues = g.validate();
        assert!(
            issues.iter().any(|e| matches!(
                e,
                TaskGraphError::MissingDependency { task, dependency } if task == "b" && dependency == "ghost"
            )),
            "missing dependency: {issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|e| matches!(e, TaskGraphError::UnknownArtifact { .. })),
            "unknown artifact: {issues:?}"
        );
    }

    #[test]
    fn topological_order_is_deterministic() {
        let mut g = TaskGraph::new();
        for id in ["d", "b", "a", "c"] {
            g.add_task(task(id, id, "fake-agent")).unwrap();
        }
        g.add_dependency(&"c".to_string(), &"b".to_string())
            .unwrap();
        g.add_dependency(&"b".to_string(), &"a".to_string())
            .unwrap();
        g.add_dependency(&"d".to_string(), &"b".to_string())
            .unwrap();
        let order = g.topological_order().unwrap();
        // a has no deps → first. b depends on a. d and c both depend on b;
        // of those two, d is earlier in insertion order, so it goes first.
        assert_eq!(
            order,
            vec!["a", "b", "d", "c"]
                .into_iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        // Same graph → same order repeatedly.
        assert_eq!(order, g.topological_order().unwrap());
    }

    #[test]
    fn explicit_agent_assignment_required() {
        let t = task("x", "X", "fake-agent");
        assert_eq!(t.assigned_agent, "fake-agent");
        assert!(t.agent_execution_id.is_none());
    }

    #[test]
    fn retry_policy_honors_classification() {
        let p = RetryPolicy::default();
        // max_retries = 1: one retry after the first failed attempt.
        assert!(p.may_retry(FailureClass::NetworkFailure, 1));
        assert!(p.may_retry(FailureClass::AgentCrash, 1));
        assert!(!p.may_retry(FailureClass::AuthenticationFailure, 1));
        assert!(!p.may_retry(FailureClass::TaskFailure, 0));
        assert!(!p.may_retry(FailureClass::NetworkFailure, 2));
        let strict = RetryPolicy {
            max_retries: 2,
            retry_classes: vec![FailureClass::TransientProviderFailure],
        };
        assert!(strict.may_retry(FailureClass::TransientProviderFailure, 2));
        assert!(!strict.may_retry(FailureClass::AgentCrash, 0));
    }

    #[test]
    fn failure_classification_is_deterministic() {
        use AgentState::*;
        assert_eq!(
            FailureClass::classify(Completed, Some(0), &[]),
            FailureClass::TaskFailure
        );
        assert_eq!(
            FailureClass::classify(Failed, Some(2), &[]),
            FailureClass::AuthenticationFailure
        );
        assert_eq!(
            FailureClass::classify(Failed, Some(3), &[]),
            FailureClass::TransientProviderFailure
        );
        assert_eq!(
            FailureClass::classify(Crashed, Some(139), &[]),
            FailureClass::AgentCrash
        );
        assert_eq!(
            FailureClass::classify(Failed, Some(1), &[]),
            FailureClass::TaskFailure
        );
    }
}
