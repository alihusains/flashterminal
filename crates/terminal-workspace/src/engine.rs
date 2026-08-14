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
use pty::PtyManager;
use terminal_core::{DirtyTracker, RenderSnapshot, TerminalState};
use terminal_session::agent::{AgentRegistry, AgentRuntime};
use terminal_session::credential::CredentialStore;
use terminal_session::execution::{ExecutionId, ExecutionKind};
use terminal_session::launch::AgentLaunchConfig;
use terminal_session::orchestration::{
    RuntimeAgentView, SchedulerCommand, SchedulerView, Task, TaskContext, TaskEvent, TaskGraph,
    TaskGraphError, TaskId, TaskPolicy, TaskScheduler, TaskStatus,
};
use terminal_session::planning::{
    classify_intent, normalize_intent, AgentAvailability, AgentSummary, IntentDisposition,
    PersistedPlanState, PlanEditChange, PlanStepStatus, PlanValidator, PlannerAuditRecord,
    PlannerConfig, PlannerConstraints, PlannerContext, PlannerContextBuilder, PlannerContextInput,
    PlannerError, PlannerEvent, PlannerMetrics, PlannerPhase, PlannerProvider, PlannerRequest,
    PlannerState, PlannerStatus, TaskSummary,
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
            layout: LayoutEngine::new(),
            notifications: NotificationCenter::new(),
            events: EventBus::new(),
            metrics: EngineMetrics::default(),
            session_exit_codes: HashMap::new(),
            notified_attention: HashMap::new(),
            wake: wake_arc,
            first_render: true,
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
    pub fn worktree_merge(
        &mut self,
        id: &str,
        target_branch: &str,
    ) -> Result<MergeOutcome, WorktreeError> {
        self.worktrees.merge(id, target_branch)
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
            view.running.insert(
                task_id,
                RuntimeAgentView {
                    state,
                    exit_code: self
                        .agent_runtime
                        .get_session(&eid)
                        .and_then(|s| s.exit_code),
                    work: self.agent_runtime.get_work(&eid),
                    estimated_cost_cents: self.agent_runtime.estimated_cost_cents(&eid),
                },
            );
        }
        let cmds = self.tasks.step(&view);
        self.execute_commands(cmds);
        self.publish_task_events();
    }

    fn execute_commands(&mut self, cmds: Vec<SchedulerCommand>) {
        for cmd in cmds {
            match cmd {
                SchedulerCommand::SpawnTask { task_id } => match self.spawn_task_agent(&task_id) {
                    Ok(eid) => self.tasks.note_spawned(&task_id, &eid),
                    Err(e) => self.tasks.note_spawn_failed(&task_id, e),
                },
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
                }
                TaskEvent::TaskFailed { task_id, .. } | TaskEvent::TaskCancelled { task_id } => {
                    self.preserve_worktree(task_id);
                }
                _ => {}
            }
            self.events.publish(ApplicationEvent::TaskEvent { event });
        }
        self.maybe_finish_plan();
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
                if !result.files_changed.is_empty() {
                    result.files_changed = diff.files_changed.clone();
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
