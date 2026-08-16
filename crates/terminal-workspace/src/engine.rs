//! The multiplexer runtime (§8–9, §22–23, §27–28).
//!
//! Ownership model (Phase 1 spec §3):
//!
//! ```text
//! Workspace  ── owns ──▶ Tabs ── owns ──▶ Pane tree (pure data)
//! Pane       ── references ──▶ SessionId
//! Engine     ── owns ──▶ Session (PTY+parser+state), keyed by SessionId
//! Renderer   ── renders ──▶ pane snapshots (never owns state)
//! ```
//!
//! The engine is UI-framework agnostic: the desktop or CLI drives it, calls
//! [`Multiplexer::drain_frame`] once per frame, and renders pane snapshots
//! via [`Multiplexer::snapshot_for`].

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Timelike;
use pty::PtyManager;
use terminal_core::{DirtyTracker, RenderSnapshot, TerminalState};
use terminal_session::adaptive::{
    ArtifactInvalidation, AutonomyPolicy, HumanEscalation, PlanVersion, PlannerQualityMetrics,
    ProposedReplan, ReplanLimits, ReplanMetrics, ReplanSeverity, ReplanSignal, ReplanTrigger,
    SignalRegistry, TaskInvalidation, WorkflowEvaluator, WorkflowSnapshot,
};
use terminal_session::agent::{AgentRegistry, AgentRuntime};
use terminal_session::artifacts::{
    ArtifactAccessPolicy, ArtifactLineage, ArtifactMaterializer, ArtifactRetentionPolicy,
    ArtifactSelector, ArtifactStore,
};
use terminal_session::audit::{AuditEvent, AuditEventKind, AuditTrail};
use terminal_session::collaboration::{
    ResultSynthesizer, ReviewAggregation, ReviewAggregator, ReviewPolicy, ReviewReport,
    SynthesisInput, SynthesisResult,
};
use terminal_session::credential::CredentialStore;
use terminal_session::execution::{ExecutionId, ExecutionKind};
use terminal_session::launch::AgentLaunchConfig;
use terminal_session::orchestration::{
    Artifact, RuntimeAgentView, SchedulerCommand, SchedulerView, Task, TaskContext, TaskEvent,
    TaskGraph, TaskGraphError, TaskId, TaskPolicy, TaskScheduler, TaskStatus,
};
use terminal_session::planning::{
    classify_intent, normalize_intent, AgentAvailability, AgentSummary, IntentDisposition,
    PersistedPlanState, PlanEditChange, PlanStepStatus, PlanValidator, PlannerAuditRecord,
    PlannerConfig, PlannerConstraints, PlannerContext, PlannerContextBuilder, PlannerContextInput,
    PlannerError, PlannerEvent, PlannerMetrics, PlannerPhase, PlannerProvider, PlannerRequest,
    PlannerState, PlannerStatus, TaskSummary,
};
use terminal_session::policy::{
    Action, ApprovalError, ApprovalId, AutonomyLevel, FilesystemScope, NetworkPolicy,
    PolicyContext, PolicyDecision, PolicyEngine, PolicyEvaluation,
};
use terminal_session::provider::ProviderRegistry;
use terminal_session::worktrees::{
    git_available, CleanupPolicy, DiffSummary, DirtyPolicy, ExecutionEnvironment, IsolationMode,
    MergeOutcome, WorktreeBudget, WorktreeError, WorktreeInspection, WorktreeManager,
    WorktreeRecord, WorktreeState,
};
use terminal_session::Session;

use crate::events::EventBus;
use crate::layout::{LayoutEngine, PaneRect, Rect};
use crate::model::{
    Pane, PaneId, PersistedState, SplitDirection, Tab, TabId, Workspace, WorkspaceId,
};
use crate::notify::{Notification, NotificationCenter, NotificationKind, NotificationPrefs};
use crate::pane_tree::PaneNode;
use serde::{Deserialize, Serialize};
use terminal_session::agent::AgentSnapshot;
use terminal_session::execution::{AgentEvent, AgentState, ApplicationEvent};
use terminal_session::work::AgentFilter;

/// Default cell geometry for sessions spawned before any layout is known.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Phase 3B: maps an authoritative scheduler event to the task status the
/// plan's step-status view mirrors (§23 — the planner observes the
/// scheduler, it never decides transitions). Artifact events carry no
/// status change.
fn task_status_of_event(event: &TaskEvent) -> Option<TaskStatus> {
    match event {
        TaskEvent::TaskCreated { .. } => Some(TaskStatus::Pending),
        TaskEvent::TaskReady { .. } => Some(TaskStatus::Ready),
        TaskEvent::TaskStarted { .. } => Some(TaskStatus::Running),
        TaskEvent::TaskBlocked { .. } => Some(TaskStatus::Blocked),
        TaskEvent::TaskWaiting { .. } => Some(TaskStatus::Waiting),
        TaskEvent::TaskNeedsReview { .. } => Some(TaskStatus::NeedsReview),
        TaskEvent::TaskCompleted { .. } => Some(TaskStatus::Completed),
        TaskEvent::TaskFailed { .. } => Some(TaskStatus::Failed),
        TaskEvent::TaskCancelled { .. } => Some(TaskStatus::Cancelled),
        TaskEvent::TaskRetrying { .. } => Some(TaskStatus::Ready),
        TaskEvent::TaskInterrupted { .. } => Some(TaskStatus::Interrupted),
        TaskEvent::TaskArtifactCreated { .. } => None,
    }
}

/// Phase 3D: raw-path inputs keep their 3A semantics (resolved by the
/// adapter); structured `artifact://` URIs and plain artifact ids go
/// through the artifact store (§10).
fn is_raw_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("./") || s.starts_with("../")
}

/// Resolves a declared input artifact reference to a store artifact id:
/// plain ids pass through, `artifact://<task>/<id>` URIs are parsed (§10).
fn resolve_artifact_ref(s: &str) -> Option<String> {
    if is_raw_path(s) {
        return None;
    }
    if let Some(r) = terminal_session::artifacts::ArtifactReference::parse(s) {
        return Some(r.artifact_id);
    }
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

/// Phase 2C: parses a snapshot state string back to an `AgentState`
/// (snapshots are serde'd as `format!("{:?}", state)`).
fn agent_state_from_str(s: &str) -> AgentState {
    match s {
        "Created" => AgentState::Created,
        "Starting" => AgentState::Starting,
        "Working" => AgentState::Working,
        "Waiting" => AgentState::Waiting,
        "NeedsApproval" => AgentState::NeedsApproval,
        "Blocked" => AgentState::Blocked,
        "Completed" => AgentState::Completed,
        "Failed" => AgentState::Failed,
        "Crashed" => AgentState::Crashed,
        "Stopped" => AgentState::Stopped,
        "Disconnected" => AgentState::Disconnected,
        // Unparseable (defensive): treat as disconnected so counts stay sane.
        _ => AgentState::Disconnected,
    }
}

/// Phase 2C §13: dashboard sort rank — attention first, then running,
/// failed, completed (deterministic tie-break by start time).
fn sort_rank(s: &AgentSnapshot) -> (u8, Option<i64>) {
    let state = agent_state_from_str(&s.state);
    let rank = if terminal_session::work::attention_for(state).is_some() {
        0
    } else if state == AgentState::Completed || state == AgentState::Stopped {
        2
    } else if state == AgentState::Failed
        || state == AgentState::Crashed
        || state == AgentState::Blocked
    {
        1
    } else {
        0
    };
    (rank, s.started_at_ms)
}

/// Per-frame fairness budgets (§27): background panes are drained with a cap
/// so a flood in one pane cannot starve the focused pane or others.
const VISIBLE_DRAIN_CAP: usize = 4096;
const BACKGROUND_DRAIN_CAP: usize = 512;

/// Rolling metrics (state events/s and apply latency p95, §22).
#[derive(Debug, Clone, Default)]
pub struct EngineMetrics {
    /// Total events applied since engine start.
    pub events_applied: u64,
    /// (events, apply_ns, at) per recent drain frame.
    samples: VecDeque<(u64, u64, Instant)>,
}

impl EngineMetrics {
    const WINDOW: Duration = Duration::from_secs(2);
    const MAX_SAMPLES: usize = 240;

    fn record(&mut self, events: u64, apply_ns: u64) {
        self.events_applied += events;
        self.samples.push_back((events, apply_ns, Instant::now()));
        while self.samples.len() > Self::MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// Events applied per second over the last 2 s window.
    pub fn events_per_second(&self) -> f64 {
        let now = Instant::now();
        let total: u64 = self
            .samples
            .iter()
            .filter(|(_, _, at)| now.duration_since(*at) <= Self::WINDOW)
            .map(|(e, _, _)| *e)
            .sum();
        total as f64 / Self::WINDOW.as_secs_f64()
    }

    /// p95 apply-latency (µs) per frame over the retained samples.
    pub fn apply_latency_p95_us(&self) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut lats: Vec<u64> = self.samples.iter().map(|(_, ns, _)| *ns).collect();
        lats.sort_unstable();
        let idx = ((lats.len() as f64 * 0.95) as usize).min(lats.len() - 1);
        lats[idx] as f64 / 1e3
    }
}

/// One frame of drain results (used by the desktop to decide redraws).
#[derive(Debug, Clone, Copy, Default)]
pub struct DrainResult {
    pub changed: bool,
    pub events_applied: usize,
}

/// One pane's immutable render data for a single application frame (§12,
/// §28). Produced by [`Multiplexer::pane_frames`] in a single borrow so the
/// desktop can build one GPU frame over all panes without touching the
/// engine again.
#[derive(Debug)]
pub struct PaneFrame<'a> {
    pub pane_id: PaneId,
    pub snapshot: RenderSnapshot<'a>,
    pub dirty: DirtyTracker,
    /// Top-left of the pane viewport in window pixels.
    pub origin: (f32, f32),
}

// ---------------------------------------------------------------------------
// Phase 2C: dashboard / summary / review (§12–§14, §9)
// ---------------------------------------------------------------------------

/// One dashboard row (§13): the pane owning the agent (when still alive)
/// plus the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRow {
    pub pane_id: Option<PaneId>,
    pub snapshot: AgentSnapshot,
}

/// Global agent dashboard (§13) with attention counts (§12).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDashboard {
    pub total: usize,
    pub running: usize,
    pub needs_you: usize,
    pub failed: usize,
    pub completed: usize,
    pub rows: Vec<AgentRow>,
}

/// Lightweight per-workspace agent summary (§14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceAgentSummary {
    pub agents: usize,
    pub running: usize,
    pub needs_you: usize,
    pub completed: usize,
    pub failed: usize,
}

/// One changed file with an optional best-effort git diff (§9).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentFileChange {
    pub path: String,
    pub diff: Option<String>,
}

/// The review surface (§9–§10): files (+diff) and commands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentReview {
    pub files: Vec<AgentFileChange>,
    pub commands: Vec<String>,
}

pub struct Multiplexer {
    workspaces: Vec<Workspace>,
    active_workspace: usize,
    pty: Arc<PtyManager>,
    /// Terminal sessions keyed by ExecutionId (agents and shells share this
    /// map: an agent pane's PTY session flows through the same drain path).
    terminal_sessions: HashMap<ExecutionId, Arc<Session>>,
    /// Terminal states keyed by ExecutionId.
    terminal_states: HashMap<ExecutionId, TerminalState>,
    /// Agent runtime for managing agent sessions.
    agent_runtime: AgentRuntime,
    /// Phase 3A: deterministic task orchestration engine (§10–§12, §43).
    tasks: TaskScheduler,
    /// Phase 3B: planner state machine (3b.md §17–§26). The LLM is a
    /// *planner* — it proposes; only the deterministic validator, compiler
    /// and scheduler execute (§2, §33).
    planner: PlannerState,
    /// Planner configuration (provider/model/profile/approval — §8, §35).
    planner_config: PlannerConfig,
    /// The planner provider boundary (§7). Tests inject deterministic
    /// mocks; no real LLM is required in standard CI (§47).
    planner_provider: Option<Box<dyn PlannerProvider>>,
    /// Task ids the current plan owns in the scheduler (execute/resume
    /// bookkeeping — resume replaces the plan's own tasks, never others).
    plan_task_ids: Vec<TaskId>,
    /// Phase 3C: worktree isolation manager (3c.md §5) — the only git
    /// caller for worktree operations. The planner never touches git; the
    /// scheduler never creates worktrees (§44–§45).
    worktrees: WorktreeManager,
    /// Phase 3D: authoritative artifact store (3d.md §3) — bounded payloads,
    /// lineage, access policy, cross-worktree materialization. Large
    /// payloads never ride the event bus (§27).
    artifacts: ArtifactStore,
    /// Phase 3D §44: structured replanning signals — never an autonomous
    /// replan; surfaced for a human decision.
    replan_signals: Vec<(String, String, u64)>, // (cause, detail, at_ms) — legacy surface
    /// Dedup keys for auto-detected replan signals (`cause:task_id`).
    replan_emitted: std::collections::HashSet<String>,
    /// Phase 3E: adaptive orchestration state (§4–§37).
    adaptive: AdaptiveState,
    /// Phase 3D §18–§20: review reports per reviewed task (independent
    /// reviewers — no reviewer can modify another's result, §19).
    review_reports: HashMap<TaskId, Vec<ReviewReport>>,
    layout: LayoutEngine,
    pub notifications: NotificationCenter,
    pub events: EventBus,
    pub metrics: EngineMetrics,
    pub session_exit_codes: HashMap<ExecutionId, Option<i32>>,
    /// Phase 2C §21: attention states already notified per agent (one
    /// notification per state change — never per output/transition).
    notified_attention: HashMap<ExecutionId, Vec<AgentState>>,
    /// Fired (from reader threads) whenever a session enqueues a batch — the
    /// desktop uses it to wake the UI loop on PTY output.
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// True until the first frame has been rendered; the first frame marks
    /// every pane all-dirty so fresh grids are drawn once instead of blank.
    first_render: bool,
    /// Phase 3F §33: PAUSE ALL gate — blocks new work (task spawns, manual
    /// agent spawns, replans) while preserving current state. Running PTY
    /// processes keep running (an honest pause mechanism for arbitrary
    /// child processes does not exist here; §33 "where technically
    /// possible").
    workflow_paused: bool,
    /// Phase 4 §1–§16: central policy engine — deterministic risk
    /// classification, dangerous-command protection, filesystem scope,
    /// network/secret/budget policies, autonomy levels, approval store.
    policy: PolicyEngine,
    /// Phase 4 §17–§19: first-class audit trail (bounded, redacted,
    /// persisted with the session state).
    audit: AuditTrail,
}

/// Phase 3F §32: report of a STOP ALL action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopAllReport {
    /// Agent sessions terminated (STOP ALL stopped their process groups).
    pub agents_stopped: usize,
    /// Tasks cancelled (running + pending execution).
    pub tasks_stopped: usize,
    /// Tasks that were awaiting a human decision and were preserved.
    pub preserved_decisions: usize,
}

/// Phase 3F §34: live workflow state summary — answers "what is running,
/// what needs me, what did it cost" without opening any panel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowSummary {
    /// Distinct workspaces that own at least one task.
    pub workflows: usize,
    pub running: usize,
    pub waiting: usize,
    /// Tasks in NeedsReview + agents in NeedsApproval + open replans.
    pub needs_approval: usize,
    /// Tasks completed since local midnight.
    pub completed_today: usize,
    pub failed: usize,
    /// Estimated cost in cents (completed task results + running agents).
    pub estimated_cost_cents: u64,
    pub paused: bool,
}

/// Phase 3F §31: everything that currently needs a human decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItems {
    /// Agents waiting on a permission decision.
    pub agents: Vec<AttentionAgent>,
    /// Completed tasks gated on review.
    pub review_tasks: Vec<AttentionTask>,
    /// Open replan proposals awaiting approval/rejection.
    pub replans: Vec<AttentionReplan>,
    /// `agents + review_tasks + replans` — the "NEEDS YOU" badge count.
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionAgent {
    pub execution_id: ExecutionId,
    pub display_name: String,
    pub state: String,
    pub attention: Option<String>,
    pub estimated_cost_cents: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionTask {
    pub task_id: TaskId,
    pub title: String,
    pub state: String,
    pub estimated_cost_cents: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionReplan {
    pub replan_id: String,
    pub workflow_id: String,
    pub reason: String,
    pub version: Option<u32>,
    pub estimated_cost_cents: Option<u64>,
}

/// Phase 3E adaptive orchestration state (3e.md §4–§37). Owns the
/// deterministic replanning half of the engine: signals, plan versions,
/// diffs, invalidations, escalation, limits, policy and metrics. The
/// planner only ever proposes; this state + the human approval boundary
/// decide what actually changes (§2).
#[derive(Debug, Default)]
struct AdaptiveState {
    /// Formal replan signals (§4) — deduplicated + cooldown-gated.
    signals: Vec<ReplanSignal>,
    /// Dedup/cooldown registry (§8–§9).
    registry: SignalRegistry,
    /// Immutable plan versions (v1 → v2 → v3, §13).
    plan_versions: Vec<PlanVersion>,
    /// Open replan proposals awaiting human decision (§12, §15).
    proposals: Vec<ProposedReplan>,
    /// Task invalidations (§19).
    task_invalidations: Vec<TaskInvalidation>,
    /// Artifact invalidations (§20) — old records preserved for lineage.
    artifact_invalidations: Vec<ArtifactInvalidation>,
    /// Human escalations (§33).
    escalations: Vec<HumanEscalation>,
    /// Autonomy policy (§34–§37). Automatic is disabled in Phase 3E.
    autonomy: AutonomyPolicy,
    /// Workflow-level replan limits (§9, §31).
    limits: ReplanLimits,
    /// Replan metrics (§24).
    metrics: ReplanMetrics,
    /// Planner-quality metrics (§25) — tracked separately.
    quality: PlannerQualityMetrics,
    /// Whether the replan limit was reached (§32) — workflow enters
    /// Blocked/human-intervention state.
    limit_reached: bool,
}

impl Multiplexer {
    pub fn new() -> Result<Self> {
        Self::with_wake(None)
    }

    /// Like [`Multiplexer::new`], but `wake` fires whenever any session's
    /// reader thread enqueues data (event-driven redraw, Phase 0.5.1 §2).
    pub fn with_wake(wake: Option<Box<dyn Fn() + Send + Sync>>) -> Result<Self> {
        let pty = Arc::new(PtyManager::new().context("init PTY")?);
        let registry = Arc::new(AgentRegistry::new());
        let wake_arc = wake.map(|w| Arc::from(w) as Arc<dyn Fn() + Send + Sync>);
        let agent_runtime = AgentRuntime::new(
            registry,
            ProviderRegistry::new(),
            Arc::clone(&pty),
            CredentialStore::default(),
            wake_arc.clone(),
        );
        Ok(Self {
            workspaces: Vec::new(),
            active_workspace: 0,
            pty,
            terminal_sessions: HashMap::new(),
            terminal_states: HashMap::new(),
            agent_runtime,
            tasks: TaskScheduler::new(TaskGraph::new(), TaskPolicy::default()),
            planner: PlannerState::new(),
            planner_config: PlannerConfig::default(),
            planner_provider: None,
            plan_task_ids: Vec::new(),
            worktrees: WorktreeManager::new(),
            artifacts: ArtifactStore::new(),
            replan_signals: Vec::new(),
            replan_emitted: std::collections::HashSet::new(),
            adaptive: AdaptiveState::default(),
            review_reports: HashMap::new(),
            layout: LayoutEngine::new(),
            notifications: NotificationCenter::new(),
            events: EventBus::new(),
            metrics: EngineMetrics::default(),
            session_exit_codes: HashMap::new(),
            notified_attention: HashMap::new(),
            wake: wake_arc,
            first_render: true,
            workflow_paused: false,
            policy: PolicyEngine::default(),
            audit: AuditTrail::new(),
        })
    }

    // ------------------------------------------------------------------
    // Workspace operations
    // ------------------------------------------------------------------

    /// Creates a workspace with one tab containing one terminal pane.
    pub fn create_workspace(&mut self, name: &str, project_root: &str) -> Result<WorkspaceId> {
        let mut ws = Workspace::new(name, project_root);
        let tab = self.new_tab_in(&ws.project_root, &ws.id)?;
        ws.active_tab = Some(tab.id.clone());
        ws.tabs.push(tab);
        let id = ws.id.clone();
        self.workspaces.push(ws);
        self.active_workspace = self.workspaces.len() - 1;
        Ok(id)
    }

    pub fn rename_workspace(&mut self, id: &WorkspaceId, name: &str) -> Result<()> {
        self.workspace_mut(id)?.name = name.to_string();
        Ok(())
    }

    /// Closes a workspace, terminating every session in it. The last
    /// workspace cannot be closed (fail-safe, §36).
    pub fn close_workspace(&mut self, id: &WorkspaceId) -> Result<()> {
        if self.workspaces.len() <= 1 {
            bail!("cannot close the last workspace");
        }
        let idx = self
            .workspaces
            .iter()
            .position(|w| &w.id == id)
            .context("workspace not found")?;
        let ws = self.workspaces.remove(idx);
        self.terminate_workspace(&ws);
        if self.active_workspace >= self.workspaces.len() {
            self.active_workspace = self.workspaces.len() - 1;
        }
        Ok(())
    }

    pub fn switch_workspace(&mut self, id: &WorkspaceId) -> Result<()> {
        let idx = self
            .workspaces
            .iter()
            .position(|w| &w.id == id)
            .context("workspace not found")?;
        self.active_workspace = idx;
        Ok(())
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Creates a default workspace if none exist. The engine invariant is
    /// `workspaces` is never empty, so `active_workspace` is always safe
    /// after this call (or after any `create_workspace`).
    pub fn ensure_workspace(&mut self) -> WorkspaceId {
        if self.workspaces.is_empty() {
            if let Ok(id) = self.create_workspace("Default", "/") {
                return id;
            }
        }
        self.workspaces[0].id.clone()
    }

    pub fn active_workspace(&self) -> &Workspace {
        self.workspaces
            .get(self.active_workspace)
            .or_else(|| self.workspaces.last())
            .expect("engine invariant: at least one workspace")
    }

    pub fn active_workspace_mut(&mut self) -> &mut Workspace {
        let idx = if self.workspaces.is_empty() {
            self.ensure_workspace();
            0
        } else {
            self.active_workspace.min(self.workspaces.len() - 1)
        };
        &mut self.workspaces[idx]
    }

    pub fn workspace(&self, id: &WorkspaceId) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| &w.id == id)
    }

    fn workspace_mut(&mut self, id: &WorkspaceId) -> Result<&mut Workspace> {
        self.workspaces
            .iter_mut()
            .find(|w| &w.id == id)
            .context("workspace not found")
    }

    fn terminate_workspace(&mut self, ws: &Workspace) {
        let ids: Vec<(String, ExecutionId)> = ws
            .tabs
            .iter()
            .flat_map(|t| {
                let mut v = Vec::new();
                t.root.panes(&mut v);
                v.into_iter()
                    .map(|p| (p.id.clone(), p.execution_id.clone()))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (pane_id, eid) in ids {
            self.events
                .publish(ApplicationEvent::PaneClosed { pane_id });
            self.terminal_sessions.remove(&eid);
            self.terminal_states.remove(&eid);
            self.agent_runtime.remove(&eid);
        }
    }

    // ------------------------------------------------------------------
    // Tab operations
    // ------------------------------------------------------------------

    fn new_tab_in(&mut self, project_root: &str, ws_id: &str) -> Result<Tab> {
        let eid =
            self.spawn_terminal_session(project_root.to_string(), DEFAULT_COLS, DEFAULT_ROWS)?;
        let pane = Pane::new(
            ExecutionKind::Terminal,
            eid.clone(),
            project_root.to_string(),
        );
        self.events.publish(ApplicationEvent::PaneCreated {
            pane_id: pane.id.clone(),
            execution_id: eid.clone(),
        });
        let root = PaneNode::leaf(pane);
        let active = root.pane_id().cloned();
        let mut tab = Tab::new(ws_id, root);
        tab.active_pane = active;
        Ok(tab)
    }

    /// Creates a new tab in the active workspace.
    pub fn new_tab(&mut self) -> Result<TabId> {
        let (root_cwd, ws_id) = {
            let ws = self.active_workspace_mut();
            (ws.project_root.clone(), ws.id.clone())
        };
        let tab = self.new_tab_in(&root_cwd, &ws_id)?;
        let id = tab.id.clone();
        let ws = self.active_workspace_mut();
        ws.tabs.push(tab);
        ws.active_tab = Some(id.clone());
        Ok(id)
    }

    fn ensure_active_tab(&mut self) -> Result<()> {
        let has = self
            .active_workspace()
            .active_tab
            .as_ref()
            .map(|id| self.active_workspace().tabs.iter().any(|t| &t.id == id))
            .unwrap_or(false);
        if has {
            return Ok(());
        }
        let id = self.new_tab()?;
        self.switch_tab(&id)
    }

    fn active_tab_mut(&mut self) -> Result<&mut Tab> {
        self.ensure_active_tab()?;
        let ws = self.active_workspace_mut();
        let id = ws.active_tab.clone().context("no active tab")?;
        ws.tabs
            .iter_mut()
            .find(|t| t.id == id)
            .context("active tab missing")
    }

    /// Closes a tab (terminating its sessions). Closing the last tab leaves
    /// an empty workspace (fail-safe, §36).
    pub fn close_tab(&mut self, tab_id: &str) -> Result<()> {
        let (removed, keep_last) = {
            let ws = self.active_workspace_mut();
            let idx = ws
                .tabs
                .iter()
                .position(|t| t.id == tab_id)
                .context("tab not found")?;
            (ws.tabs.remove(idx), !ws.tabs.is_empty())
        };
        let ids: Vec<(String, ExecutionId)> = {
            let mut v = Vec::new();
            removed.root.panes(&mut v);
            v.into_iter()
                .map(|p| (p.id.clone(), p.execution_id.clone()))
                .collect()
        };
        for (pane_id, eid) in ids {
            self.events
                .publish(ApplicationEvent::PaneClosed { pane_id });
            self.terminal_sessions.remove(&eid);
            self.terminal_states.remove(&eid);
            self.agent_runtime.remove(&eid);
        }
        let ws = self.active_workspace_mut();
        if ws.active_tab.as_deref() == Some(tab_id) {
            ws.active_tab = if keep_last {
                ws.tabs.last().map(|t| t.id.clone())
            } else {
                None
            };
        }
        Ok(())
    }

    pub fn switch_tab(&mut self, tab_id: &str) -> Result<()> {
        let ws = self.active_workspace_mut();
        if !ws.tabs.iter().any(|t| t.id == tab_id) {
            bail!("tab not found");
        }
        ws.active_tab = Some(tab_id.to_string());
        Ok(())
    }

    /// Reorders a tab to a new position in the active workspace (§35: tab
    /// reorder). Returns an error if the tab or destination index is invalid.
    pub fn reorder_tab(&mut self, tab_id: &str, to_index: usize) -> Result<()> {
        let ws = self.active_workspace_mut();
        let Some(from) = ws.tabs.iter().position(|t| t.id == tab_id) else {
            bail!("tab not found");
        };
        let tab = ws.tabs.remove(from);
        let to_index = to_index.min(ws.tabs.len());
        ws.tabs.insert(to_index, tab);
        Ok(())
    }

    pub fn next_tab(&mut self) -> Result<()> {
        let ws = self.active_workspace();
        let n = ws.tabs.len();
        if n == 0 {
            return Ok(());
        }
        let cur = ws
            .active_tab
            .as_ref()
            .and_then(|id| ws.tabs.iter().position(|t| &t.id == id))
            .unwrap_or(n - 1);
        let id = ws.tabs[(cur + 1) % n].id.clone();
        self.switch_tab(&id)
    }

    pub fn previous_tab(&mut self) -> Result<()> {
        let ws = self.active_workspace();
        let n = ws.tabs.len();
        if n == 0 {
            return Ok(());
        }
        let cur = ws
            .active_tab
            .as_ref()
            .and_then(|id| ws.tabs.iter().position(|t| &t.id == id))
            .unwrap_or(0);
        let id = ws.tabs[(cur + n - 1) % n].id.clone();
        self.switch_tab(&id)
    }

    // ------------------------------------------------------------------
    // Pane operations
    // ------------------------------------------------------------------

    /// Returns the pane tree of the active tab (if any).
    pub fn active_tree(&self) -> Option<&PaneNode> {
        let ws = self.active_workspace();
        let tab = ws.active_tab()?;
        Some(&tab.root)
    }

    /// Splits the focused pane in `direction`, spawning a new session.
    /// Focus moves to the new pane. Returns its id.
    pub fn split_pane(&mut self, direction: SplitDirection) -> Result<PaneId> {
        let (target, cwd) = {
            let ws = self.active_workspace();
            let Some(tab) = ws.active_tab() else {
                bail!("no active tab");
            };
            let target = tab
                .active_pane
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no focused pane"))?;
            let cwd = tab
                .root
                .find_pane(&target)
                .map(|p| p.cwd.clone())
                .unwrap_or_else(|| ws.project_root.clone());
            (target, cwd)
        };
        let eid = self.spawn_terminal_session(cwd.clone(), DEFAULT_COLS, DEFAULT_ROWS)?;
        let pane = Pane::new(ExecutionKind::Terminal, eid.clone(), cwd);
        let tab = self.active_tab_mut()?;
        let inserted = tab
            .root
            .split_by_id(&target, direction, pane)
            .context("target pane disappeared")?;
        tab.active_pane = Some(inserted.clone());
        self.events.publish(ApplicationEvent::PaneCreated {
            pane_id: inserted.clone(),
            execution_id: eid,
        });
        Ok(inserted)
    }

    /// Splits the focused pane with a new agent session (Phase 2B §24).
    /// The launch config is stored in the pane's metadata so restore can
    /// re-spawn it later (§36).
    pub fn split_pane_agent(
        &mut self,
        direction: SplitDirection,
        launch: AgentLaunchConfig,
    ) -> Result<PaneId> {
        let (target, cwd) = {
            let ws = self.active_workspace();
            let Some(tab) = ws.active_tab() else {
                bail!("no active tab");
            };
            let target = tab
                .active_pane
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no focused pane"))?;
            let cwd = tab
                .root
                .find_pane(&target)
                .map(|p| p.cwd.clone())
                .unwrap_or_else(|| ws.project_root.clone());
            (target, cwd)
        };
        let eid = self.spawn_agent_session(launch.clone(), DEFAULT_COLS, DEFAULT_ROWS)?;
        let mut pane = Pane::new(ExecutionKind::Agent, eid.clone(), cwd);
        // Persist a redacted copy — credentials (or registered secrets in
        // args/env) must never reach state.json or pane snapshots (§28–29).
        let mut stored = launch;
        stored.redact();
        pane.metadata = serde_json::json!({ "agent": { "launch": stored } });
        let tab = self.active_tab_mut()?;
        let inserted = tab
            .root
            .split_by_id(&target, direction, pane)
            .context("target pane disappeared")?;
        tab.active_pane = Some(inserted.clone());
        self.events.publish(ApplicationEvent::PaneCreated {
            pane_id: inserted.clone(),
            execution_id: eid,
        });
        Ok(inserted)
    }

    /// Closes a pane (terminating its session). Closing the last pane of a
    /// tab closes the tab.
    pub fn close_pane(&mut self, pane_id: &PaneId) -> Result<()> {
        let (last, tab_id) = {
            let tab = self.active_tab_mut()?;
            (tab.root.pane_count() <= 1, tab.id.clone())
        };
        if last {
            return self.close_tab(&tab_id);
        }
        let removed = {
            let tab = self.active_tab_mut()?;
            tab.root.remove_pane(pane_id).context("pane not found")?
        };
        self.events.publish(ApplicationEvent::PaneClosed {
            pane_id: removed.id.clone(),
        });
        self.terminal_sessions.remove(&removed.execution_id);
        self.terminal_states.remove(&removed.execution_id);
        self.agent_runtime.remove(&removed.execution_id);
        let tab = self.active_tab_mut()?;
        if tab.active_pane.as_deref() == Some(pane_id) {
            tab.active_pane = tab.root.pane_id().cloned();
        }
        Ok(())
    }

    pub fn focus_pane(&mut self, pane_id: &PaneId) -> Result<()> {
        let tab = self.active_tab_mut()?;
        if tab.root.find_pane(pane_id).is_none() {
            bail!("pane not found");
        }
        tab.active_pane = Some(pane_id.clone());
        Ok(())
    }

    fn focus_step(&mut self, dir: i32) -> Result<()> {
        let tab = self.active_tab_mut()?;
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        if panes.len() <= 1 {
            return Ok(());
        }
        let cur = tab
            .active_pane
            .as_ref()
            .and_then(|id| panes.iter().position(|p| &p.id == id))
            .unwrap_or(0);
        let next = ((cur as i32 + dir).rem_euclid(panes.len() as i32)) as usize;
        tab.active_pane = Some(panes[next].id.clone());
        Ok(())
    }

    pub fn focus_next(&mut self) -> Result<()> {
        self.focus_step(1)
    }

    pub fn focus_previous(&mut self) -> Result<()> {
        self.focus_step(-1)
    }

    pub fn zoom_pane(&mut self, pane_id: &PaneId) -> Result<()> {
        let tab = self.active_tab_mut()?;
        if tab.root.find_pane(pane_id).is_none() {
            bail!("pane not found");
        }
        let cur = tab
            .metadata
            .get("zoom")
            .and_then(|v| v.as_str().map(String::from));
        if cur.as_deref() == Some(pane_id) {
            tab.metadata = serde_json::json!({});
        } else {
            tab.metadata = serde_json::json!({ "zoom": pane_id });
        }
        Ok(())
    }

    pub fn swap_panes(&mut self, a: &PaneId, b: &PaneId) -> Result<()> {
        let tab = self.active_tab_mut()?;
        if !tab.root.swap_panes(a, b) {
            bail!("pane(s) not found");
        }
        Ok(())
    }

    pub fn move_pane(&mut self, pane_id: &PaneId, forward: bool) -> Result<()> {
        let tab = self.active_tab_mut()?;
        if !tab.root.move_pane(pane_id, forward) {
            bail!("pane not movable");
        }
        Ok(())
    }

    pub fn resize_pane(&mut self, pane_id: &PaneId, delta_px: f32) -> Result<()> {
        let tab = self.active_tab_mut()?;
        if !crate::layout::LayoutEngine::new().resize_pane(&mut tab.root, pane_id, delta_px) {
            bail!("pane not found");
        }
        Ok(())
    }

    /// Layout of the active tab's panes inside `outer` (respects zoom).
    pub fn layout_active(&self, outer: Rect) -> Vec<PaneRect> {
        let Some(root) = self.active_tree() else {
            return Vec::new();
        };
        let zoom = self.active_workspace().active_tab().and_then(|t| {
            t.metadata
                .get("zoom")
                .and_then(|v| v.as_str())
                .map(String::from)
        });
        self.layout.layout(root, outer, zoom.as_ref())
    }

    /// Produces one [`PaneFrame`] per pane in `rects` order, snapshotting
    /// each state and *consuming* its dirty tracker (the renderer must only
    /// ever see the frame's tracker). The snapshots borrow this engine, so
    /// this must be the last engine interaction of the frame. The first
    /// frame marks every pane all-dirty so nothing renders blank.
    pub fn pane_frames<'a>(&'a mut self, rects: &[(PaneId, Rect)]) -> Vec<PaneFrame<'a>> {
        // Resolve each rect to its execution id (owned — no borrow lingers).
        let wanted: Vec<(ExecutionId, PaneId, (f32, f32))> = rects
            .iter()
            .filter_map(|(pid, rect)| {
                let eid = self.execution_id_for_pane(pid)?;
                Some((eid, pid.clone(), (rect.x as f32, rect.y as f32)))
            })
            .collect();
        let want: std::collections::HashSet<String> =
            wanted.iter().map(|(e, _, _)| e.0.clone()).collect();
        // One mutable pass over the state map: consume dirty + snapshot.
        let mut by_eid: HashMap<ExecutionId, (RenderSnapshot<'a>, DirtyTracker)> = HashMap::new();
        for (eid, st) in self.terminal_states.iter_mut() {
            if want.contains(&eid.0) {
                if self.first_render {
                    st.mark_all_dirty();
                }
                let dirty = st.consume_dirty();
                let snapshot = st.snapshot();
                by_eid.insert(eid.clone(), (snapshot, dirty));
            }
        }
        self.first_render = false;
        // Assemble in rects order (frames carry their own origins).
        wanted
            .into_iter()
            .filter_map(|(eid, pane_id, origin)| {
                let (snapshot, dirty) = by_eid.remove(&eid)?;
                Some(PaneFrame {
                    pane_id,
                    snapshot,
                    dirty,
                    origin,
                })
            })
            .collect()
    }

    /// The focused pane id of the active tab.
    pub fn focused_pane(&self) -> Option<PaneId> {
        self.active_workspace().active_tab()?.active_pane.clone()
    }

    /// The active tab id.
    pub fn active_tab_id(&self) -> Option<String> {
        self.active_workspace().active_tab.clone()
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.active_workspace().active_tab()
    }

    /// The terminal session for a pane (for input routing, §9). Agent
    /// panes are interactive too — their PTY session lives in the same map.
    pub fn terminal_session_for_pane(&self, pane_id: &PaneId) -> Option<&Session> {
        let ws = self.active_workspace();
        let tab = ws.active_tab()?;
        let pane = tab.root.find_pane(pane_id)?;
        match pane.execution_kind {
            ExecutionKind::Terminal | ExecutionKind::Agent => self
                .terminal_sessions
                .get(&pane.execution_id)
                .map(|s| s.as_ref()),
        }
    }

    /// The pane id hosting a given execution (all workspaces).
    pub fn pane_id_for_execution(&self, eid: &ExecutionId) -> Option<PaneId> {
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                if let Some(p) = panes.into_iter().find(|p| &p.execution_id == eid) {
                    return Some(p.id.clone());
                }
            }
        }
        None
    }

    pub fn state_for_pane(&self, pane_id: &PaneId) -> Option<&TerminalState> {
        let eid = self.execution_id_for_pane(pane_id)?;
        self.terminal_states.get(&eid)
    }

    pub fn state_for_pane_mut(&mut self, pane_id: &PaneId) -> Option<&mut TerminalState> {
        let eid = self.execution_id_for_pane(pane_id)?;
        self.terminal_states.get_mut(&eid)
    }

    pub fn execution_id_for_pane(&self, pane_id: &PaneId) -> Option<ExecutionId> {
        let ws = self.active_workspace();
        let tab = ws.active_tab()?;
        tab.root.find_pane(pane_id).map(|p| p.execution_id.clone())
    }

    /// Writes bytes to the focused pane's session (input routing, §9).
    pub fn write_focused(&self, bytes: &[u8]) {
        if let Some(id) = self.focused_pane() {
            if let Some(s) = self.terminal_session_for_pane(&id) {
                s.write(bytes);
            }
        }
    }

    /// Resizes a pane's session + state to a grid size (fast ioctls only —
    /// never blocks the UI thread, §13).
    pub fn resize_pane_grid(&mut self, pane_id: &PaneId, cols: u16, rows: u16) -> Result<()> {
        let eid = self
            .execution_id_for_pane(pane_id)
            .context("pane not found")?;
        if let Some(s) = self.terminal_sessions.get(&eid) {
            let (old_cols, old_rows) = {
                let st = self.terminal_states.get(&eid).expect("state for session");
                (st.cols, st.rows)
            };
            if old_cols != cols || old_rows != rows {
                s.resize(cols, rows);
                if let Some(st) = self.terminal_states.get_mut(&eid) {
                    st.resize(cols, rows);
                }
            }
        }
        Ok(())
    }

    /// Drains all panes with fairness (§27): the focused pane is drained
    /// without cap, other panes in the active tab up to [`VISIBLE_DRAIN_CAP`]
    /// per frame, background panes up to [`BACKGROUND_DRAIN_CAP`]. Events
    /// are applied in batches with a single dirty region per frame (§23,
    /// §28) — no render happens inside this call.
    pub fn drain_frame(&mut self) -> DrainResult {
        let mut changed = false;
        let mut total_events = 0u64;
        let t0 = Instant::now();

        // Classify executions: focused / visible / background.
        let mut focused = None;
        let mut visible: Vec<ExecutionId> = Vec::new();
        {
            let ws = self.active_workspace();
            if let Some(tab) = ws.active_tab() {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                focused = tab.active_pane.clone();
                for p in panes {
                    visible.push(p.execution_id.clone());
                }
            }
        }
        let focused_eid = focused.and_then(|f| self.execution_id_for_pane(&f));

        // Deterministic drain order (§48): HashMap iteration is unstable, and
        // which exit is *observed first* within one frame decides the
        // emission order of same-frame task events (failed vs completed).
        let mut eids: Vec<ExecutionId> = self.terminal_sessions.keys().cloned().collect();
        eids.sort_by(|a, b| a.0.cmp(&b.0));
        for eid in eids {
            let cap = if Some(&eid) == focused_eid.as_ref() {
                usize::MAX
            } else if visible.contains(&eid) {
                VISIBLE_DRAIN_CAP
            } else {
                BACKGROUND_DRAIN_CAP
            };
            let Some(session) = self.terminal_sessions.get(&eid) else {
                continue;
            };
            let mut applied = 0usize;
            let mut raw_events = 0u64;
            let mut frame_changed = false;
            while applied < cap {
                let Some(state) = self.terminal_states.get_mut(&eid) else {
                    break;
                };
                let before = session.pending_len();
                let applied_now = session.drain(state);
                let after = session.pending_len();
                frame_changed |= applied_now;
                if before == after {
                    break;
                }
                // Raw terminal event count (pending drained this call) —
                // `applied` counts drain batches which are ~1/frame.
                raw_events += before.saturating_sub(after) as u64;
                applied += 1;
            }
            total_events += raw_events;
            changed |= frame_changed;

            if session.has_exited() && !self.session_exit_codes.contains_key(&eid) {
                self.session_exit_codes.insert(eid.clone(), None);
                self.events.publish(ApplicationEvent::SessionExited {
                    execution_id: eid.clone(),
                    code: None,
                });
                if let Some(pane_id) = self.pane_id_for_execution(&eid) {
                    self.notifications
                        .emit(Notification::new(NotificationKind::ProcessExited {
                            pane_id,
                            code: None,
                        }));
                }
            }
        }

        // Agent semantic events (activity state, permission prompts,
        // completion/failure) → notifications + event bus. The terminal
        // stream itself was already rendered through the drain path above.
        for (eid, event) in self.agent_runtime.drain_events() {
            changed = true;
            if let AgentEvent::Exited { code } = event {
                if !self.session_exit_codes.contains_key(&eid) {
                    self.session_exit_codes.insert(eid.clone(), code);
                    self.events.publish(ApplicationEvent::SessionExited {
                        execution_id: eid.clone(),
                        code,
                    });
                    if let Some(pane_id) = self.pane_id_for_execution(&eid) {
                        self.notifications.emit(Notification::new(
                            NotificationKind::ProcessExited { pane_id, code },
                        ));
                    }
                }
            }
            // Phase 2C §21: meaningful attention/completion notifications,
            // honoring quiet-mode prefs (§22).
            self.emit_agent_notifications(&eid, &event);
            self.events.publish(ApplicationEvent::AgentEvent {
                execution_id: eid,
                event,
            });
        }
        // Phase 3A: one orchestration pass per frame (§22 — task events are
        // published in transition order; same-task ordering is strict).
        self.step_tasks();
        // Deliver coalesced output to subscribers; disconnect slow clients.
        self.events.flush();

        let elapsed_ns = t0.elapsed().as_nanos() as u64;
        self.metrics.record(total_events, elapsed_ns);
        DrainResult {
            changed,
            events_applied: total_events as usize,
        }
    }

    // ------------------------------------------------------------------
    // Sessions & persistence
    // ------------------------------------------------------------------

    /// Spawns a terminal session + state, registering both, and returns the ExecutionId.
    fn spawn_terminal_session(&mut self, cwd: String, cols: u16, rows: u16) -> Result<ExecutionId> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let cwd_ok = if std::path::Path::new(&cwd).is_dir() {
            cwd
        } else {
            "/".to_string()
        };
        let wake = self
            .wake
            .clone()
            .map(|w| Box::new(move || w()) as Box<dyn Fn() + Send>);
        let (session, _pid) =
            Session::spawn_with_wake(Arc::clone(&self.pty), &shell, &cwd_ok, cols, rows, wake)
                .with_context(|| format!("spawn {shell} in {cwd_ok}"))?;
        let eid = ExecutionId(session.id().to_string());
        let state = TerminalState::new(cols, rows);
        self.terminal_sessions
            .insert(eid.clone(), Arc::new(session));
        self.terminal_states.insert(eid.clone(), state);
        Ok(eid)
    }

    /// Spawns an agent session from a launch config, registering its PTY
    /// session + state so the agent's terminal stream flows through the
    /// same drain path as shells. The agent runtime resolves the binary,
    /// injects credentials, and runs the activity pump; the engine owns the
    /// `Session` copy for rendering.
    pub fn spawn_agent_session(
        &mut self,
        launch: AgentLaunchConfig,
        cols: u16,
        rows: u16,
    ) -> Result<ExecutionId> {
        // Phase 3F §33: PAUSE ALL blocks new work from starting.
        if self.workflow_paused {
            anyhow::bail!("all workflows are paused — resume before spawning agents");
        }
        let (eid, session) = self.agent_runtime.spawn(launch, cols, rows)?;
        let state = TerminalState::new(cols, rows);
        self.terminal_sessions.insert(eid.clone(), session);
        self.terminal_states.insert(eid.clone(), state);
        Ok(eid)
    }

    /// Restarts an agent session with its stored launch config (new PTY,
    /// same ExecutionId). The fresh Session replaces the pane's old one.
    pub fn restart_agent_session(&mut self, execution_id: &ExecutionId) -> Result<()> {
        let (cols, rows) = self
            .terminal_states
            .get(execution_id)
            .map(|st| (st.cols, st.rows))
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        let session = self.agent_runtime.restart(execution_id, cols, rows)?;
        self.terminal_sessions.insert(execution_id.clone(), session);
        Ok(())
    }

    /// Resumes an agent (capability-gated; Claude Code `--resume` in 2B).
    pub fn resume_agent_session(&mut self, execution_id: &ExecutionId) -> Result<()> {
        let (cols, rows) = self
            .terminal_states
            .get(execution_id)
            .map(|st| (st.cols, st.rows))
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        let session = self.agent_runtime.resume(execution_id, cols, rows)?;
        self.terminal_sessions.insert(execution_id.clone(), session);
        Ok(())
    }

    /// Pauses an agent (intentionally unsupported until a real mechanism
    /// exists — no fake capability surfaces).
    pub fn pause_agent_session(&mut self, execution_id: &ExecutionId) -> Result<()> {
        self.agent_runtime.pause(execution_id)
    }

    // ------------------------------------------------------------------
    // Phase 3F §32–§34: global controls + workflow state summary
    // ------------------------------------------------------------------

    /// §33 PAUSE ALL — blocks new work from starting (scheduler spawns,
    /// manual agent spawns, replans) while preserving current state.
    /// Running PTY processes are NOT stopped (honest limitation); agents
    /// mid-work finish or are stopped individually.
    pub fn set_workflow_paused(&mut self, paused: bool) {
        if self.workflow_paused == paused {
            return;
        }
        self.workflow_paused = paused;
        self.events.publish(ApplicationEvent::WorkflowPaused {
            paused,
            workflow_id: self.active_workspace().id.clone(),
        });
        self.audit_kind(
            if paused {
                AuditEventKind::PauseAll
            } else {
                AuditEventKind::WorkflowResumed
            },
            self.active_workspace().id.clone(),
            if paused {
                "PAUSE ALL — new work blocked; running processes continue honestly"
            } else {
                "workflow resumed"
            },
            "user",
        );
    }

    pub fn workflow_paused(&self) -> bool {
        self.workflow_paused
    }

    /// §32 STOP ALL — stops every running agent session (process groups
    /// terminated), cancels running + pending tasks, and preserves state:
    /// records, artifacts, plan versions and human-pending decisions all
    /// survive. Nothing ever auto-resumes.
    pub fn stop_all(&mut self) -> StopAllReport {
        let mut report = StopAllReport::default();
        // 1. Every live agent session (task-bound or manual).
        let eids: Vec<ExecutionId> = self.agent_runtime.execution_ids();
        for eid in eids {
            if self.agent_runtime.stop(&eid).is_ok() {
                report.agents_stopped += 1;
            }
        }
        // 2. Active/pending tasks (human-pending states are preserved).
        let task_ids: Vec<TaskId> = self.tasks.graph().list_task_ids();
        for tid in task_ids {
            let status = self.tasks.graph().get_task(&tid).map(|t| t.status);
            let active = matches!(
                status,
                Some(
                    TaskStatus::Pending
                        | TaskStatus::Ready
                        | TaskStatus::Running
                        | TaskStatus::Waiting
                        | TaskStatus::Blocked
                )
            );
            if active {
                if self.tasks.cancel(&tid).is_ok() {
                    report.tasks_stopped += 1;
                }
            } else if matches!(status, Some(TaskStatus::NeedsReview)) {
                report.preserved_decisions += 1;
            }
        }
        // 3. Open replan proposals stay open (a decision was pending).
        report.preserved_decisions += self.adaptive.proposals.len();
        self.step_tasks();
        self.publish_task_events();
        self.events.publish(ApplicationEvent::WorkflowStopped {
            workflow_id: self.active_workspace().id.clone(),
        });
        self.audit.record_kind(
            AuditEventKind::StopAll,
            self.active_workspace().id.clone(),
            format!(
                "stopped {} agents and {} tasks ({} decisions preserved)",
                report.agents_stopped, report.tasks_stopped, report.preserved_decisions
            ),
            "user",
        );
        report
    }

    // ------------------------------------------------------------------
    // Phase 4: policy engine + audit trail (§1–§19)
    // ------------------------------------------------------------------

    /// Immutable policy state (autonomy, scopes, budgets, approvals).
    pub fn policy_state(&self) -> &PolicyEngine {
        &self.policy
    }

    /// Mutable policy access — restricted to user-facing configuration
    /// (never planner-facing; the planner only proposes).
    pub fn policy_state_mut(&mut self) -> &mut PolicyEngine {
        &mut self.policy
    }

    /// Evaluates an action against policy and records the evaluation in
    /// the audit trail (§2, §17). The caller decides what to do with the
    /// decision (execute / block / request approval).
    pub fn evaluate_action(&mut self, action: &Action, ctx: &PolicyContext) -> PolicyEvaluation {
        let ev = self.policy.evaluate(action, ctx);
        // Specific denied kinds (§17): FilesystemDenied / NetworkDenied /
        // SecretDenied make the audit trail answer "why was this blocked?"
        // without needing to re-derive the category from the action
        // description.
        let kind = match (&ev.decision, action) {
            (PolicyDecision::Deny, Action::Filesystem { .. }) => AuditEventKind::FilesystemDenied,
            (PolicyDecision::Deny, Action::Network { .. }) => AuditEventKind::NetworkDenied,
            (PolicyDecision::Deny, Action::Secret { .. }) => AuditEventKind::SecretDenied,
            (PolicyDecision::Deny, _) => AuditEventKind::ActionDenied,
            (PolicyDecision::Allow, _) => AuditEventKind::ActionAllowed,
            (PolicyDecision::RequireApproval, _) => AuditEventKind::ActionRequiredApproval,
        };
        self.audit.record(
            AuditEvent::new(kind, ctx.workflow_id.clone(), ev.action.clone(), "policy")
                .with_risk(ev.risk)
                .with_task(ctx.task_id.clone().unwrap_or_default())
                .with_agent(ctx.agent_id.clone().unwrap_or_default())
                .with_detail(ev.reasons.join("; "))
                .with_result(match ev.decision {
                    PolicyDecision::Deny => {
                        terminal_session::audit::AuditResult::Denied(ev.reasons.join("; "))
                    }
                    _ => terminal_session::audit::AuditResult::Success,
                }),
        );
        ev
    }

    /// Low-level audit recording (for existing engine paths).
    pub fn audit_kind(
        &mut self,
        kind: AuditEventKind,
        workflow_id: impl Into<String>,
        action: impl Into<String>,
        source: impl Into<String>,
    ) -> String {
        self.audit.record_kind(kind, workflow_id, action, source)
    }

    pub fn audit_trail(&self) -> &AuditTrail {
        &self.audit
    }

    pub fn audit_records(&self) -> &[AuditEvent] {
        self.audit.all()
    }

    /// "Why did FlashTerminal do this?" (§18).
    pub fn audit_explain(&self, id: &str) -> Option<String> {
        self.audit.explain(id)
    }

    pub fn audit_latest(&self, kind: AuditEventKind) -> Option<String> {
        self.audit.explain_latest(kind)
    }

    /// Requests an approval for a RequireApproval decision (§15).
    /// Records ApprovalRequested in the audit trail.
    #[allow(clippy::too_many_arguments)]
    pub fn request_policy_approval(
        &mut self,
        evaluation: &PolicyEvaluation,
        ctx: &PolicyContext,
        action_hash: &str,
    ) -> ApprovalId {
        let id = self.policy.request_approval(
            evaluation,
            ctx,
            action_hash,
            terminal_session::policy::Approval::DEFAULT_TTL_MS,
        );
        self.audit.record(
            AuditEvent::new(
                AuditEventKind::ApprovalRequested,
                ctx.workflow_id.clone(),
                evaluation.action.clone(),
                format!("agent {}", ctx.agent_id.clone().unwrap_or_default()),
            )
            .with_task(ctx.task_id.clone().unwrap_or_default())
            .with_agent(ctx.agent_id.clone().unwrap_or_default())
            .with_risk(evaluation.risk)
            .with_detail(format!("approval {id}; {}", evaluation.reasons.join("; "))),
        );
        id
    }

    /// Grants a pending approval (§15). Records ApprovalGranted.
    pub fn grant_policy_approval(&mut self, id: &str, actor: &str) -> Result<(), ApprovalError> {
        let wf = self
            .policy
            .approvals
            .get(id)
            .map(|a| a.workflow_id.clone())
            .unwrap_or_default();
        self.policy.approvals.grant(id, actor)?;
        if !wf.is_empty() {
            self.audit.record_kind(
                AuditEventKind::ApprovalGranted,
                wf,
                format!("approval {id} granted by {actor}"),
                "user",
            );
        }
        Ok(())
    }

    /// Rejects a pending approval. Records ApprovalRejected.
    pub fn reject_policy_approval(&mut self, id: &str) -> Result<(), ApprovalError> {
        let wf = self
            .policy
            .approvals
            .get(id)
            .map(|a| a.workflow_id.clone())
            .unwrap_or_default();
        self.policy.approvals.reject(id)?;
        if !wf.is_empty() {
            self.audit.record_kind(
                AuditEventKind::ApprovalRejected,
                wf,
                format!("approval {id} rejected"),
                "user",
            );
        }
        Ok(())
    }

    /// Honors a granted approval for execution (§15) — verifies workflow,
    /// agent, freshness and action hash; consumes on success.
    pub fn honor_policy_approval(
        &mut self,
        id: &str,
        workflow_id: &str,
        agent_id: Option<&str>,
        action_hash: &str,
    ) -> Result<(), ApprovalError> {
        self.policy
            .approvals
            .honor(id, workflow_id, agent_id, action_hash)
    }

    /// Pending approvals for a workflow (approval center UX).
    pub fn pending_approvals(&self, workflow_id: &str) -> Vec<terminal_session::policy::Approval> {
        self.policy
            .approvals
            .pending(workflow_id)
            .into_iter()
            .cloned()
            .collect()
    }

    /// Sets the autonomy level (§14). Audited; agents can never change
    /// their own autonomy (the engine only accepts human UI calls).
    pub fn set_autonomy_level(&mut self, level: AutonomyLevel) {
        let prev = self.policy.autonomy;
        if prev == level {
            return;
        }
        self.policy.autonomy = level;
        self.audit_kind(
            AuditEventKind::PolicyEvaluated,
            self.active_workspace().id.clone(),
            format!("autonomy level changed: {:?} → {:?}", prev, level),
            "user",
        );
    }

    pub fn autonomy_level(&self) -> AutonomyLevel {
        self.policy.autonomy
    }

    /// Sets the filesystem scope (§7). Audited.
    pub fn set_filesystem_scope(&mut self, scope: FilesystemScope) {
        let prev = self.policy.filesystem.as_str().to_string();
        self.policy.filesystem = scope;
        self.audit_kind(
            AuditEventKind::PolicyEvaluated,
            self.active_workspace().id.clone(),
            format!(
                "filesystem scope: {prev} → {}",
                self.policy.filesystem.as_str()
            ),
            "user",
        );
    }

    pub fn filesystem_scope(&self) -> &FilesystemScope {
        &self.policy.filesystem
    }

    /// Sets the network policy (§10). The planner cannot change this —
    /// only the user UI path calls here.
    pub fn set_network_policy(&mut self, policy: NetworkPolicy) {
        let prev = self.policy.network.as_str().to_string();
        self.policy.network = policy;
        self.audit_kind(
            AuditEventKind::PolicyEvaluated,
            self.active_workspace().id.clone(),
            format!("network policy: {prev} → {}", self.policy.network.as_str()),
            "user",
        );
    }

    pub fn network_policy(&self) -> &NetworkPolicy {
        &self.policy.network
    }

    /// Grants a secret allowance for a workflow (§11). `human_granted`
    /// must come from the user UI (the planner cannot self-authorize).
    pub fn grant_secret_allowance(
        &mut self,
        workflow_id: &str,
        path_prefix: &str,
        human_granted: bool,
    ) {
        self.policy
            .secrets
            .allowances
            .push(terminal_session::policy::SecretAllowance::new(
                workflow_id,
                path_prefix,
                human_granted,
            ));
        self.audit_kind(
            AuditEventKind::PolicyEvaluated,
            workflow_id,
            format!("secret allowance granted (human={human_granted}): {path_prefix}"),
            "user",
        );
    }

    /// Budget increase (§13): only honored with `authorized=true` (human
    /// approval or a policy-level configuration call). Audited.
    pub fn increase_budget(
        &mut self,
        dimension: terminal_session::policy::BudgetDimension,
        new_cap: u64,
        authorized: bool,
    ) -> Result<(), String> {
        let mut policy = self.policy.budget_policy.clone();
        self.policy
            .budget
            .authorize_increase(&mut policy, dimension, new_cap, authorized)?;
        self.policy.budget_policy = policy;
        self.audit_kind(
            AuditEventKind::BudgetIncreased,
            self.active_workspace().id.clone(),
            format!("budget increased: {} → {new_cap}", dimension.label()),
            if authorized {
                "user"
            } else {
                "attempted-change"
            },
        );
        Ok(())
    }

    pub fn budget_exceeded(&self) -> Vec<(terminal_session::policy::BudgetDimension, u64, u64)> {
        self.policy.budget_exceeded()
    }

    /// Records budget consumption (engine internal + tests).
    pub fn record_budget(&mut self, dim: terminal_session::policy::BudgetDimension, delta: u64) {
        self.policy.record_budget(dim, delta);
    }

    /// §30: revert the workflow to a previous plan version. Safe subset:
    /// the reverted plan becomes a *new proposal* gated behind approval —
    /// filesystem changes are never silently reverted.
    pub fn revert_workflow_to(&mut self, version: u32) -> Result<String, String> {
        let target = self
            .adaptive
            .plan_versions
            .iter()
            .find(|v| v.version == version)
            .cloned()
            .ok_or_else(|| format!("no plan version {version} exists"))?;
        let current = self
            .adaptive
            .plan_versions
            .iter()
            .filter(|v| v.superseded_by.is_none())
            .max_by_key(|v| v.version);
        if current.map(|c| c.version) == Some(version) {
            return Err("already at this plan version".into());
        }
        let next_version = self
            .adaptive
            .plan_versions
            .iter()
            .map(|v| v.version)
            .max()
            .unwrap_or(0)
            + 1;
        let approved = current.map(|c| c.approved).unwrap_or(false);
        let mut reverted = terminal_session::adaptive::PlanVersion::new(
            next_version,
            target.plan.clone(),
            current,
            false,
        );
        reverted.approved = approved;
        reverted.approved_at = Some(terminal_session::planning::now_ms());
        reverted.diff_from_previous = Some(terminal_session::adaptive::PlanDiff {
            added: Vec::new(),
            removed: Vec::new(),
            modified: vec![format!("revert to plan v{version}")],
            changed_agents: Vec::new(),
            changed_dependencies: Vec::new(),
            changed_budget: None,
        });
        // v(N-1) chain: mark the current version superseded by the revert.
        if let Some(c) = self.adaptive.plan_versions.last_mut() {
            c.superseded_by = Some(next_version);
        }
        self.adaptive.plan_versions.push(reverted);
        self.audit_kind(
            AuditEventKind::WorkflowReverted,
            self.active_workspace().id.clone(),
            format!("revert proposed to plan v{version} as v{next_version}"),
            "user",
        );
        // The revert does not touch tasks/graphs by itself — a human
        // approves and executes the proposal via plan_execute.
        Ok(format!("plan-v{next_version}"))
    }

    /// §34: live workflow state summary (counts + estimated cost).
    pub fn workflow_summary(&self) -> WorkflowSummary {
        let mut sum = WorkflowSummary::default();
        let mut ws_with_tasks = std::collections::HashSet::new();
        let now = chrono::Local::now();
        let midnight_ms = terminal_session::planning::now_ms()
            .saturating_sub(now.num_seconds_from_midnight() as u64 * 1000);
        for t in self.tasks.graph().list_tasks() {
            ws_with_tasks.insert(t.workspace_id.clone());
            match t.status {
                TaskStatus::Running => sum.running += 1,
                TaskStatus::Waiting | TaskStatus::Blocked => sum.waiting += 1,
                TaskStatus::NeedsReview => sum.needs_approval += 1,
                TaskStatus::Failed | TaskStatus::Cancelled => sum.failed += 1,
                TaskStatus::Completed => {
                    if t.completed_at_ms.map(|m| m >= midnight_ms).unwrap_or(false) {
                        sum.completed_today += 1;
                    }
                    if let Some(r) = &t.result {
                        sum.estimated_cost_cents = sum
                            .estimated_cost_cents
                            .saturating_add(r.estimated_cost_cents.unwrap_or(0));
                    }
                }
                _ => {}
            }
        }
        sum.workflows = ws_with_tasks.len();
        for snap in self.agent_runtime.list_sessions() {
            if snap.state == "NeedsApproval" {
                sum.needs_approval += 1;
            }
            if let Some(c) = self
                .agent_runtime
                .estimated_cost_cents(&ExecutionId(snap.execution_id))
            {
                sum.estimated_cost_cents = sum.estimated_cost_cents.saturating_add(c);
            }
        }
        sum.needs_approval += self.adaptive.proposals.len();
        // Waiting = task-granularity view; agents mid-work that are neither
        // running tasks nor tasks at all are not double counted.
        sum.paused = self.workflow_paused;
        sum
    }

    /// §31: every object that currently needs a human decision.
    pub fn attention_items(&self) -> AttentionItems {
        let mut items = AttentionItems::default();
        for snap in self.agent_runtime.list_sessions() {
            if snap.state == "NeedsApproval" {
                items.agents.push(AttentionAgent {
                    execution_id: ExecutionId(snap.execution_id.clone()),
                    display_name: snap.display_name.clone(),
                    state: snap.state.clone(),
                    attention: snap.attention.map(|a| match a {
                        terminal_session::work::AttentionReason::PermissionRequested => {
                            "awaiting permission".to_string()
                        }
                        terminal_session::work::AttentionReason::NeedsInput => {
                            "awaiting input".to_string()
                        }
                        terminal_session::work::AttentionReason::ErrorIntervention => {
                            "error — needs intervention".to_string()
                        }
                        terminal_session::work::AttentionReason::Ambiguous => {
                            "ambiguous decision".to_string()
                        }
                    }),
                    estimated_cost_cents: self
                        .agent_runtime
                        .estimated_cost_cents(&ExecutionId(snap.execution_id)),
                });
            }
        }
        for t in self.tasks.graph().list_tasks() {
            if t.status == TaskStatus::NeedsReview {
                items.review_tasks.push(AttentionTask {
                    task_id: t.id.clone(),
                    title: t.title.clone(),
                    state: "Needs review".to_string(),
                    estimated_cost_cents: t.result.as_ref().and_then(|r| r.estimated_cost_cents),
                });
            }
        }
        for p in &self.adaptive.proposals {
            let version = self
                .adaptive
                .plan_versions
                .iter()
                .rev()
                .find(|v| {
                    p.plan
                        .steps
                        .iter()
                        .any(|s| format!("v{}", v.version) == s.id)
                })
                .map(|v| v.version);
            items.replans.push(AttentionReplan {
                replan_id: p.id.clone(),
                workflow_id: p.workflow_id.clone(),
                reason: p.reason.clone(),
                version,
                estimated_cost_cents: p.estimated_cost_cents,
            });
        }
        items.total = items.agents.len() + items.review_tasks.len() + items.replans.len();
        items
    }

    pub fn agent_runtime(&self) -> &AgentRuntime {
        &self.agent_runtime
    }

    pub fn agent_runtime_mut(&mut self) -> &mut AgentRuntime {
        &mut self.agent_runtime
    }

    // ------------------------------------------------------------------
    // Phase 3A: task orchestration (§43) — proxies onto the scheduler
    // ------------------------------------------------------------------

    /// Creates a task in a workspace (3a.md §8 typed errors: unknown agent
    /// definitions, unknown workspaces and unknown dependencies are
    /// rejected here, not at run time).
    pub fn task_create(
        &mut self,
        workspace_id: &WorkspaceId,
        title: &str,
        description: &str,
        assigned_agent: &str,
        dependencies: &[TaskId],
        review_required: bool,
    ) -> Result<TaskId, TaskGraphError> {
        if !self.agent_runtime.definition_exists(assigned_agent) {
            return Err(TaskGraphError::UnknownAgentDefinition(
                assigned_agent.into(),
            ));
        }
        if self.workspace(workspace_id).is_none() {
            return Err(TaskGraphError::UnknownWorkspace(workspace_id.clone()));
        }
        for dep in dependencies {
            if self.tasks.graph().get_task(dep).is_none() {
                return Err(TaskGraphError::UnknownTask(dep.clone()));
            }
        }
        let mut task = Task::new(title, description, assigned_agent, workspace_id);
        task.review_required = review_required;
        let id = task.id.clone();
        self.tasks.graph_mut().add_task(task)?;
        for dep in dependencies {
            self.tasks.graph_mut().add_dependency(&id, dep)?;
        }
        let event = terminal_session::orchestration::TaskEvent::TaskCreated {
            task_id: id.clone(),
            title: title.to_string(),
        };
        self.tasks.emit(event);
        self.publish_task_events();
        Ok(id)
    }

    /// §43 `task.run` — schedules the whole graph (or a single task).
    pub fn task_run(&mut self) {
        self.tasks.submit_all();
        self.step_tasks();
        self.publish_task_events();
    }

    pub fn task_add_dependency(
        &mut self,
        task: &TaskId,
        dep: &TaskId,
    ) -> Result<(), TaskGraphError> {
        self.tasks.graph_mut().add_dependency(task, dep)
    }

    pub fn task_remove_dependency(
        &mut self,
        task: &TaskId,
        dep: &TaskId,
    ) -> Result<(), TaskGraphError> {
        self.tasks.graph_mut().remove_dependency(task, dep)
    }

    pub fn task_cancel(&mut self, task_id: &TaskId) -> Result<(), TaskGraphError> {
        let cmds = self.tasks.cancel(task_id)?;
        self.execute_commands(cmds);
        self.step_tasks();
        self.publish_task_events();
        Ok(())
    }

    /// §43 `task.retry` — re-queues a terminal/blocked task from scratch.
    pub fn task_retry(&mut self, task_id: &TaskId) -> Result<(), TaskGraphError> {
        self.tasks.retry(task_id)?;
        self.step_tasks();
        self.publish_task_events();
        Ok(())
    }

    pub fn task_get(&self, task_id: &TaskId) -> Option<&Task> {
        self.tasks.graph().get_task(task_id)
    }

    /// Determinism trace (task_id, event-kind) in emission order (§48).
    pub fn scheduler_trace(&self) -> &[(TaskId, String)] {
        self.tasks.trace()
    }

    /// Sets a task's launch environment (deterministic fixture knob — the
    /// adapter still builds the vendor instruction).
    pub fn task_set_environment(
        &mut self,
        task_id: &TaskId,
        pairs: &[(String, String)],
    ) -> Result<(), TaskGraphError> {
        let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        task.environment = pairs.to_vec();
        Ok(())
    }

    /// Phase 3D §8/§40: declares an explicit input-artifact grant — the
    /// strongest access grant. The scheduler validates existence before
    /// execution (§9) and the engine materializes it into the task's
    /// worktree (§11).
    pub fn task_add_input_artifact(
        &mut self,
        task_id: &TaskId,
        artifact_ref: &str,
    ) -> Result<(), TaskGraphError> {
        let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        if !task.input_artifacts.iter().any(|a| a == artifact_ref) {
            task.input_artifacts.push(artifact_ref.to_string());
        }
        Ok(())
    }

    /// Appends explicit launch arguments (fixture scenarios such as
    /// `--duration` for bounded long-running agents).
    pub fn task_add_arguments(
        &mut self,
        task_id: &TaskId,
        args: &[&str],
    ) -> Result<(), TaskGraphError> {
        let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        for a in args {
            task.arguments.push(a.to_string());
        }
        Ok(())
    }

    /// Reassigns a task's agent. Registry validation happens at creation
    /// and in `workflow_validate` (an invalid ref is a graph error, §8);
    /// mutation itself is unvalidated so broken workflows stay inspectable.
    pub fn task_set_agent(&mut self, task_id: &TaskId, agent: &str) -> Result<(), TaskGraphError> {
        let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) else {
            return Err(TaskGraphError::UnknownTask(task_id.clone()));
        };
        task.assigned_agent = agent.to_string();
        Ok(())
    }

    pub fn task_list(&self) -> Vec<&Task> {
        self.tasks.graph().list_tasks()
    }

    /// §8 workflow validation (graph structure + engine refs).
    pub fn workflow_validate(&self) -> Vec<TaskGraphError> {
        let mut issues = self.tasks.graph().validate();
        for t in self.tasks.graph().list_tasks() {
            if !self.agent_runtime.definition_exists(&t.assigned_agent) {
                issues.push(TaskGraphError::UnknownAgentDefinition(
                    t.assigned_agent.clone(),
                ));
            }
            if self.workspace(&t.workspace_id).is_none() {
                issues.push(TaskGraphError::UnknownWorkspace(t.workspace_id.clone()));
            }
        }
        issues
    }

    /// §43 `scheduler.status()` — full orchestration snapshot.
    pub fn scheduler_status(&self) -> terminal_session::orchestration::SchedulerStatus {
        self.tasks.status()
    }

    pub fn task_policy(&self) -> TaskPolicy {
        self.tasks.policy().clone()
    }

    /// Sets the authoritative scheduler policy and syncs its Phase 3C
    /// knobs (dirty policy, worktree budget) into the worktree manager —
    /// the TaskPolicy is the single source of truth for both (§10, §14,
    /// §47).
    pub fn set_task_policy(&mut self, policy: TaskPolicy) {
        self.worktrees.set_dirty_policy(policy.dirty);
        let mut budget = self.worktrees.budget().clone();
        budget.max_worktrees = policy.max_worktrees;
        self.worktrees.set_budget(budget);
        self.tasks.set_policy(policy);
    }

    /// §29 human review boundary: approve/reject a NeedsReview task. The
    /// worktree lifecycle follows the review (§21): approval accepts the
    /// result as a valid artifact (merge stays a separate explicit step,
    /// §54); rejection keeps the worktree available for rework (§24).
    pub fn resolve_task_review(
        &mut self,
        task_id: &TaskId,
        approve: bool,
    ) -> Result<(), TaskGraphError> {
        self.tasks.resolve_review(task_id, approve)?;
        let worktree_id = self
            .tasks
            .graph()
            .get_task(task_id)
            .and_then(|t| t.worktree_id.clone());
        if let Some(wt) = worktree_id {
            let state = if approve {
                WorktreeState::Approved
            } else {
                WorktreeState::Rejected
            };
            let _ = self.worktrees.set_state(&wt, state);
        }
        self.step_tasks();
        self.publish_task_events();
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 3C: worktree management (3c.md §5, §30–§32, §44) — the
    // engine's public worktree surface. All git operations stay inside
    // `WorktreeManager`; the engine only mediates task/worktree policy.
    // ------------------------------------------------------------------

    /// Worktree metadata (versioned, secret-free — §35, §49).
    pub fn worktree_list(&self) -> Vec<WorktreeRecord> {
        self.worktrees.list().into_iter().cloned().collect()
    }

    pub fn worktree_get(&self, id: &str) -> Option<WorktreeRecord> {
        self.worktrees.get(id).cloned()
    }

    /// §5 `inspect` — live disk truth (path/branch/HEAD).
    pub fn worktree_inspect(&self, id: &str) -> Result<WorktreeInspection, WorktreeError> {
        self.worktrees.inspect(id)
    }

    /// §18 deterministic diff vs the base revision (secret-free paths).
    pub fn worktree_diff(&self, id: &str) -> Result<DiffSummary, WorktreeError> {
        self.worktrees.diff(id)
    }

    /// §22 explicit merge — only ever from `Approved`; conflicts surface as
    /// [`MergeOutcome::Conflict`] without data loss (§23). Never automatic.
    /// 3e §22: a conflict generates a `ReplanSignal` (MergeConflict) with
    /// evidence — the planner may then propose remediation; automatic
    /// conflict resolution stays disabled.
    pub fn worktree_merge(
        &mut self,
        id: &str,
        target_branch: &str,
    ) -> Result<MergeOutcome, WorktreeError> {
        let outcome = self.worktrees.merge(id, target_branch)?;
        if let MergeOutcome::Conflict(conflict) = &outcome {
            let task_id = self
                .worktrees
                .get(id)
                .and_then(|r| r.task_id.clone())
                .unwrap_or_default();
            let signal = ReplanSignal::new(
                self.active_workspace().id.clone(),
                Some(task_id.clone()),
                ReplanTrigger::MergeConflict,
                ReplanSeverity::Warning,
                format!(
                    "merge conflict in {} on {} files: {}",
                    task_id,
                    conflict.files.len(),
                    conflict.files.join(", ")
                ),
                conflict.files.clone(),
            );
            self.record_replan_signal(signal);
        }
        Ok(outcome)
    }

    /// §30 discard (explicit user action — never automatic).
    pub fn worktree_discard(&mut self, id: &str) -> Result<(), WorktreeError> {
        self.worktrees.remove(id)
    }

    /// §30 configured cleanup policy sweep.
    pub fn worktree_cleanup(&mut self) -> Vec<String> {
        self.worktrees
            .cleanup()
            .into_iter()
            .map(|e| e.to_string())
            .collect()
    }

    /// §31 orphaned worktrees (never auto-deleted — surfaced for review).
    pub fn worktree_orphans(&self) -> Vec<WorktreeRecord> {
        self.worktrees.orphans().into_iter().cloned().collect()
    }

    pub fn worktree_budget(&self) -> WorktreeBudget {
        self.worktrees.budget().clone()
    }

    pub fn set_worktree_budget(&mut self, budget: WorktreeBudget) {
        self.worktrees.set_budget(budget);
    }

    pub fn worktree_dirty_policy(&self) -> DirtyPolicy {
        self.worktrees.dirty_policy()
    }

    pub fn set_worktree_dirty_policy(&mut self, p: DirtyPolicy) {
        self.worktrees.set_dirty_policy(p);
    }

    pub fn worktree_cleanup_policy(&self) -> CleanupPolicy {
        self.worktrees.cleanup_policy()
    }

    pub fn set_worktree_cleanup_policy(&mut self, p: CleanupPolicy) {
        self.worktrees.set_cleanup_policy(p);
    }

    // ------------------------------------------------------------------
    // Phase 3D: collaboration (3d.md §3–§22, §39–§45) — artifacts, review
    // findings, synthesis, replan signals. All data is deterministic,
    // bounded, and secret-free (§35, §38).
    // ------------------------------------------------------------------

    /// §3: all artifacts in the store (metadata — never payloads, §27).
    pub fn artifact_list(&self) -> Vec<terminal_session::artifacts::ArtifactRecord> {
        self.artifacts.metadata_snapshot()
    }

    pub fn artifact_get(&self, id: &str) -> Option<terminal_session::artifacts::ArtifactRecord> {
        self.artifacts.get(id).cloned()
    }

    /// Bounded, redacted payload of an artifact.
    pub fn artifact_payload(&self, id: &str) -> Option<Vec<u8>> {
        self.artifacts.payload(id).map(|p| p.to_vec())
    }

    /// §5/§6: deterministic selection (task, kind, workspace, reference).
    pub fn artifact_select(
        &self,
        selector: &ArtifactSelector,
    ) -> Vec<terminal_session::artifacts::ArtifactRecord> {
        selector
            .select(&self.artifacts, self.tasks.graph())
            .into_iter()
            .cloned()
            .collect()
    }

    /// §5: lineage (producers, consumers, task outputs).
    pub fn artifact_lineage(&self) -> ArtifactLineage {
        ArtifactLineage::build(&self.artifacts, self.tasks.graph())
    }

    /// §37: retention policy (default Keep — never delete work results).
    pub fn artifact_retention(&self) -> ArtifactRetentionPolicy {
        self.artifacts.retention()
    }

    pub fn set_artifact_retention(&mut self, p: ArtifactRetentionPolicy) {
        self.artifacts.set_retention(p);
    }

    /// §18: records a reviewer's report for a task. Findings become
    /// first-class artifacts (§18) and the aggregated consensus is
    /// returned (§20) — deterministic, policy-driven.
    pub fn record_review_report(
        &mut self,
        task_id: &TaskId,
        report: &ReviewReport,
    ) -> ReviewAggregation {
        let policy = ReviewPolicy::default();
        let mut reports: Vec<ReviewReport> = self
            .review_reports
            .get(task_id)
            .cloned()
            .unwrap_or_default();
        for finding in &report.findings {
            let art = terminal_session::orchestration::Artifact {
                id: finding.id.clone(),
                kind: terminal_session::orchestration::ArtifactType::Document,
                path: finding.file.clone(),
                description: format!("review finding ({})", finding.severity.label()),
                created_by_task: finding
                    .created_by_task
                    .clone()
                    .or_else(|| Some(task_id.clone())),
                metadata: vec![
                    ("finding".to_string(), finding.finding.clone()),
                    ("severity".to_string(), finding.severity.label().to_string()),
                ],
                created_by_agent: Some("reviewer".to_string()),
                workspace_id: self
                    .tasks
                    .graph()
                    .get_task(task_id)
                    .map(|t| t.workspace_id.clone()),
                worktree: None,
                revision: None,
                created_at_ms: terminal_session::planning::now_ms(),
            };
            self.artifacts
                .register(art, None, terminal_session::planning::now_ms());
            self.events.publish(ApplicationEvent::ReviewFindingCreated {
                finding: finding.clone(),
            });
        }
        reports.push(report.clone());
        let aggregation = ReviewAggregator::aggregate(&reports, &policy);
        self.review_reports.insert(task_id.clone(), reports);
        aggregation
    }

    /// §20: aggregated consensus for a task (all recorded reports).
    pub fn task_review_consensus(&self, task_id: &TaskId) -> Option<ReviewAggregation> {
        let reports = self.review_reports.get(task_id)?;
        Some(ReviewAggregator::aggregate(
            reports,
            &ReviewPolicy::default(),
        ))
    }

    pub fn review_reports(&self, task_id: &TaskId) -> Vec<ReviewReport> {
        self.review_reports
            .get(task_id)
            .cloned()
            .unwrap_or_default()
    }

    /// §14–§15: deterministic synthesis over explicitly selected task
    /// results + artifacts. Never receives the whole project history, and
    /// never references artifacts it was not given (§54).
    pub fn synthesize(
        &mut self,
        plan_id: Option<String>,
        workflow_id: Option<String>,
        task_ids: &[TaskId],
        artifact_ids: &[String],
    ) -> Result<SynthesisResult, String> {
        let synthesis_id = format!("synthesis:{}", uuid::Uuid::new_v4());
        self.events.publish(ApplicationEvent::SynthesisStarted {
            synthesis_id: synthesis_id.clone(),
            task_ids: task_ids.to_vec(),
        });
        let mut results = Vec::new();
        for id in task_ids {
            let task = self
                .tasks
                .graph()
                .get_task(id)
                .ok_or_else(|| format!("unknown task {id}"))?;
            let r = task
                .result
                .clone()
                .ok_or_else(|| format!("task {id} has no result"))?;
            results.push(r);
        }
        let mut artifacts = Vec::new();
        for id in artifact_ids {
            let a = self
                .artifacts
                .artifact(id)
                .cloned()
                .ok_or_else(|| format!("unknown artifact {id}"))?;
            artifacts.push(a);
        }
        let input = SynthesisInput {
            task_results: results,
            artifacts,
            plan_id,
            workflow_id,
        };
        let provider = self
            .planner_provider_id()
            .map(|p| (p.clone(), self.planner_config.model.clone()));
        let result = ResultSynthesizer::synthesize(
            &input,
            provider.as_ref().map(|(p, m)| (p.as_str(), m.as_str())),
        )?;
        self.events.publish(ApplicationEvent::SynthesisCompleted {
            result: Box::new(result.clone()),
        });
        Ok(result)
    }

    /// §44/3e §4: records a formal replan signal — never an autonomous
    /// replan. Deduplicated + cooldown-gated (§8–§9) and persisted (§42).
    /// `cause`/`detail` remain the legacy surface (`replan_signals`),
    /// mirrored as a ManualUserRequest signal in the formal registry.
    pub fn signal_replan(&mut self, cause: &str, detail: &str) {
        let at = terminal_session::planning::now_ms();
        self.replan_signals
            .push((cause.to_string(), detail.to_string(), at));
        self.replan_emitted.insert(format!("manual:{cause}"));
        let signal = ReplanSignal::new(
            self.active_workspace().id.clone(),
            None,
            ReplanTrigger::ManualUserRequest,
            ReplanSeverity::Info,
            format!("{cause}: {detail}"),
            Vec::new(),
        );
        self.adaptive.metrics.replan_trigger_count += 1;
        self.events.publish(ApplicationEvent::ReplanRequested {
            signal_id: signal.id.clone(),
            workflow_id: self.active_workspace().id.clone(),
            trigger: signal.trigger.as_str().to_string(),
            severity: signal.severity.label().to_string(),
        });
        self.events.publish(ApplicationEvent::WorkflowNeedsReplan {
            workflow_id: self.active_workspace().id.clone(),
            cause: cause.to_string(),
            detail: detail.to_string(),
        });
        self.adaptive.signals.push(signal);
    }

    /// Legacy surface: pending replanning signals (cause, detail, at_ms).
    pub fn replan_signals(&self) -> Vec<(String, String, u64)> {
        self.replan_signals.clone()
    }

    /// Formal signals (§4) — the Phase 3E surface.
    pub fn adaptive_signals(&self) -> Vec<ReplanSignal> {
        self.adaptive.signals.clone()
    }

    /// §44/3e §6: deterministic replan triggers the engine observes from its
    /// own state — task failures, missing artifacts, critical review
    /// findings, merge conflicts, budget risk, test failures. Deduplicated
    /// (§8) + cooldown-gated (§9).
    fn detect_replan_signals(&mut self) {
        let workflow_id = self.active_workspace().id.clone();
        let mut snapshot = WorkflowSnapshot {
            workflow_id: workflow_id.clone(),
            ..Default::default()
        };
        for task in self.tasks.graph().list_tasks() {
            snapshot
                .tasks
                .push(terminal_session::adaptive::TaskObservation {
                    task_id: task.id.clone(),
                    status: task.status,
                    failed: task.status == TaskStatus::Failed,
                    retries: task.attempt_count,
                });
            // Test failures from the agent's observed commands (§7) — a
            // failed task whose work shows a test run is attributed to
            // TestsFailed (deterministic, never a guess).
            if task.status == TaskStatus::Failed {
                if let Some(eid) = &task.agent_execution_id {
                    if let Some(w) = self.agent_runtime.get_work(eid) {
                        let ran_tests = w.commands.iter().any(|c| {
                            let l = c.to_ascii_lowercase();
                            l.contains("test") && !l.contains("dist-test")
                        });
                        if ran_tests {
                            snapshot.test_failures.push((task.id.clone(), 1));
                        }
                    }
                }
            }
            if task.status == TaskStatus::Blocked {
                // Blocked with an artifact error → ArtifactMissing;
                // otherwise a dependency/readiness gap.
                let is_artifact = task
                    .error
                    .as_ref()
                    .map(|e| e.message.contains("artifact"))
                    .unwrap_or(false);
                if is_artifact {
                    snapshot
                        .missing_artifacts
                        .push((task.id.clone(), "input artifact".into()));
                }
            }
            if task.attempt_count >= 3 && task.status == TaskStatus::Failed {
                snapshot
                    .retries_exhausted
                    .push((task.id.clone(), task.attempt_count));
            }
        }
        // Review findings → critical trigger (§27).
        for reports in self.review_reports.values() {
            for r in reports {
                for f in &r.findings {
                    snapshot.findings.push(f.clone());
                }
            }
        }
        // Merge conflicts are surfaced by the worktree merge path (§22) —
        // the evaluator does not re-derive them from records.
        // Budget (§23).
        snapshot.budget = self.budget_observation();
        let evaluator = WorkflowEvaluator;
        for signal in evaluator.evaluate(&snapshot) {
            self.record_replan_signal(signal);
        }
    }

    /// §23: budget observation from the authoritative scheduler policy +
    /// agent runtime estimates.
    fn budget_observation(&self) -> Option<terminal_session::adaptive::BudgetObservation> {
        let budget = self.tasks.policy().max_cost_cents?;
        let mut spent = 0u64;
        let mut remaining = 0u64;
        for t in self.tasks.graph().list_tasks() {
            if let Some(r) = &t.result {
                spent = spent.saturating_add(r.estimated_cost_cents.unwrap_or(0));
            }
            if let Some(eid) = &t.agent_execution_id {
                remaining = remaining
                    .saturating_add(self.agent_runtime.estimated_cost_cents(eid).unwrap_or(0));
            }
        }
        Some(terminal_session::adaptive::BudgetObservation {
            spent_cents: spent,
            budget_cents: Some(budget),
            estimated_remaining_cents: Some(remaining),
        })
    }

    /// Records one signal through the dedup/cooldown gate (§8–§9) and
    /// publishes the ReplanRequested event (§39). Manual signals bypass the
    /// cooldown (a human explicitly asked).
    fn record_replan_signal(&mut self, signal: ReplanSignal) {
        let manual = signal.trigger == ReplanTrigger::ManualUserRequest;
        if !manual {
            let cooldown = self.adaptive.limits.replan_cooldown_seconds;
            if !self.adaptive.registry.admit(&signal, cooldown) {
                return;
            }
        }
        // §31–§32: loop protection — once the limit is reached, further
        // signals escalate to a human instead of looping.
        if self.adaptive.metrics.replan_count >= self.adaptive.limits.max_replans
            && !self.adaptive.limit_reached
        {
            self.adaptive.limit_reached = true;
            self.escalate_human(
                "replan limit reached",
                format!(
                    "workflow hit its replan limit ({}) — cannot safely continue automatically",
                    self.adaptive.limits.max_replans
                ),
                vec!["replan limit reached".to_string()],
                vec![
                    "increase replan limit".to_string(),
                    "manual intervention".to_string(),
                ],
            );
            return;
        }
        let severity = signal.severity;
        let trigger = signal.trigger;
        let signal_id = signal.id.clone();
        *self
            .adaptive
            .metrics
            .trigger_counts
            .entry(trigger.as_str().to_string())
            .or_insert(0) += 1;
        self.adaptive.metrics.replan_trigger_count += 1;
        // Legacy mirror (Phase 3D surface).
        self.replan_signals.push((
            trigger.as_str().to_string(),
            signal.reason.clone(),
            signal.created_at,
        ));
        self.replan_emitted.insert(signal.dedupe_key());
        self.events.publish(ApplicationEvent::ReplanRequested {
            signal_id: signal_id.clone(),
            workflow_id: self.active_workspace().id.clone(),
            trigger: trigger.as_str().to_string(),
            severity: severity.label().to_string(),
        });
        self.events.publish(ApplicationEvent::WorkflowNeedsReplan {
            workflow_id: self.active_workspace().id.clone(),
            cause: trigger.as_str().to_string(),
            detail: signal.reason.clone(),
        });
        self.adaptive.signals.push(signal);
    }

    /// §33: human escalation — what happened, what was attempted, evidence,
    /// options. Never hides uncertainty.
    fn escalate_human(
        &mut self,
        what_happened: impl Into<String>,
        attempted: impl Into<String>,
        evidence: Vec<String>,
        options: Vec<String>,
    ) {
        let esc = HumanEscalation::new(
            self.active_workspace().id.clone(),
            what_happened,
            vec![attempted.into()],
            evidence,
            options,
        );
        self.events.publish(ApplicationEvent::HumanEscalation {
            escalation_id: esc.id.clone(),
            workflow_id: self.active_workspace().id.clone(),
            reason: esc.what_happened.clone(),
        });
        self.adaptive.escalations.push(esc);
    }

    /// §45/3e §12: human replan — constructs a ReplanContext + formal
    /// replan request (mode=Replan), runs the planner, and records a
    /// `ProposedReplan` + `PlanVersion` + `PlanDiff` (§12–§14). The new
    /// plan goes through the normal pipeline and requires approval (never
    /// silently rewrites a running workflow).
    pub fn replan_workflow(&mut self, goal: &str) -> Result<String, PlannerError> {
        if self.planner_provider.is_none() {
            return Err(PlannerError::NoProvider);
        }
        // Phase 3F §33: PAUSE ALL blocks new work, replans included.
        if self.workflow_paused {
            return Err(PlannerError::NotAllowed {
                reason: "all workflows are paused — resume before replanning".to_string(),
            });
        }
        // Signals addressed by this replan are cleared (§45) — the legacy
        // mirror too, so the 3D surface reflects what the replan handled.
        self.replan_signals.clear();
        self.adaptive.signals.clear();
        // §31: loop protection — no new replans past the limit.
        if self.adaptive.metrics.replan_count >= self.adaptive.limits.max_replans {
            if !self.adaptive.limit_reached {
                self.adaptive.limit_reached = true;
                self.escalate_human(
                    "replan limit reached",
                    "replan requested but the workflow replan limit is exhausted",
                    vec!["replan limit reached".to_string()],
                    vec![
                        "increase replan limit".to_string(),
                        "manual intervention".to_string(),
                    ],
                );
            }
            return Err(PlannerError::NotAllowed {
                reason: "replan limit reached — workflow requires human intervention".to_string(),
            });
        }
        let ws_id = self.active_workspace().id.clone();
        let context = PlannerContextBuilder::new(self.planner_context_input()).build();
        // Fold current task results + artifacts into the (bounded) context
        // so the planner can reference them (§43) without raw payloads.
        let mut summaries = Vec::new();
        for t in self.task_list() {
            if let Some(r) = &t.result {
                summaries.push(format!(
                    "{}: {} ({} files, {} warnings)",
                    t.title,
                    r.summary,
                    r.files_changed.len(),
                    r.warnings.len()
                ));
            }
        }
        let pending_triggers: Vec<ReplanTrigger> =
            self.adaptive.signals.iter().map(|s| s.trigger).collect();
        let replan_context = self.build_replan_context();
        let request = PlannerRequest {
            request_id: format!("replan-{}", terminal_session::planning::now_ms()),
            intent: format!(
                "{goal} | current state: {} | replan context: {}",
                summaries.join("; "),
                replan_context.to_request_fragment()
            ),
            workspace_id: ws_id.clone(),
            context,
            constraints: self.planner_constraints(),
            mode: terminal_session::planning::PlannerRequestMode::Replan,
        };
        self.planner.begin_request(
            &request.request_id,
            &request.intent,
            &self.planner_config.provider,
            &self.planner_config.model,
        )?;
        self.publish_planner_event(PlannerEvent::PlanningStarted {
            request_id: request.request_id.clone(),
            intent: request.intent.clone(),
        });
        let provider = self.planner_provider.as_ref().unwrap();
        let result = provider.generate(&request, &self.planner_config);
        for event in self.planner.on_provider_result(result, 0) {
            self.publish_planner_event(event);
        }
        match self.planner.phase() {
            PlannerPhase::NeedsApproval => self.plan_validate()?,
            PlannerPhase::Failed => {
                self.adaptive.quality.invalid_replan_rate += 1;
                return Err(self.planner.last_error().cloned().unwrap_or(
                    PlannerError::NotAllowed {
                        reason: "replanning failed".to_string(),
                    },
                ));
            }
            _ => {}
        }
        // §12: wrap the planner output into a formal proposal + version.
        let Some(plan) = self.planner.plan().cloned() else {
            return Err(PlannerError::NotAllowed {
                reason: "no plan produced".to_string(),
            });
        };
        let proposal = ProposedReplan::from_plan(
            ws_id.clone(),
            format!("replan requested: {goal}"),
            plan.clone(),
            pending_triggers,
        );
        let version = self.record_plan_version(plan);
        self.adaptive.quality.valid_replan_rate += 1;
        self.events.publish(ApplicationEvent::ReplanProposed {
            replan_id: proposal.id.clone(),
            workflow_id: ws_id.clone(),
            version,
            reason: proposal.reason.clone(),
        });
        self.adaptive.proposals.push(proposal);
        self.adaptive.metrics.replan_count += 1;
        Ok(self.adaptive.proposals.last().unwrap().id.clone())
    }

    /// §13: appends an immutable plan version (v1 → v2 → v3), linking the
    /// previous version as superseded and computing the PlanDiff (§14).
    fn record_plan_version(&mut self, plan: terminal_session::planning::ProposedPlan) -> u32 {
        let next = self.adaptive.plan_versions.len() as u32 + 1;
        let prev = self.adaptive.plan_versions.last().cloned();
        let version = PlanVersion::new(next, plan, prev.as_ref(), false);
        if let Some(mut p) = prev {
            let superseded = p.version;
            p.superseded_by = Some(next);
            if let Some(idx) = self
                .adaptive
                .plan_versions
                .iter()
                .position(|v| v.version == superseded)
            {
                self.adaptive.plan_versions[idx] = p;
            }
            self.events.publish(ApplicationEvent::PlanSuperseded {
                superseded_version: superseded,
                new_version: next,
            });
        }
        self.adaptive.plan_versions.push(version);
        next
    }

    /// §10: bounded replan context for the planner.
    fn build_replan_context(&self) -> terminal_session::adaptive::ReplanContext {
        let wf = WorkflowSnapshot {
            workflow_id: self.active_workspace().id.clone(),
            tasks: self
                .tasks
                .graph()
                .list_tasks()
                .iter()
                .map(|t| terminal_session::adaptive::TaskObservation {
                    task_id: t.id.clone(),
                    status: t.status,
                    failed: t.status == TaskStatus::Failed,
                    retries: t.attempt_count,
                })
                .collect(),
            budget: self.budget_observation(),
            ..Default::default()
        };
        terminal_session::adaptive::ReplanContextBuilder::new().build(&wf)
    }

    // ------------------------------------------------------------------
    // Phase 3E: adaptive orchestration public surface (3e.md §15–§42)
    // ------------------------------------------------------------------

    /// §40/§41: replan proposals awaiting human decision.
    pub fn replan_list(&self) -> Vec<ProposedReplan> {
        self.adaptive.proposals.clone()
    }

    /// §40/§41: inspect one proposal (full plan + diff vs previous).
    pub fn replan_get(&self, id: &str) -> Option<ProposedReplan> {
        self.adaptive.proposals.iter().find(|p| p.id == id).cloned()
    }

    /// §13: immutable plan version history (v1 → v2 → v3).
    pub fn workflow_history(&self) -> Vec<PlanVersion> {
        self.adaptive.plan_versions.clone()
    }

    /// §33: human intervention records (escalations + invalidations).
    pub fn workflow_interventions(&self) -> Vec<HumanEscalation> {
        self.adaptive.escalations.clone()
    }

    /// §19: task invalidations.
    pub fn task_invalidations(&self) -> Vec<TaskInvalidation> {
        self.adaptive.task_invalidations.clone()
    }

    /// §20: artifact invalidations (old records preserved).
    pub fn artifact_invalidations(&self) -> Vec<ArtifactInvalidation> {
        self.adaptive.artifact_invalidations.clone()
    }

    /// §19: explicit task invalidation — requires reason + evidence and is
    /// gated behind human approval (the planner can never silently
    /// invalidate completed work). Returns the invalidation id.
    pub fn invalidate_task(
        &mut self,
        task_id: &TaskId,
        reason: &str,
        evidence: Vec<String>,
        approved: bool,
    ) -> Result<String, String> {
        if self.tasks.graph().get_task(task_id).is_none() {
            return Err(format!("unknown task {task_id}"));
        }
        let inv = TaskInvalidation::new(task_id.clone(), reason, evidence);
        let inv = if approved {
            let mut inv = inv;
            inv.approved = true;
            inv.approved_at = Some(terminal_session::planning::now_ms());
            // Revert the completed task to a state that reflects the
            // invalidation: it can be re-run by a future plan, but its
            // result is no longer trusted. We keep the record + result
            // (auditability) and mark it `Failed` with the reason.
            let _ = self.tasks.graph_mut().get_task_mut(task_id).map(|t| {
                if matches!(t.status, TaskStatus::Completed | TaskStatus::NeedsReview) {
                    let _ = t.transition(TaskStatus::Failed);
                }
                t.error = Some(terminal_session::orchestration::TaskError::new(
                    terminal_session::orchestration::TaskErrorKind::Unknown,
                    terminal_session::orchestration::FailureClass::Unknown,
                    format!("task invalidated: {reason}"),
                ));
            });
            self.events.publish(ApplicationEvent::TaskInvalidated {
                task_id: task_id.clone(),
                reason: reason.to_string(),
            });
            inv
        } else {
            inv
        };
        self.adaptive.task_invalidations.push(inv.clone());
        Ok(inv.task_id)
    }

    /// §20: explicit artifact invalidation. The old artifact record is
    /// **preserved** for lineage — never deleted.
    pub fn invalidate_artifact(
        &mut self,
        artifact_id: &str,
        reason: &str,
        evidence: Vec<String>,
    ) -> Result<(), String> {
        if self.artifacts.get(artifact_id).is_none() {
            return Err(format!("unknown artifact {artifact_id}"));
        }
        let inv = ArtifactInvalidation::new(artifact_id, reason, evidence);
        self.events.publish(ApplicationEvent::ArtifactInvalidated {
            artifact_id: artifact_id.to_string(),
            reason: reason.to_string(),
        });
        self.adaptive.artifact_invalidations.push(inv);
        Ok(())
    }

    /// §15/§16/§17: approve a replan — the ONLY path that applies it.
    /// Completes the approval gate, marks the plan version approved, and
    /// re-validates before the user executes (same pipeline as initial
    /// plans, §21). Completed historical work is preserved (§18) unless
    /// explicitly invalidated (§19).
    pub fn replan_approve(&mut self, id: &str) -> Result<(), PlannerError> {
        let proposal = self
            .adaptive
            .proposals
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| PlannerError::NotAllowed {
                reason: format!("unknown replan {id}"),
            })?;
        // The proposal's plan must already be in the planner (it is —
        // replan_workflow left it awaiting approval). Approve + validate.
        self.plan_approve()?;
        // Record approval on the matching plan version (the latest).
        if let Some(v) = self.adaptive.plan_versions.last_mut() {
            v.approved = true;
            v.approved_at = Some(terminal_session::planning::now_ms());
        }
        self.adaptive.metrics.replan_approval_count += 1;
        self.adaptive.metrics.time_to_replan_ms +=
            proposal.plan.estimated_duration_min.unwrap_or(0) as u64;
        self.adaptive.metrics.additional_cost_cents += proposal.estimated_cost_cents.unwrap_or(0);
        self.events.publish(ApplicationEvent::ReplanApproved {
            replan_id: proposal.id.clone(),
            workflow_id: self.active_workspace().id.clone(),
            version: self.adaptive.plan_versions.len() as u32,
        });
        Ok(())
    }

    /// §15/§28: reject a replan — the original workflow remains intact, no
    /// new tasks execute, nothing is destroyed.
    pub fn replan_reject(&mut self, id: &str, reason: &str) -> Result<(), PlannerError> {
        let idx = self
            .adaptive
            .proposals
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| PlannerError::NotAllowed {
                reason: format!("unknown replan {id}"),
            })?;
        let proposal = self.adaptive.proposals.remove(idx);
        self.adaptive.metrics.replan_rejection_count += 1;
        self.adaptive.quality.human_rejection_rate += 1;
        // The planner's pending proposal is also rejected so its phase is
        // consistent (the graph was never touched).
        let _ = self.planner.reject(reason);
        self.events.publish(ApplicationEvent::ReplanRejected {
            replan_id: proposal.id.clone(),
            workflow_id: self.active_workspace().id.clone(),
            reason: reason.to_string(),
        });
        Ok(())
    }

    /// §16/§29: edit a proposed replan (add/remove task, change agent,
    /// change dependency). The edited plan is re-validated before
    /// execution — never executed without re-validation.
    pub fn replan_edit(
        &mut self,
        id: &str,
        changes: &[PlanEditChange],
    ) -> Result<(), PlannerError> {
        if !self.adaptive.proposals.iter().any(|p| p.id == id) {
            return Err(PlannerError::NotAllowed {
                reason: format!("unknown replan {id}"),
            });
        }
        for change in changes {
            self.planner.edit(change)?;
        }
        self.adaptive.metrics.replan_edit_count += 1;
        self.adaptive.quality.human_edit_rate += 1;
        self.events.publish(ApplicationEvent::ReplanEdited {
            replan_id: id.to_string(),
            workflow_id: self.active_workspace().id.clone(),
            version: self.adaptive.plan_versions.len() as u32,
        });
        // §16: never execute an edited plan without re-validation.
        self.plan_validate()?;
        Ok(())
    }

    /// §31–§32: current replan limits + loop-protection status.
    pub fn replan_limits(&self) -> (ReplanLimits, bool) {
        (self.adaptive.limits.clone(), self.adaptive.limit_reached)
    }

    /// §34–§37: autonomy policy (Automatic disabled in Phase 3E).
    pub fn autonomy_policy(&self) -> AutonomyPolicy {
        self.adaptive.autonomy
    }

    /// §24: replan metrics.
    pub fn replan_metrics(&self) -> ReplanMetrics {
        self.adaptive.metrics.clone()
    }

    /// §25: planner-quality metrics (tracked separately from execution).
    pub fn planner_quality_metrics(&self) -> PlannerQualityMetrics {
        self.adaptive.quality.clone()
    }

    /// §9/§31: configuration (cooldown, max replans, autonomy policy).
    /// The planner cannot raise its own limits (§38) — deterministic config.
    pub fn set_adaptive_policy(&mut self, limits: ReplanLimits, autonomy: AutonomyPolicy) {
        self.adaptive.limits = limits;
        self.adaptive.autonomy = autonomy;
    }

    /// §29: the execution-environment preview for a task — repository,
    /// base, isolation, branch, working directory, agent (3c.md §29).
    pub fn task_environment_preview(&self, task_id: &TaskId) -> Option<ExecutionEnvironment> {
        let task = self.tasks.graph().get_task(task_id)?;
        let workspace = self.workspace(&task.workspace_id)?;
        let wt = task
            .worktree_id
            .as_ref()
            .and_then(|w| self.worktrees.get(w));
        let worktree_id = wt.map(|r| r.id.clone());
        Some(ExecutionEnvironment {
            repository: workspace.project_root.clone(),
            base_revision: wt.and_then(|r| r.base_revision.clone()),
            base_branch: wt.and_then(|r| r.base_branch.clone()),
            base_timestamp_ms: wt.map(|r| r.base_timestamp_ms).unwrap_or(0),
            working_directory: wt
                .map(|r| r.path.clone())
                .unwrap_or_else(|| workspace.project_root.clone()),
            worktree_id,
            branch: wt.map(|r| r.branch.clone()),
            isolation: task.isolation,
            environment_variables: Vec::new(),
        })
    }

    /// §40 "Open Agent": attaches a live pane to an existing task agent
    /// execution (headless by default in 3A — attach on demand).
    pub fn attach_task_agent_pane(&mut self, task_id: &TaskId) -> Result<PaneId> {
        let eid = self
            .tasks
            .graph()
            .get_task(task_id)
            .and_then(|t| t.agent_execution_id.clone())
            .ok_or_else(|| anyhow::anyhow!("task has no agent execution"))?;
        let (target, cwd) = {
            let ws = self.active_workspace();
            let Some(tab) = ws.active_tab() else {
                bail!("no active tab");
            };
            let target = tab
                .active_pane
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no focused pane"))?;
            let cwd = tab
                .root
                .find_pane(&target)
                .map(|p| p.cwd.clone())
                .unwrap_or_else(|| ws.project_root.clone());
            (target, cwd)
        };
        let launch = self
            .agent_runtime
            .get_session(&eid)
            .map(|s| s.launch)
            .ok_or_else(|| anyhow::anyhow!("agent execution no longer registered"))?;
        let mut pane = Pane::new(ExecutionKind::Agent, eid.clone(), cwd);
        let mut stored = launch;
        stored.redact();
        pane.metadata = serde_json::json!({ "agent": { "launch": stored } });
        let tab = self.active_tab_mut()?;
        let inserted = tab
            .root
            .split_by_id(&target, SplitDirection::Vertical, pane)
            .context("target pane disappeared")?;
        tab.active_pane = Some(inserted.clone());
        self.events.publish(ApplicationEvent::PaneCreated {
            pane_id: inserted.clone(),
            execution_id: eid,
        });
        Ok(inserted)
    }

    // ------------------------------------------------------------------
    // Phase 3A: scheduler drain integration
    // ------------------------------------------------------------------

    /// One deterministic orchestration pass, driven from `drain_frame`:
    /// builds the runtime view → scheduler decides → engine executes
    /// commands → events published (§22, §54).
    fn step_tasks(&mut self) {
        let mut view = SchedulerView::default();
        for (task_id, eid) in self.tasks.running_snapshot() {
            let Some(state) = self.agent_runtime.raw_state(&eid) else {
                continue;
            };
            // Exited = the reader thread observed EOF *or* the process is
            // gone (authoritative `try_wait`). The reader observes EOF on
            // its own schedule, which under load can lag a full frame behind
            // the actual process exit (3a §48 determinism); `try_wait`
            // reports the child as dead immediately, so the scheduler's
            // deferral fires as soon as a co-started sibling's process is
            // gone even while its reader/pump still settle.
            let exited = self.agent_runtime.has_exited(&eid)
                || self
                    .pty
                    .try_wait(&eid.0)
                    .map(|s| s.is_some())
                    .unwrap_or(false);
            view.running.insert(
                task_id.clone(),
                RuntimeAgentView {
                    state,
                    exit_code: self
                        .agent_runtime
                        .get_session(&eid)
                        .and_then(|s| s.exit_code),
                    work: self.agent_runtime.get_work(&eid),
                    estimated_cost_cents: self.agent_runtime.estimated_cost_cents(&eid),
                    exited,
                },
            );
        }
        // Phase 3D §9: artifact readiness — a Ready task whose declared
        // input artifact is unavailable is Blocked (never silently
        // continued). The store is authoritative; raw path inputs keep
        // their 3A semantics.
        for task_id in self.tasks.graph().list_task_ids() {
            let Some(task) = self.tasks.graph().get_task(&task_id) else {
                continue;
            };
            if task.status != TaskStatus::Ready {
                continue;
            }
            let missing: Vec<String> = task
                .input_artifacts
                .iter()
                .filter_map(|a| {
                    let id = resolve_artifact_ref(a)?;
                    if self.artifacts.get(&id).is_none() {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect();
            if !missing.is_empty() {
                let reason = format!(
                    "required input artifact(s) unavailable: {}",
                    missing.join(", ")
                );
                view.artifact_blocked.insert(task_id.clone(), reason);
            }
        }
        let cmds = self.tasks.step(&view, !self.workflow_paused);
        self.execute_commands(cmds);
        self.publish_task_events();
    }

    fn execute_commands(&mut self, cmds: Vec<SchedulerCommand>) {
        for cmd in cmds {
            match cmd {
                SchedulerCommand::SpawnTask { task_id } => {
                    // Phase 3F §33: PAUSE ALL — pending tasks stay Ready and
                    // nothing new starts; Resume re-runs the same commands.
                    if self.workflow_paused {
                        continue;
                    }
                    match self.spawn_task_agent(&task_id) {
                        Ok(eid) => self.tasks.note_spawned(&task_id, &eid),
                        Err(e) => self.tasks.note_spawn_failed(&task_id, e),
                    }
                }
                SchedulerCommand::StopTask {
                    task_id,
                    execution_id,
                } => {
                    let result = self.agent_runtime.stop(&execution_id);
                    self.tasks.note_stopped(&task_id, &execution_id);
                    if let Err(e) = result {
                        tracing::warn!(
                            "task {}: stop of {} failed: {:#}",
                            task_id,
                            execution_id.0,
                            e
                        );
                    }
                }
            }
        }
    }

    fn publish_task_events(&mut self) {
        for event in self.tasks.take_events() {
            // Phase 3B: mirror authoritative scheduler transitions into the
            // plan's step status map — the planner observes, never mutates
            // task state directly (§23, §33).
            if self.planner.phase() == PlannerPhase::Executing {
                if let Some(status) = task_status_of_event(&event) {
                    self.planner.on_step_status(event.task_id(), status);
                    if matches!(event, TaskEvent::TaskCompleted { .. }) {
                        self.publish_planner_event(PlannerEvent::PlanStepCompleted {
                            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
                            step_id: event.task_id().clone(),
                        });
                    }
                }
            }
            // Phase 3C: completion → deterministic diff + review state for
            // isolated tasks (§18–§19); failure/cancellation preserves the
            // worktree and its changes (§30, §42).
            match &event {
                TaskEvent::TaskCompleted { task_id, .. }
                | TaskEvent::TaskNeedsReview { task_id } => {
                    self.capture_worktree_result(task_id);
                    // Phase 3D §3: register the task's output artifacts in
                    // the store (redacted, bounded payloads) so dependent
                    // tasks can consume them cross-worktree (§11).
                    self.register_task_artifacts(task_id);
                }
                TaskEvent::TaskFailed { task_id, .. } | TaskEvent::TaskCancelled { task_id } => {
                    self.preserve_worktree(task_id);
                }
                _ => {}
            }
            self.events.publish(ApplicationEvent::TaskEvent { event });
        }
        self.maybe_finish_plan();
        // Phase 3D §44: deterministic replan signals (deduplicated).
        self.detect_replan_signals();
    }

    /// Detects plan completion from the authoritative scheduler state
    /// (never from the planner itself) and advances the plan state machine
    /// (§23 — completion is observed, not decided by the planner).
    fn maybe_finish_plan(&mut self) {
        if self.planner.phase() != PlannerPhase::Executing {
            return;
        }
        let steps = self.planner.status().steps;
        let all_terminal = !steps.is_empty()
            && steps.iter().all(|s| {
                matches!(
                    s.status,
                    PlanStepStatus::Completed
                        | PlanStepStatus::Failed
                        | PlanStepStatus::Cancelled
                        | PlanStepStatus::Skipped
                )
            });
        if !all_terminal {
            return;
        }
        let all_completed = steps.iter().all(|s| s.status == PlanStepStatus::Completed);
        self.planner.finish_execution(all_completed);
        if all_completed {
            self.planner.metrics_mut().executions_succeeded += 1;
        } else {
            self.planner.metrics_mut().executions_failed += 1;
        }
    }

    fn publish_planner_event(&mut self, event: PlannerEvent) {
        self.events
            .publish(ApplicationEvent::PlannerEvent { event });
    }

    // ------------------------------------------------------------------
    // Phase 3B: planner (3b.md §1–§34) — the LLM is a planner, never an
    // orchestrator. Everything here ends in the deterministic validator,
    // compiler, or scheduler; the planner never spawns processes, changes
    // policy, or mutates task state directly.
    // ------------------------------------------------------------------

    /// Injects the planner provider (§7). Tests inject deterministic
    /// mocks; no real LLM is required in standard CI (§47).
    pub fn set_planner_provider(&mut self, provider: Box<dyn PlannerProvider>) {
        self.planner_provider = Some(provider);
    }

    pub fn planner_config(&self) -> &PlannerConfig {
        &self.planner_config
    }

    pub fn set_planner_config(&mut self, config: PlannerConfig) {
        self.planner_config = config;
    }

    pub fn planner_status(&self) -> PlannerStatus {
        self.planner.status()
    }

    pub fn planner_metrics(&self) -> PlannerMetrics {
        self.planner.metrics().clone()
    }

    pub fn planner_audit(&self) -> Vec<PlannerAuditRecord> {
        self.planner.audit().to_vec()
    }

    pub fn planner_last_error(&self) -> Option<String> {
        self.planner.last_error().map(|e| e.to_string())
    }

    /// §43–§44: deterministic disposition of a natural-language request —
    /// simple commands bypass the planner, multi-step work routes to it.
    pub fn classify_request(&self, intent: &str) -> IntentDisposition {
        classify_intent(intent)
    }

    /// The provider id bound to the planner (or `None` when no provider is
    /// configured — §46: offline terminals keep working, planning is the
    /// only thing unavailable).
    pub fn planner_provider_id(&self) -> Option<String> {
        self.planner_provider
            .as_ref()
            .map(|p| p.provider_id().to_string())
    }

    /// §5: the bounded, allowlisted context the planner would receive.
    /// Public so the context is inspectable/auditable (§5 — never an
    /// unrestricted dump).
    pub fn planner_context(&self) -> PlannerContext {
        PlannerContextBuilder::new(self.planner_context_input()).build()
    }

    /// The constraints snapshot handed to the planner (§4, §12): budget
    /// and parallelism come from the authoritative scheduler policy, so a
    /// plan can never raise them (§33).
    fn planner_constraints(&self) -> PlannerConstraints {
        let policy = self.tasks.policy();
        PlannerConstraints {
            budget_cents: policy.max_cost_cents,
            max_parallel_tasks: policy.max_parallel_tasks,
            approval: self.planner_config.approval,
            user_preferences: Vec::new(),
            max_worktrees: policy.max_worktrees,
        }
    }

    /// Bounded, allowlisted context input from the engine's own state
    /// (§5): workspace, agents, tasks, providers, constraints — never
    /// secrets, never the filesystem dump (§27).
    fn planner_context_input(&self) -> PlannerContextInput {
        let ws = self.active_workspace();
        let available_agents = self
            .agent_runtime
            .registry()
            .list()
            .iter()
            .map(|def| AgentSummary {
                id: def.id.clone(),
                display_name: def.display_name.clone(),
                capabilities: self
                    .agent_runtime
                    .find_adapter(&def.id)
                    .map(|a| a.capabilities())
                    .unwrap_or_default(),
            })
            .collect();
        let to_summary = |t: &Task| TaskSummary {
            id: t.id.clone(),
            title: t.title.clone(),
            status: format!("{:?}", t.status),
        };
        let all: Vec<&Task> = self.task_list();
        let active_tasks: Vec<TaskSummary> = all
            .iter()
            .filter(|t| !t.status.is_terminal())
            .map(|t| to_summary(t))
            .take(12)
            .collect();
        let recent_tasks: Vec<TaskSummary> = all
            .iter()
            .filter(|t| t.status.is_terminal())
            .rev()
            .map(|t| to_summary(t))
            .take(12)
            .collect();
        PlannerContextInput {
            workspace_id: ws.id.clone(),
            workspace_name: ws.name.clone(),
            project_root: ws.project_root.clone(),
            available_agents,
            active_tasks,
            recent_tasks,
            provider_ids: self.agent_runtime.provider_ids(),
            constraints: self.planner_constraints(),
        }
    }

    /// §6, §20–§21, §43–§44: routes a request through the planner. Simple
    /// intents are bypassed deterministically. A plan reaches
    /// `NeedsApproval` only after schema parse (§9) and policy validation
    /// (§14) pass; no task starts until then (§21).
    pub fn plan_request(&mut self, intent: &str) -> Result<(), PlannerError> {
        match classify_intent(intent) {
            IntentDisposition::Bypass { reason } => {
                self.planner.metrics_mut().bypassed_intents += 1;
                return Err(PlannerError::Bypassed { reason });
            }
            IntentDisposition::Plan => {}
        }
        if self.planner_provider.is_none() {
            return Err(PlannerError::NoProvider);
        }
        let normalized = normalize_intent(intent);
        let ws = self.active_workspace();
        let request = PlannerRequest {
            request_id: format!("plan-req-{}", terminal_session::planning::now_ms()),
            intent: normalized.objective,
            workspace_id: ws.id.clone(),
            context: PlannerContextBuilder::new(self.planner_context_input()).build(),
            constraints: self.planner_constraints(),
            mode: terminal_session::planning::PlannerRequestMode::Initial,
        };
        self.planner.begin_request(
            &request.request_id,
            &request.intent,
            &self.planner_config.provider,
            &self.planner_config.model,
        )?;
        self.publish_planner_event(PlannerEvent::PlanningStarted {
            request_id: request.request_id.clone(),
            intent: request.intent.clone(),
        });
        let provider = self.planner_provider.as_ref().unwrap();
        let t0 = std::time::Instant::now();
        let result = provider.generate(&request, &self.planner_config);
        let latency_ms = t0.elapsed().as_millis() as u64;
        for event in self.planner.on_provider_result(result, latency_ms) {
            self.publish_planner_event(event);
        }
        match self.planner.phase() {
            PlannerPhase::NeedsApproval => self.plan_validate()?,
            PlannerPhase::Failed => {
                return Err(self.planner.last_error().cloned().unwrap_or(
                    PlannerError::NotAllowed {
                        reason: "planning failed".to_string(),
                    },
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// §14: validates the current plan against the engine's authoritative
    /// registry and scheduler policy (agents, dependencies, cycles,
    /// budget, parallelism). Unavailable agents are reported — never
    /// silently substituted (§12).
    pub fn plan_validate(&mut self) -> Result<(), PlannerError> {
        let Some(plan) = self.planner.plan().cloned() else {
            return Err(PlannerError::NotAllowed {
                reason: "no plan to validate".to_string(),
            });
        };
        let constraints = self.planner_constraints();
        let registry = self.agent_runtime.registry();
        let mut availability = std::collections::HashMap::new();
        for def in registry.list() {
            availability.insert(def.id.clone(), self.planner_agent_availability(&def.id));
        }
        let check = |id: &str| {
            availability
                .get(id)
                .copied()
                .unwrap_or(AgentAvailability::Unknown)
        };
        let validator = PlanValidator::with_availability(registry, &check);
        match self.planner.validate_plan(&validator, &constraints) {
            Ok(()) => {
                self.publish_planner_event(PlannerEvent::PlanValidated {
                    request_id: self.planner.request_id().unwrap_or_default().to_string(),
                    plan_hash: self.planner.status().plan_hash,
                    warnings: plan.warnings,
                });
                Ok(())
            }
            Err(e) => {
                self.publish_planner_event(PlannerEvent::PlanningFailed {
                    request_id: self.planner.request_id().unwrap_or_default().to_string(),
                    error: e.clone(),
                    retries: 0,
                });
                Err(e)
            }
        }
    }

    /// §12: deterministic agent availability — registered AND
    /// binary-resolvable through the adapter boundary. The planner never
    /// probes executables itself, and the check mirrors the runtime's own
    /// spawn resolution exactly (fake-agent has its own binary lookup).
    fn planner_agent_availability(&self, id: &str) -> AgentAvailability {
        let Some(def) = self.agent_runtime.registry().get(id) else {
            return AgentAvailability::Unknown;
        };
        let Some(adapter) = self.agent_runtime.find_adapter(id) else {
            return AgentAvailability::Unknown;
        };
        let resolved = if id == "fake-agent" {
            terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary()
        } else {
            terminal_session::adapters::resolve_binary(adapter.as_ref(), def)
        };
        match resolved {
            Ok(_) => AgentAvailability::Available,
            Err(_) => AgentAvailability::Unavailable,
        }
    }

    pub fn plan_approve(&mut self) -> Result<(), PlannerError> {
        self.planner.approve()?;
        self.publish_planner_event(PlannerEvent::PlanApproved {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
            plan_hash: self.planner.status().plan_hash,
        });
        Ok(())
    }

    pub fn plan_reject(&mut self, reason: &str) -> Result<(), PlannerError> {
        let plan_hash = self.planner.status().plan_hash;
        self.planner.reject(reason)?;
        self.publish_planner_event(PlannerEvent::PlanRejected {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
            plan_hash,
            reason: reason.to_string(),
        });
        Ok(())
    }

    /// §18–§19: a human edit. Applied locally, never silently replaces the
    /// plan; the engine re-validates before execution.
    pub fn plan_edit(&mut self, change: &PlanEditChange) -> Result<(), PlannerError> {
        self.planner.edit(change)?;
        self.publish_planner_event(PlannerEvent::PlanEdited {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
            plan_hash: self.planner.status().plan_hash,
            change: format!("{change:?}"),
        });
        Ok(())
    }

    /// §18: user-set parallel batch cap (clamped by the state machine; the
    /// scheduler's own policy remains the ceiling — §33).
    pub fn plan_set_parallelism(&mut self, n: usize) {
        self.planner.set_parallelism(n);
    }

    pub fn plan_cancel(&mut self) {
        self.planner.cancel();
        self.publish_planner_event(PlannerEvent::PlanCancelled {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
        });
    }

    /// §23: compiles the approved plan and hands it to the authoritative
    /// scheduler. The planner is out of control from here; the scheduler's
    /// own policy (budget, concurrency) governs execution (§33).
    pub fn plan_execute(&mut self) -> Result<Vec<TaskId>, PlannerError> {
        // §19: edited plans are re-validated before execution — a human
        // edit never silently skips the deterministic gates.
        if self.planner.status().edited {
            self.plan_validate()?;
        }
        let ws_id = self.active_workspace().id.clone();
        let (graph, _policy) = self.planner.compile_for_execution(&ws_id)?;
        let ids = self.import_plan_graph(&graph)?;
        self.tasks.submit_all();
        self.step_tasks();
        self.publish_planner_event(PlannerEvent::PlanExecutionStarted {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
            task_ids: ids.clone(),
        });
        Ok(ids)
    }

    /// Imports a compiled plan graph into the scheduler. Rejects id
    /// collisions (never silently overwrites unrelated tasks). The
    /// compiled policy is NOT applied — the scheduler policy stays
    /// authoritative (§33).
    fn import_plan_graph(&mut self, graph: &TaskGraph) -> Result<Vec<TaskId>, PlannerError> {
        let ids = graph.list_task_ids();
        for id in &ids {
            if self.tasks.graph().get_task(id).is_some() {
                return Err(PlannerError::NotAllowed {
                    reason: format!("task id {id} already exists in the scheduler"),
                });
            }
        }
        for task in graph.list_tasks() {
            let mut t = task.clone();
            t.id = task.id.clone();
            // Edges are registered below via `add_dependency`; the cloned
            // task's dependency list is cleared so the authoritative graph
            // owns the edges (§33).
            t.dependencies.clear();
            self.tasks
                .graph_mut()
                .add_task(t)
                .map_err(|e| PlannerError::NotAllowed {
                    reason: e.to_string(),
                })?;
        }
        for id in &ids {
            for dep in graph.dependencies_of(id) {
                self.tasks
                    .graph_mut()
                    .add_dependency(id, &dep)
                    .map_err(|e| PlannerError::NotAllowed {
                        reason: e.to_string(),
                    })?;
            }
        }
        self.plan_task_ids = ids.clone();
        Ok(ids)
    }

    /// §26: explicit resume after interruption. Removes the plan's own
    /// previously-scheduled tasks (never anyone else's), re-imports the
    /// remaining steps, and re-submits. Never resumes silently.
    pub fn plan_resume(&mut self) -> Result<Vec<TaskId>, PlannerError> {
        let ws_id = self.active_workspace().id.clone();
        let (graph, _policy) = self.planner.resume(&ws_id)?;
        for id in &self.plan_task_ids {
            let _ = self.tasks.graph_mut().remove_task(id);
        }
        self.plan_task_ids.clear();
        let ids = self.import_plan_graph(&graph)?;
        self.tasks.submit_all();
        self.step_tasks();
        self.publish_planner_event(PlannerEvent::PlanResumed {
            plan_id: self.planner.plan_id().unwrap_or_default().to_string(),
            completed: self.planner.status().completed_count,
            remaining: ids.len(),
        });
        Ok(ids)
    }

    /// §26: interrupt execution (restart path). Resume is always explicit.
    pub fn plan_interrupt(&mut self, reason: &str) {
        self.planner.interrupt(reason);
    }

    /// §25: persisted plan slice (never credentials or private reasoning).
    pub fn plan_export_persisted(&self) -> Option<PersistedPlanState> {
        self.planner.export_persisted()
    }

    /// §26: restore an interrupted plan after restart. The plan stays in
    /// `Interrupted` — nothing resumes silently.
    pub fn plan_restore(&mut self, state: PersistedPlanState) {
        self.planner = PlannerState::import_persisted(state);
        self.plan_task_ids.clear();
    }

    /// Builds the launch for a task through the adapter boundary (§20):
    /// `prepare_task` produces the vendor instruction; explicit task
    /// arguments/environment are appended as deterministic overrides. The
    /// execution environment (worktree etc.) is resolved *before* the
    /// adapter — the scheduler/environment layer decides where the agent
    /// runs, never the adapter (3c.md §11, §44).
    fn spawn_task_agent(&mut self, task_id: &TaskId) -> Result<ExecutionId> {
        // Phase 3C §44: request the execution environment first. Isolated
        // tasks get a dedicated worktree; the launch cwd is the env's
        // working directory.
        let env = self.resolve_execution_environment(task_id)?;
        // Phase 3D §11: cross-worktree artifact consumption — materialize
        // the task's explicitly granted input artifacts into its own
        // worktree before the agent starts. Never assumes a shared
        // filesystem; access is enforced by the policy (§39–§40).
        if let Some(e) = &env {
            let input_refs: Vec<String> = self
                .tasks
                .graph()
                .get_task(task_id)
                .map(|t| t.input_artifacts.clone())
                .unwrap_or_default();
            for input in &input_refs {
                let Some(art_id) = resolve_artifact_ref(input) else {
                    continue;
                };
                if !ArtifactAccessPolicy::can_access(task_id, &art_id, self.tasks.graph()) {
                    continue;
                }
                match ArtifactMaterializer::materialize(
                    &self.artifacts,
                    &art_id,
                    &e.working_directory,
                ) {
                    Ok(Some(_)) => {
                        self.events.publish(ApplicationEvent::ArtifactConsumed {
                            task_id: task_id.clone(),
                            artifact_id: art_id.clone(),
                        });
                    }
                    Ok(None) => {
                        tracing::warn!(
                            "task {task_id}: input artifact {art_id} has no materializable payload"
                        );
                    }
                    Err(err) => {
                        tracing::warn!("task {task_id}: materializing {art_id} failed: {err}");
                    }
                }
            }
        }
        let launch = {
            let graph = self.tasks.graph();
            let task = graph
                .get_task(task_id)
                .ok_or_else(|| anyhow::anyhow!("unknown task {task_id}"))?;
            let workspace = self
                .workspace(&task.workspace_id)
                .ok_or_else(|| anyhow::anyhow!("unknown workspace"))?;
            let cwd = env
                .as_ref()
                .map(|e| e.working_directory.clone())
                .unwrap_or_else(|| workspace.project_root.clone());
            // The attempt number is deterministic scheduling info the adapter
            // may need (fixtures like `flaky`), so it is part of the context.
            let mut context_env = task.environment.clone();
            context_env.push((
                "FAKE_AGENT_ATTEMPT".to_string(),
                task.attempt_count.to_string(),
            ));
            let ctx = TaskContext {
                workspace_id: workspace.id.clone(),
                workspace_name: workspace.name.clone(),
                project_root: workspace.project_root.clone(),
                task_id: task.id.clone(),
                task_title: task.title.clone(),
                task_description: task.description.clone(),
                dependencies: graph.dependency_summaries(task_id),
                artifact_paths: graph.input_artifact_paths(task_id),
                relevant_files: graph.relevant_files(task_id),
                environment: context_env,
            };
            let adapter = self
                .agent_runtime
                .find_adapter(&task.assigned_agent)
                .ok_or_else(|| anyhow::anyhow!("unknown agent definition"))?;
            let prepared = adapter.prepare_task(&ctx);
            let mut args = prepared.arguments;
            args.extend(task.arguments.iter().cloned());
            let mut env_pairs = prepared.environment;
            env_pairs.extend(task.environment.iter().cloned());
            env_pairs.push((
                "FAKE_AGENT_ATTEMPT".to_string(),
                task.attempt_count.to_string(),
            ));
            AgentLaunchConfig {
                definition_id: task.assigned_agent.clone(),
                cwd,
                arguments: args,
                provider_id: None,
                model_id: None,
                credential_ref: None,
                resume_id: None,
                environment: env_pairs,
            }
        };
        // §12: hard wrong-cwd guard — the agent must run exactly where the
        // worktree says. The launch cwd was built from the environment, so
        // this is a defense-in-depth verification against any launch-level
        // tampering.
        if let Some(e) = &env {
            if let Some(wt) = &e.worktree_id {
                self.worktrees.assert_cwd(wt, &launch.cwd)?;
            }
        }
        // Isolated coding tasks always land in review — completed never
        // means merged (§19, §54).
        if env.as_ref().map(|e| e.isolation) == Some(IsolationMode::GitWorktree) {
            if let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) {
                task.review_required = true;
            }
        }
        let r = self.spawn_agent_session(launch, DEFAULT_COLS, DEFAULT_ROWS);
        if let Err(e) = &r {
            tracing::warn!("task {task_id}: spawn failed: {e:#}");
        }
        r
    }

    /// §44: resolves the execution environment for a task attempt — the
    /// only place worktrees are created (never in the adapter, never in the
    /// planner). Reuses the task's worktree on attempt 1 and per the
    /// explicit retry policy (§10, §43). Non-git workspaces degrade
    /// gracefully to the shared root with a warning (no isolation possible).
    fn resolve_execution_environment(
        &mut self,
        task_id: &TaskId,
    ) -> Result<Option<ExecutionEnvironment>> {
        let (repo, isolation, shared, slug, worktree_id, attempt) = {
            let graph = self.tasks.graph();
            let task = graph
                .get_task(task_id)
                .ok_or_else(|| anyhow::anyhow!("unknown task {task_id}"))?;
            let workspace = self
                .workspace(&task.workspace_id)
                .ok_or_else(|| anyhow::anyhow!("unknown workspace"))?;
            (
                workspace.project_root.clone(),
                task.isolation,
                task.requires_shared_workspace,
                task.slug.clone(),
                task.worktree_id.clone(),
                task.attempt_count,
            )
        };
        match isolation {
            IsolationMode::SharedWorkspace => {
                if !shared {
                    tracing::warn!(
                        "task {task_id}: shared-workspace isolation without requires_shared_workspace"
                    );
                }
                Ok(Some(ExecutionEnvironment {
                    repository: repo.clone(),
                    base_revision: None,
                    base_branch: None,
                    base_timestamp_ms: 0,
                    working_directory: repo,
                    worktree_id: None,
                    branch: None,
                    isolation: IsolationMode::SharedWorkspace,
                    environment_variables: Vec::new(),
                }))
            }
            IsolationMode::GitWorktree => {
                // Graceful degradation: no git binary or not a repository →
                // run in the shared root (documented, warn-level). Real
                // isolation requires a real repository (§13).
                if !git_available() || self.worktrees.repository_ok(&repo, None).is_err() {
                    tracing::warn!("task {task_id}: workspace is not a git repository — running in the shared workspace without isolation");
                    return Ok(Some(ExecutionEnvironment {
                        repository: repo.clone(),
                        base_revision: None,
                        base_branch: None,
                        base_timestamp_ms: 0,
                        working_directory: repo,
                        worktree_id: None,
                        branch: None,
                        isolation: IsolationMode::SharedWorkspace,
                        environment_variables: Vec::new(),
                    }));
                }
                let base = self.worktrees.head_revision(&repo).ok();
                let slug = if slug.is_empty() { "task" } else { &slug };
                let retry = self.tasks.policy().retry_worktree;
                let env = self.worktrees.environment_for_task(
                    &repo,
                    task_id,
                    slug,
                    worktree_id.as_deref(),
                    base.as_deref(),
                    attempt,
                    retry,
                )?;
                // §10: at most one active worktree per task — record the
                // owner so completion can capture diff/provenance.
                if let Some(wt) = &env.worktree_id {
                    if let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) {
                        task.worktree_id = Some(wt.clone());
                    }
                }
                Ok(Some(env))
            }
            IsolationMode::TemporaryDirectory => {
                // Throwaway directory — no git operations, nothing persisted.
                let dir = std::env::temp_dir().join(format!("flash-task-{task_id}-{attempt}"));
                let _ = std::fs::create_dir_all(&dir);
                Ok(Some(ExecutionEnvironment {
                    repository: repo,
                    base_revision: None,
                    base_branch: None,
                    base_timestamp_ms: 0,
                    working_directory: dir.to_string_lossy().to_string(),
                    worktree_id: None,
                    branch: None,
                    isolation: IsolationMode::TemporaryDirectory,
                    environment_variables: Vec::new(),
                }))
            }
        }
    }

    /// §17–§18: captures deterministic diff + worktree provenance into the
    /// scheduler's TaskResult at completion (the scheduler stays pure — no
    /// git) and moves the worktree to `NeedsReview`. The diff is generated
    /// from git against the base revision, never from the agent's own
    /// summary (§18).
    fn capture_worktree_result(&mut self, task_id: &TaskId) {
        let Some(worktree_id) = self
            .tasks
            .graph()
            .get_task(task_id)
            .and_then(|t| t.worktree_id.clone())
        else {
            return;
        };
        let diff = match self.worktrees.diff(&worktree_id) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("task {task_id}: worktree diff failed: {e:#}");
                return;
            }
        };
        let record = self.worktrees.get(&worktree_id).cloned();
        let (base_revision, branch, worktree_path) = match &record {
            Some(r) => (
                r.base_revision.clone(),
                Some(r.branch.clone()),
                Some(r.path.clone()),
            ),
            None => (diff.base_revision.clone(), None, None),
        };
        // The agent just finished → NeedsReview. But this can also fire
        // when review was already resolved (approval re-emits TaskCompleted
        // through the scheduler), so never downgrade an explicit
        // Approved/Rejected/Merged state (§21).
        let state = self.worktrees.get(&worktree_id).map(|r| r.state);
        if matches!(
            state,
            Some(WorktreeState::Created | WorktreeState::Active | WorktreeState::Completed)
        ) {
            let _ = self
                .worktrees
                .set_state(&worktree_id, WorktreeState::NeedsReview);
        }
        if let Some(task) = self.tasks.graph_mut().get_task_mut(task_id) {
            if let Some(result) = task.result.as_mut() {
                result.base_revision = base_revision;
                result.result_revision = diff.result_revision.clone();
                result.branch = branch;
                result.worktree = worktree_path;
                result.diff_summary = Some(Box::new(diff.clone()));
                result.files_changed = diff.files_changed.clone();
            }
        }
    }

    /// Phase 3D §3–§4: registers a completed task's output artifacts in the
    /// authoritative store — metadata is engine-stamped (agent, workspace,
    /// worktree, revision, timestamp), payloads are bounded and redacted
    /// (§38: artifacts must never become a secret-leak path). Emits
    /// metadata-only events (§27).
    fn register_task_artifacts(&mut self, task_id: &TaskId) {
        let Some(task) = self.tasks.graph().get_task(task_id).cloned() else {
            return;
        };
        let Some(result) = &task.result else {
            return;
        };
        let worktree_path = task
            .worktree_id
            .as_ref()
            .and_then(|w| self.worktrees.get(w))
            .map(|r| r.path.clone());
        let now = terminal_session::planning::now_ms();
        // Repository artifacts from the deterministic diff (git, never the
        // agent's claim) — the scheduler's observed-file artifacts may be
        // empty when the agent exited before observation caught up.
        let mut diff_files: Vec<String> = Vec::new();
        if let Some(d) = &result.diff_summary {
            diff_files.extend(d.files_changed.iter().cloned());
            diff_files.extend(d.files_created.iter().cloned());
            diff_files.extend(d.files_deleted.iter().cloned());
        }
        diff_files.sort();
        diff_files.dedup();
        let mut registered: Vec<String> = Vec::new();
        for rel in diff_files {
            if task
                .output_artifacts
                .iter()
                .any(|a| a.path.as_deref() == Some(rel.as_str()))
            {
                continue;
            }
            let artifact = terminal_session::orchestration::Artifact {
                id: terminal_session::orchestration::new_artifact_id(),
                kind: terminal_session::orchestration::ArtifactType::CodeChange,
                path: Some(rel.clone()),
                description: "git diff change".to_string(),
                created_by_task: Some(task.id.clone()),
                metadata: vec![("auto".to_string(), "true".to_string())],
                created_by_agent: Some(task.assigned_agent.clone()),
                workspace_id: Some(task.workspace_id.clone()),
                worktree: worktree_path.clone(),
                revision: result.result_revision.clone(),
                created_at_ms: now,
            };
            let payload = worktree_path.as_ref().and_then(|wt| {
                let src = std::path::Path::new(wt).join(&rel);
                if !src.is_file() {
                    return None;
                }
                let bytes = std::fs::read(&src).ok()?;
                if bytes.len() > self.artifacts.max_payload_bytes() {
                    return None;
                }
                Some(
                    terminal_session::redact::Redactor::redact(&String::from_utf8_lossy(&bytes))
                        .into_bytes(),
                )
            });
            let meta = self.artifacts.register(artifact, payload, now);
            self.events.publish(ApplicationEvent::ArtifactCreated {
                artifact_id: meta.id.clone(),
                task_id: Some(task.id.clone()),
                kind: format!("{:?}", meta.kind),
                description: meta.description.clone(),
            });
            registered.push(meta.id);
        }
        for art in &task.output_artifacts {
            if self.artifacts.get(&art.id).is_some() {
                registered.push(art.id.clone());
                continue;
            }
            let mut artifact = art.clone();
            artifact.created_by_agent = Some(task.assigned_agent.clone());
            artifact.workspace_id = Some(task.workspace_id.clone());
            artifact.worktree = worktree_path.clone();
            artifact.revision = result.result_revision.clone();
            artifact.created_at_ms = now;
            // Bounded, redacted payload read from the producer worktree.
            let payload = art.path.as_ref().and_then(|rel| {
                let src = std::path::Path::new(&worktree_path.as_ref()?).join(rel);
                if !src.is_file() {
                    return None;
                }
                let bytes = std::fs::read(&src).ok()?;
                if bytes.len() > self.artifacts.max_payload_bytes() {
                    return None;
                }
                Some(
                    terminal_session::redact::Redactor::redact(&String::from_utf8_lossy(&bytes))
                        .into_bytes(),
                )
            });
            let meta = self.artifacts.register(artifact, payload, now);
            self.events.publish(ApplicationEvent::ArtifactCreated {
                artifact_id: meta.id.clone(),
                task_id: Some(task.id.clone()),
                kind: format!("{:?}", meta.kind),
                description: meta.description.clone(),
            });
            registered.push(meta.id);
        }
        // §5: lineage is driven by the task's declared outputs — stamp the
        // registered ids onto the task so `ArtifactLineage` maps producers.
        if !registered.is_empty() {
            if let Some(task_mut) = self.tasks.graph_mut().get_task_mut(task_id) {
                for id in registered {
                    if !task_mut.output_artifacts.iter().any(|a| a.id == id) {
                        task_mut.output_artifacts.push(Artifact {
                            id,
                            kind: terminal_session::orchestration::ArtifactType::CodeChange,
                            path: None,
                            description: String::new(),
                            created_by_task: Some(task_id.clone()),
                            metadata: Vec::new(),
                            created_by_agent: None,
                            workspace_id: None,
                            worktree: None,
                            revision: None,
                            created_at_ms: 0,
                        });
                    }
                }
            }
        }
    }

    /// §30/§42: a failed/cancelled isolated task keeps its worktree and
    /// changes (never deleted automatically) — the record moves to
    /// `Completed` so the user can review/reopen/discard.
    fn preserve_worktree(&mut self, task_id: &TaskId) {
        let Some(worktree_id) = self
            .tasks
            .graph()
            .get_task(task_id)
            .and_then(|t| t.worktree_id.clone())
        else {
            return;
        };
        let _ = self
            .worktrees
            .set_state(&worktree_id, WorktreeState::Completed);
    }

    // ------------------------------------------------------------------
    // Phase 2C: dashboard / attention / review / prefs (§9–§14, §21–§22)
    // ------------------------------------------------------------------

    /// Global agent dashboard (§13): counts + sorted rows. Sort order is
    /// deterministic: needs-you first, then running, failed, completed.
    pub fn agent_dashboard(&self, filter: AgentFilter) -> AgentDashboard {
        let mut rows: Vec<AgentRow> = self
            .agent_runtime
            .list_sessions()
            .into_iter()
            .filter(|s| {
                let state = agent_state_from_str(&s.state);
                filter.matches_state(state)
            })
            .map(|snapshot| {
                let pane_id =
                    self.pane_id_for_execution(&ExecutionId(snapshot.execution_id.clone()));
                AgentRow { pane_id, snapshot }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(sort_rank(&r.snapshot)));
        let mut d = AgentDashboard {
            rows,
            ..Default::default()
        };
        d.total = d.rows.len();
        for r in &d.rows {
            let state = agent_state_from_str(&r.snapshot.state);
            match state {
                AgentState::Completed | AgentState::Stopped => d.completed += 1,
                AgentState::Failed | AgentState::Crashed | AgentState::Blocked => {
                    d.failed += 1;
                    if terminal_session::work::attention_for(state).is_some() {
                        d.needs_you += 1;
                    }
                }
                _ => {
                    if terminal_session::work::attention_for(state).is_some() {
                        d.needs_you += 1;
                    } else {
                        d.running += 1;
                    }
                }
            }
        }
        d
    }

    /// Per-workspace summary (§14) for the active workspace.
    pub fn workspace_agent_summary(&self) -> WorkspaceAgentSummary {
        let mut s = WorkspaceAgentSummary::default();
        let Some(tab) = self.active_workspace().active_tab() else {
            return s;
        };
        let mut panes = Vec::new();
        tab.root.panes(&mut panes);
        for p in panes {
            if p.execution_kind != ExecutionKind::Agent {
                continue;
            }
            s.agents += 1;
            let Some(snap) = self.agent_runtime.get_session(&p.execution_id) else {
                continue;
            };
            let state = agent_state_from_str(&snap.state);
            match state {
                AgentState::Completed | AgentState::Stopped => s.completed += 1,
                AgentState::Failed | AgentState::Crashed | AgentState::Blocked => {
                    s.failed += 1;
                    if terminal_session::work::attention_for(state).is_some() {
                        s.needs_you += 1;
                    }
                }
                _ => {
                    if terminal_session::work::attention_for(state).is_some() {
                        s.needs_you += 1;
                    } else {
                        s.running += 1;
                    }
                }
            }
        }
        s
    }

    /// Review surface (§9–§10): changed files with best-effort git diffs
    /// (bounded) + the command history.
    pub fn agent_review(&self, eid: &ExecutionId) -> Option<AgentReview> {
        let work = self.agent_runtime.get_work(eid)?;
        let mut review = AgentReview {
            commands: work.commands.clone(),
            ..Default::default()
        };
        for f in work.files_changed.iter().take(64) {
            let diff = self.git_diff(&work.session_id, f);
            review.files.push(AgentFileChange {
                path: f.clone(),
                diff,
            });
        }
        Some(review)
    }

    fn git_diff(&self, cwd: &str, file: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["-C", cwd, "diff", "--", file])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut lines: Vec<&str> = text.lines().collect();
        if lines.len() > 200 {
            lines.truncate(200);
        }
        Some(lines.join("\n"))
    }

    /// Quiet-mode prefs for the active workspace (§22), stored in its
    /// metadata (never secret-bearing).
    pub fn notification_prefs(&self) -> NotificationPrefs {
        self.active_workspace()
            .metadata
            .get("notifications")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }

    pub fn set_notification_prefs(&mut self, prefs: &NotificationPrefs) {
        let ws = self.active_workspace_mut();
        if !ws.metadata.is_object() {
            ws.metadata = serde_json::json!({});
        }
        let obj = ws.metadata.as_object_mut().expect("metadata is an object");
        obj.insert(
            "notifications".into(),
            serde_json::to_value(prefs).unwrap_or_default(),
        );
    }

    /// Phase 2C §21: emits meaningful agent notifications, honoring the
    /// active workspace's quiet-mode prefs. Called from the drain path.
    fn emit_agent_notifications(&mut self, eid: &ExecutionId, event: &AgentEvent) {
        let Some(pane_id) = self.pane_id_for_execution(eid) else {
            return;
        };
        let prefs = self.notification_prefs();
        let name = self
            .agent_runtime
            .get_session(eid)
            .map(|s| s.display_name)
            .unwrap_or_else(|| "agent".into());
        match event {
            AgentEvent::StateChanged { new_state, .. } => {
                let reason = terminal_session::work::attention_for(*new_state);
                if reason.is_none() {
                    return;
                }
                let notified = self.notified_attention.entry(eid.clone()).or_default();
                if notified.contains(new_state) {
                    return;
                }
                notified.push(*new_state);
                match reason.unwrap() {
                    terminal_session::work::AttentionReason::PermissionRequested => {
                        if prefs.on_needs_me {
                            self.notifications.emit(Notification::new(
                                NotificationKind::AgentNeedsApproval {
                                    agent: name,
                                    pane_id,
                                },
                            ));
                        }
                    }
                    terminal_session::work::AttentionReason::NeedsInput => {
                        if prefs.on_needs_me {
                            self.notifications.emit(Notification::new(
                                NotificationKind::AgentNeedsInput {
                                    agent: name,
                                    pane_id,
                                },
                            ));
                        }
                    }
                    terminal_session::work::AttentionReason::Ambiguous => {
                        if prefs.on_needs_me {
                            self.notifications.emit(Notification::new(
                                NotificationKind::AttentionSummary {
                                    needs_you: 1,
                                    failed: 0,
                                    pane_id,
                                },
                            ));
                        }
                    }
                    terminal_session::work::AttentionReason::ErrorIntervention => {
                        if prefs.on_failure {
                            self.notifications.emit(Notification::new(
                                NotificationKind::AgentFailed {
                                    agent: name,
                                    code: None,
                                    pane_id,
                                },
                            ));
                        }
                    }
                }
            }
            AgentEvent::Completed if prefs.on_completion => {
                self.notifications
                    .emit(Notification::new(NotificationKind::AgentCompleted {
                        agent: name,
                        pane_id,
                    }));
            }
            _ => {}
        }
    }

    /// Serializes the whole domain state (§19, §30).
    pub fn snapshot_state(&self) -> PersistedState {
        let active = self
            .workspaces
            .get(self.active_workspace)
            .map(|w| w.id.clone());
        PersistedState {
            version: crate::persist::CURRENT_VERSION,
            workspaces: self.workspaces.clone(),
            active_workspace: active,
            tasks: Some(self.tasks.export_persisted()),
            worktrees: Some(self.worktree_list()),
            artifacts: Some(self.artifacts.metadata_snapshot()),
            review_reports: Some(self.review_reports.clone()),
            replan_signals: Some(self.replan_signals.clone()),
            adaptive: Some(terminal_session::adaptive::PersistedAdaptiveState {
                signals: self.adaptive.signals.clone(),
                plan_versions: self.adaptive.plan_versions.clone(),
                proposals: self.adaptive.proposals.clone(),
                task_invalidations: self.adaptive.task_invalidations.clone(),
                artifact_invalidations: self.adaptive.artifact_invalidations.clone(),
                escalations: self.adaptive.escalations.clone(),
                autonomy: self.adaptive.autonomy,
                limits: self.adaptive.limits.clone(),
                metrics: self.adaptive.metrics.clone(),
                quality: self.adaptive.quality.clone(),
                limit_reached: self.adaptive.limit_reached,
            }),
            // Phase 4 §17: audit trail survives restarts. Redacted at write
            // time; bounded by AuditTrail's RAM cap (§39).
            audit: Some(self.audit.persisted()),
            // Phase 4 §25: policy (autonomy, scope, network/secret/budget,
            // ledger, pending approvals) survives restarts. Credentials
            // never enter this struct. Pending approvals are re-verified
            // (identity/hash/expiry) at honor time after a restart.
            policy: Some((&self.policy).into()),
        }
    }

    /// Persists to disk (atomic write). Phase 2C §42: agent pane metadata
    /// is enriched with the bounded work record (title, last state,
    /// last activity, recent timeline, provider/model) — never secrets,
    /// never unbounded logs.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let mut state = self.snapshot_state();
        for ws in &mut state.workspaces {
            for tab in &mut ws.tabs {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                let ids: Vec<PaneId> = panes.iter().map(|p| p.id.clone()).collect();
                for pid in ids {
                    let Some(pane) = tab.root.find_pane_mut(&pid) else {
                        continue;
                    };
                    if pane.execution_kind != ExecutionKind::Agent {
                        continue;
                    }
                    let Some(work) = self.agent_runtime.get_work(&pane.execution_id) else {
                        continue;
                    };
                    let agent_obj = if pane.metadata.is_object() {
                        pane.metadata
                            .as_object_mut()
                            .expect("metadata is an object")
                    } else {
                        pane.metadata = serde_json::json!({});
                        pane.metadata
                            .as_object_mut()
                            .expect("metadata is an object")
                    };
                    let mut timeline = Vec::new();
                    for e in work.timeline.recent(20) {
                        timeline.push(serde_json::json!({
                            "kind": format!("{:?}", e.kind),
                            "detail": e.detail,
                            "at": e.at.to_rfc3339(),
                        }));
                    }
                    let last_activity = work
                        .current_activity()
                        .map(|a| {
                            serde_json::json!({
                                "kind": format!("{:?}", a.kind),
                                "source": format!("{:?}", a.source),
                                "confidence": a.confidence,
                                "detail": a.detail,
                                "count": a.count,
                            })
                        })
                        .unwrap_or(serde_json::Value::Null);
                    let last_state = self
                        .agent_runtime
                        .get_session(&pane.execution_id)
                        .map(|s| s.state)
                        .unwrap_or_else(|| "unknown".into());
                    agent_obj.insert(
                        "work".into(),
                        serde_json::json!({
                            "work_id": work.id,
                            "title": work.title,
                            "status": format!("{:?}", work.status),
                            "last_state": last_state,
                            "last_activity": last_activity,
                            "recent_timeline": timeline,
                            "started_at": work
                                .started_at
                                .map(|t| t.to_rfc3339())
                                .unwrap_or_default(),
                            "completed_at": work
                                .completed_at
                                .map(|t| t.to_rfc3339())
                                .unwrap_or_default(),
                        }),
                    );
                }
            }
        }
        crate::persist::save(&state, path)
    }

    /// Restores a persisted state: recreates the workspace/tab/pane
    /// structure and best-effort re-spawns sessions from each pane's cwd
    /// (§19, §20). Returns the panes whose session could not be restored.
    pub fn restore(&mut self, state: PersistedState) -> Vec<PaneId> {
        let mut failed = Vec::new();
        let mut new_workspaces = Vec::new();
        for ws in state.workspaces {
            let mut ws = ws;
            for tab in &mut ws.tabs {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                let ids: Vec<PaneId> = panes.iter().map(|p| p.id.clone()).collect();
                for pid in ids {
                    let pane = tab.root.find_pane(&pid);
                    let (cwd, exec_kind) = if let Some(p) = pane {
                        (p.cwd.clone(), p.execution_kind)
                    } else {
                        ("/".into(), ExecutionKind::Terminal)
                    };
                    let cwd_ok = if std::path::Path::new(&cwd).is_dir() {
                        cwd
                    } else {
                        "/".into()
                    };

                    if exec_kind == ExecutionKind::Terminal {
                        match self.spawn_terminal_session(cwd_ok, DEFAULT_COLS, DEFAULT_ROWS) {
                            Ok(eid) => {
                                if let Some(pane) = tab.root.find_pane_mut(&pid) {
                                    pane.execution_id = eid;
                                }
                            }
                            Err(e) => {
                                tracing::error!("restore pane {} failed: {}", pid, e);
                                failed.push(pid);
                            }
                        }
                    } else {
                        // Agent panes: re-spawn from the stored launch config
                        // (pane metadata, Phase 2B §36). No secrets are ever
                        // persisted — the launch holds only references.
                        let launch: Option<AgentLaunchConfig> = tab
                            .root
                            .find_pane(&pid)
                            .and_then(|p| p.metadata.get("agent"))
                            .and_then(|a| a.get("launch"))
                            .and_then(|l| serde_json::from_value(l.clone()).ok());
                        match launch {
                            Some(l) => {
                                match self.spawn_agent_session(l, DEFAULT_COLS, DEFAULT_ROWS) {
                                    Ok(eid) => {
                                        if let Some(pane) = tab.root.find_pane_mut(&pid) {
                                            pane.execution_id = eid;
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!("restore agent pane {} failed: {}", pid, e);
                                        failed.push(pid);
                                    }
                                }
                            }
                            None => {
                                tracing::warn!(
                                    "agent pane {} has no stored launch config — not restored",
                                    pid
                                );
                                failed.push(pid);
                            }
                        }
                    }
                }
            }
            new_workspaces.push(ws);
        }
        self.workspaces = new_workspaces;
        self.active_workspace = 0;
        // Phase 3A §24: rebuild the scheduler from disk. Running/Waiting
        // tasks are marked Interrupted (nothing resumes silently); Ready
        // tasks are re-queued. Task events from the transition are held
        // until the next drain.
        if let Some(tasks) = state.tasks {
            self.tasks = TaskScheduler::import_persisted(tasks);
            for event in self.tasks.take_events() {
                self.events.publish(ApplicationEvent::TaskEvent { event });
            }
        }
        // Phase 3C §49–§50: rebuild the worktree manager from disk and
        // reconnect ownership through the restored scheduler's task ids.
        // Worktrees with no live owner are marked orphaned (never
        // deleted — surfaced for review, §31).
        if let Some(records) = state.worktrees {
            self.worktrees = WorktreeManager::from_records(records);
            let ownership: HashMap<String, String> = self
                .tasks
                .graph()
                .list_tasks()
                .iter()
                .filter_map(|t| t.worktree_id.as_ref().map(|w| (w.clone(), t.id.clone())))
                .collect();
            self.worktrees.scan(&ownership);
        }
        // Phase 3D §35–§36: artifact metadata, review reports and replan
        // signals survive restarts (payloads re-read from worktrees on
        // demand; running synthesis is never silently resumed).
        if let Some(records) = state.artifacts {
            self.artifacts = ArtifactStore::from_metadata(records);
        }
        if let Some(reports) = state.review_reports {
            self.review_reports = reports;
        }
        if let Some(signals) = state.replan_signals {
            self.replan_signals = signals;
        }
        if let Some(p) = state.adaptive {
            self.adaptive.signals = p.signals;
            self.adaptive.plan_versions = p.plan_versions;
            self.adaptive.proposals = p.proposals;
            self.adaptive.task_invalidations = p.task_invalidations;
            self.adaptive.artifact_invalidations = p.artifact_invalidations;
            self.adaptive.escalations = p.escalations;
            self.adaptive.autonomy = p.autonomy;
            self.adaptive.limits = p.limits;
            self.adaptive.metrics = p.metrics;
            self.adaptive.quality = p.quality;
            self.adaptive.limit_reached = p.limit_reached;
        }
        // Phase 4 §17: restore the (redacted, bounded) audit trail. The
        // writer re-redacts defensively on restore, so a persisted trail
        // cannot smuggle secrets into history (§40).
        if let Some(events) = state.audit {
            self.audit = terminal_session::audit::AuditTrail::from_events(events);
            self.audit.record_kind(
                terminal_session::audit::AuditEventKind::WorkflowResumed,
                self.active_workspace().id.clone(),
                "application restarted — audit trail restored",
                "engine",
            );
        }
        // Phase 4 §25: restore policy — autonomy, filesystem scope,
        // network/secret/budget policy, budget ledger and pending
        // approvals. Pending approvals are surfaced to the user again
        // (never auto-executed); a planner cannot change this state.
        if let Some(p) = state.policy {
            let restored = terminal_session::policy::PolicyEngine::from(p);
            let pending = restored.approvals.pending_count();
            tracing::info!(
                "restored policy: autonomy={:?} pending_approvals={}",
                restored.autonomy,
                pending
            );
            if pending > 0 {
                self.audit.record_kind(
                    terminal_session::audit::AuditEventKind::ApprovalRequested,
                    self.active_workspace().id.clone(),
                    format!("{pending} pending approval(s) restored after restart"),
                    "engine",
                );
            }
            self.policy = restored;
        }
        failed
    }

    /// Current live terminal session count (for telemetry).
    pub fn terminal_session_count(&self) -> usize {
        self.terminal_sessions.len()
    }

    /// Total un-applied events buffered across all sessions (for telemetry
    /// and tests — how far the UI is behind the readers).
    pub fn session_pending_total(&self) -> usize {
        self.terminal_sessions
            .values()
            .map(|s| s.pending_len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SplitDirection::*;

    #[test]
    fn create_workspace_spawns_session() {
        let mut m = Multiplexer::new().unwrap();
        let id = m.create_workspace("t", "/tmp").unwrap();
        assert_eq!(m.workspace(&id).unwrap().tabs.len(), 1);
        assert_eq!(m.terminal_session_count(), 1);
        assert!(m.focused_pane().is_some());
    }

    #[test]
    fn split_and_close() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let first = m.focused_pane().unwrap();
        let second = m.split_pane(Horizontal).unwrap();
        assert_eq!(m.terminal_session_count(), 2);
        assert_ne!(first, second);
        assert_eq!(m.focused_pane(), Some(second.clone()));
        m.close_pane(&second).unwrap();
        assert_eq!(m.terminal_session_count(), 1);
        assert_eq!(m.focused_pane(), Some(first));
    }

    #[test]
    fn close_last_pane_closes_tab() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let p = m.focused_pane().unwrap();
        m.close_pane(&p).unwrap();
        assert_eq!(m.active_workspace().tabs.len(), 0);
        assert_eq!(m.terminal_session_count(), 0);
    }

    #[test]
    fn close_last_workspace_rejected() {
        let mut m = Multiplexer::new().unwrap();
        let id = m.create_workspace("only", "/tmp").unwrap();
        assert!(m.close_workspace(&id).is_err());
    }

    #[test]
    fn persistence_roundtrip_via_engine() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("w", "/tmp").unwrap();
        let second = m.split_pane(Vertical).unwrap();
        let state = m.snapshot_state();
        assert_eq!(state.workspaces.len(), 1);
        assert_eq!(state.workspaces[0].tabs[0].root.pane_count(), 2);
        let mut m2 = Multiplexer::new().unwrap();
        let failed = m2.restore(state);
        assert!(failed.is_empty());
        assert_eq!(m2.terminal_session_count(), 2);
        let tab = &m2.active_workspace().tabs[0];
        assert!(tab.root.find_pane(&second).is_some());
    }

    #[test]
    fn agent_pane_restore_respawns_from_stored_launch() {
        // Needs the deterministic fake-agent binary (same skip policy as the
        // terminal-session runtime tests).
        if terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_err() {
            eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
            return;
        }
        let launch = AgentLaunchConfig {
            definition_id: "fake-agent".to_string(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            arguments: vec!["--scenario".to_string(), "working".to_string()],
            provider_id: None,
            model_id: None,
            credential_ref: None,
            resume_id: None,
            environment: vec![],
        };
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("w", "/tmp").unwrap();
        let pane_id = m.split_pane_agent(Vertical, launch).unwrap();
        let state = m.snapshot_state();

        // Restore into a fresh engine: the agent pane must respawn with a
        // live session and a runtime entry under the same pane.
        let mut m2 = Multiplexer::new().unwrap();
        let failed = m2.restore(state);
        assert!(failed.is_empty(), "agent pane must restore: {failed:?}");
        assert_eq!(m2.terminal_session_count(), 2);
        let eid = {
            let tab = &m2.active_workspace().tabs[0];
            let pane = tab.root.find_pane(&pane_id).expect("agent pane present");
            assert_eq!(pane.execution_kind, ExecutionKind::Agent);
            pane.execution_id.clone()
        };
        assert!(
            m2.agent_runtime().get_session(&eid).is_some(),
            "restored agent registered in the runtime"
        );
        assert!(
            !m2.terminal_session_for_pane(&pane_id).unwrap().has_exited(),
            "restored agent session is live"
        );
        // And it is actually working (activity detection on the new stream).
        let mut saw_working = false;
        for _ in 0..200 {
            m2.drain_frame();
            let snap = m2.agent_runtime().get_session(&eid).unwrap();
            if snap.activity == "Working" {
                saw_working = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_working, "restored agent reached Working activity");
    }

    #[test]
    fn drain_empty_is_false() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        // The shell prompt may land in the channel at any moment, so first
        // drain everything until quiescent, then assert a steady-state
        // drain reports nothing changed (no busy-looping on an idle engine).
        let mut quiet_frames = 0u32;
        for _ in 0..200 {
            let r = m.drain_frame();
            if !r.changed && r.events_applied == 0 && m.session_pending_total() == 0 {
                quiet_frames += 1;
                if quiet_frames >= 5 {
                    return;
                }
            } else {
                quiet_frames = 0;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("engine never reached a quiet steady state");
    }

    #[test]
    fn tab_cycle() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let t1 = m.active_tab_id().unwrap();
        let t2 = m.new_tab().unwrap();
        assert_ne!(t1, t2);
        assert_eq!(m.terminal_session_count(), 2);
        m.next_tab().unwrap();
        m.previous_tab().unwrap();
        m.close_tab(&t2).unwrap();
        assert_eq!(m.terminal_session_count(), 1);
    }

    #[test]
    fn zoom_and_swap() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let a = m.focused_pane().unwrap();
        let b = m.split_pane(Horizontal).unwrap();
        m.zoom_pane(&b).unwrap();
        let tab = m.active_workspace().active_tab().unwrap();
        assert_eq!(tab.metadata["zoom"], serde_json::json!(b));
        m.zoom_pane(&b).unwrap();
        m.swap_panes(&a, &b).unwrap();
        assert!(m.terminal_session_for_pane(&a).is_some());
        assert!(m.terminal_session_for_pane(&b).is_some());
    }

    #[test]
    fn focus_cycle_works() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let a = m.focused_pane().unwrap();
        let b = m.split_pane(Horizontal).unwrap();
        let c = m.split_pane(Vertical).unwrap();
        assert_eq!(m.focused_pane(), Some(c));
        m.focus_next().unwrap();
        let after = m.focused_pane().unwrap();
        assert!(after == a || after == b);
        m.focus_next().unwrap();
        m.focus_next().unwrap();
        m.focus_previous().unwrap();
        assert!(m.focused_pane().is_some());
    }

    #[test]
    fn workspace_rename_switch_close() {
        let mut m = Multiplexer::new().unwrap();
        let a = m.create_workspace("a", "/tmp").unwrap();
        let b = m.create_workspace("b", "/tmp").unwrap();
        m.rename_workspace(&a, "renamed").unwrap();
        assert_eq!(m.workspace(&a).unwrap().name, "renamed");
        m.switch_workspace(&a).unwrap();
        assert_eq!(m.active_workspace().id, a);
        // Closing a non-active workspace is fine; the active one stays.
        m.close_workspace(&b).unwrap();
        assert_eq!(m.workspaces().len(), 1);
        assert_eq!(m.active_workspace().id, a);
    }

    #[test]
    fn tab_reorder_and_switch() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let t1 = m.active_tab_id().unwrap();
        let t2 = m.new_tab().unwrap();
        let t3 = m.new_tab().unwrap();
        m.reorder_tab(&t3, 0).unwrap();
        let ids: Vec<String> = m
            .active_workspace()
            .tabs
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(ids, vec![t3.clone(), t1.clone(), t2.clone()]);
        m.switch_tab(&t2).unwrap();
        assert_eq!(
            m.active_workspace().active_tab.as_deref(),
            Some(t2.as_str())
        );
        m.next_tab().unwrap();
        m.previous_tab().unwrap();
        assert!(m.reorder_tab("nope", 0).is_err());
    }

    #[test]
    fn pane_move_and_resize_and_write() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let a = m.focused_pane().unwrap();
        let b = m.split_pane(Horizontal).unwrap();
        // Write into the focused (new) pane's session — the PTY accepts it.
        m.write_focused(b"echo hi\n");
        assert!(m.terminal_session_for_pane(&b).is_some());
        // Resize the pane grid + state.
        m.resize_pane_grid(&a, 100, 30).unwrap();
        assert_eq!(m.state_for_pane(&a).unwrap().cols, 100);
        assert_eq!(m.state_for_pane(&a).unwrap().rows, 30);
        // Move pane b one step left.
        m.move_pane(&b, false).unwrap();
        let ids: Vec<String> = {
            let mut v = Vec::new();
            m.active_tree().unwrap().panes(&mut v);
            v.into_iter().map(|p| p.id.clone()).collect()
        };
        assert_eq!(ids, vec![b.clone(), a.clone()]);
        // Swap back.
        m.swap_panes(&a, &b).unwrap();
    }

    #[test]
    fn close_tab_terminates_sessions() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let t1 = m.active_tab_id().unwrap();
        let _t2 = m.new_tab().unwrap();
        assert_eq!(m.terminal_session_count(), 2);
        m.close_tab(&t1).unwrap();
        assert_eq!(m.terminal_session_count(), 1);
        assert_eq!(m.active_workspace().tabs.len(), 1);
        // The remaining tab became active.
        assert!(m.active_tab().is_some());
    }

    #[test]
    fn restore_marks_failed_panes() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("w", "/tmp").unwrap();
        let state = m.snapshot_state();
        let mut m2 = Multiplexer::new().unwrap();
        let failed = m2.restore(state);
        assert!(failed.is_empty());
        assert_eq!(m2.workspaces().len(), 1);
        assert_eq!(m2.terminal_session_count(), 1);
        // active_tab preserved from the snapshot
        assert!(m2.active_tab().is_some());
    }

    #[test]
    fn process_exit_is_tracked() {
        let mut m = Multiplexer::new().unwrap();
        m.create_workspace("t", "/tmp").unwrap();
        let pane = m.focused_pane().unwrap();
        let eid = m.terminal_session_for_pane(&pane).unwrap().id().to_string();
        let exec_id = terminal_session::execution::ExecutionId(eid);
        // Ask the shell to exit; drain observes it and records the exit code.
        if let Some(s) = m.terminal_session_for_pane(&pane) {
            s.write(b"exit\n");
        }
        let mut saw_exit = false;
        for _ in 0..200 {
            m.drain_frame();
            if m.session_exit_codes.contains_key(&exec_id) {
                saw_exit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_exit, "shell did not exit within the wait window");
        // The exited session is recorded but the pane tree is untouched.
        assert!(m.active_tree().is_some());
    }
}
