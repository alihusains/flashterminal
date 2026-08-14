//! FlashTerminal Desktop Application (Phase 2B.1: multiplexer + agent UX).
//!
//! Ownership (Phase 1 spec §3, §8–§10):
//!
//! ```text
//! Window ──▶ Active Workspace ──▶ Active Tab ──▶ Pane tree (engine)
//! Pane ── references ──▶ TerminalSession (engine owns PTY+parser+state)
//! Renderer ── renders ──▶ one frame of all pane snapshots (shared atlas)
//! ```
//!
//! * The UI thread owns the `Multiplexer` (behind a mutex shared with the
//!   IPC control server), calls `drain_frame()` once per frame, then renders
//!   every pane's snapshot in a single GPU frame (§28 coalescing).
//! * The renderer never owns pane state; it consumes immutable
//!   `RenderSnapshot`s plus origin rectangles (§12).
//! * Keyboard input is routed through the `CommandRegistry` first (app
//!   shortcuts), then to the focused pane's session (§9, §15).
//! * State persists to `~/.flashterminal/state.json` (versioned, §30).
//! * Agent panes (2B.1 §15–§23) get a chrome header with a live state
//!   badge, capability-gated Stop / Restart / Resume controls, a
//!   permission prompt bar with Allow/Deny, and completion/failure
//!   indicators; the sidebar lists agent sessions of the active tab with
//!   an info panel line (provider · model · confidence). The raw agent
//!   terminal stream is always the pane itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use arboard::Clipboard;
use terminal_renderer::{CursorStyle, Renderer, ViewportRender};
use terminal_workspace::command::{Command, CommandRegistry, KeyChord};
use terminal_workspace::engine::AgentDashboard;
use terminal_workspace::ipc;
use terminal_workspace::model::SplitDirection;
use terminal_workspace::terminal_session::agent::{AgentSnapshot, PermissionDecision};
use terminal_workspace::terminal_session::execution::ExecutionId;
use terminal_workspace::terminal_session::orchestration::Task;
use terminal_workspace::terminal_session::work::{AgentFilter, AgentHealthRow};
use terminal_workspace::{Multiplexer, Rect};
use winit::{
    dpi::PhysicalSize,
    event::{ElementState, Event, KeyEvent, Modifiers, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder},
    keyboard::{Key, NamedKey},
    window::{Window, WindowBuilder},
};

const FONT_SIZE_PX: f32 = 14.0;
const GLYPH_CACHE_BUDGET: usize = 8 * 1024 * 1024;
const BLINK_INTERVAL_MS: u64 = 500;
const SIDEBAR_W: f32 = 200.0;
const TAB_STRIP_H: f32 = 28.0;

/// Agent pane chrome (§15): header height reserved above the viewport.
const AGENT_HEADER_H: f32 = 24.0;
const AGENT_BTN_W: f32 = 52.0;
const AGENT_BTN_H: f32 = 16.0;
/// Permission prompt bar (§17–18): bottom strip of an agent pane in
/// `NeedsApproval`.
const AGENT_PERM_BAR_H: f32 = 26.0;
const AGENT_PERM_BTN_W: f32 = 76.0;
const AGENT_PERM_BTN_H: f32 = 18.0;

const CHROME_BG: [f32; 4] = [0.07, 0.07, 0.09, 1.0];
const CHROME_ACCENT: [f32; 4] = [0.22, 0.55, 0.95, 1.0];
const CHROME_FG: [f32; 4] = [0.72, 0.74, 0.80, 1.0];
const CHROME_FG_DIM: [f32; 4] = [0.45, 0.47, 0.52, 1.0];
const FOCUS_BORDER: [f32; 4] = [0.35, 0.65, 1.0, 0.9];

/// Agent state badge colors (§16) — one per terminal state.
const STATE_STARTING: [f32; 4] = [0.95, 0.76, 0.25, 1.0];
const STATE_WORKING: [f32; 4] = [0.35, 0.85, 0.45, 1.0];
const STATE_WAITING: [f32; 4] = [0.45, 0.60, 0.75, 1.0];
const STATE_APPROVAL: [f32; 4] = [0.95, 0.55, 0.25, 1.0];
const STATE_DONE: [f32; 4] = [0.45, 0.62, 0.45, 1.0];
const STATE_FAILED: [f32; 4] = [0.90, 0.35, 0.35, 1.0];
const STATE_STOPPED: [f32; 4] = [0.45, 0.47, 0.52, 1.0];
const PERM_BAR_BG: [f32; 4] = [0.28, 0.20, 0.06, 0.96];
const AGENT_HEADER_BG: [f32; 4] = [0.11, 0.11, 0.14, 1.0];

/// Agent chrome controls (§19). Pause intentionally has no control: the
/// runtime does not fake a pause capability (engine docs), so the desktop
/// never surfaces one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AgentButton {
    Stop,
    Restart,
    Resume,
}

/// Overlay modes for full-screen UI (Phase 2C).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayMode {
    EmptyState,
    ProviderSetup,
    /// Phase 3A (§55): minimal task dashboard.
    Tasks,
}

/// Provider setup state (Phase 2C §28).
#[derive(Debug, Clone, Default)]
struct ProviderSetupState {
    selecting_provider: bool,
    configured_providers: Vec<(String, bool)>,
    error: Option<String>,
}

/// Work view content (diff panel) for an agent (Phase 2C §9).
#[derive(Debug, Clone, Default)]
struct WorkView {
    /// Files changed (git diff).
    changed_files: Vec<String>,
    /// Current selected file.
    selected_file: usize,
    /// Diff text.
    diff_text: String,
    /// Success indicator.
    success: bool,
}

/// A chrome click outcome, resolved before any `&mut self` call.
enum AgentControl {
    Button(AgentButton),
    Permission(bool),
    FocusPane,
}

/// One agent pane's chrome layout for the current frame (hit-testing).
struct AgentHit {
    pane_id: String,
    execution_id: String,
    /// Full pane rect (header + viewport + permission bar live inside it).
    pane_rect: Rect,
    header_rect: Rect,
    buttons: Vec<(AgentButton, Rect)>,
    /// `(allow, deny)` click targets while the agent waits for approval.
    permission: Option<(Rect, Rect)>,
}

fn agent_state_color(state: &str) -> [f32; 4] {
    match state {
        "Starting" => STATE_STARTING,
        "Working" => STATE_WORKING,
        "Waiting" => STATE_WAITING,
        "NeedsApproval" => STATE_APPROVAL,
        "Completed" => STATE_DONE,
        "Failed" | "Crashed" => STATE_FAILED,
        "Stopped" => STATE_STOPPED,
        _ => CHROME_FG_DIM,
    }
}

fn agent_running(state: &str) -> bool {
    matches!(state, "Starting" | "Working" | "Waiting" | "NeedsApproval")
}

/// Right-anchored header controls, most destructive last (Stop outermost).
fn header_buttons(snap: Option<&AgentSnapshot>) -> Vec<AgentButton> {
    let Some(s) = snap else {
        return Vec::new();
    };
    let running = agent_running(&s.state);
    let mut out = Vec::new();
    if s.capabilities.restart && !running {
        out.push(AgentButton::Restart);
    }
    if s.capabilities.resume && !running {
        out.push(AgentButton::Resume);
    }
    if s.capabilities.stop && running {
        out.push(AgentButton::Stop);
    }
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        s.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
            + "…"
    }
}

/// Phase 3A §55: task status color in the dashboard.
fn task_state_color(
    status: &terminal_workspace::terminal_session::orchestration::TaskStatus,
) -> [f32; 4] {
    use terminal_workspace::terminal_session::orchestration::TaskStatus::*;
    match status {
        Pending | Ready | Interrupted => CHROME_FG_DIM,
        Running | Waiting => STATE_WORKING,
        NeedsReview => STATE_APPROVAL,
        Blocked => STATE_WAITING,
        Completed => STATE_DONE,
        Failed | Cancelled | Skipped => STATE_FAILED,
    }
}

/// User events delivered to the event loop from other threads.
#[derive(Debug, Clone, Copy)]
enum AppEvent {
    /// A session reader thread enqueued a batch: wake up and redraw.
    SessionData,
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    engine: Arc<Mutex<Multiplexer>>,
    registry: CommandRegistry,
    wake_proxy: Option<winit::event_loop::EventLoopProxy<AppEvent>>,
    selection_anchor: Option<(String, u16, u16)>,
    last_mouse_pos: Option<winit::dpi::PhysicalPosition<f64>>,
    modifiers: Modifiers,
    cursor_style: CursorStyle,
    clipboard: Option<Clipboard>,
    last_render: std::time::Instant,
    /// Cached pane rects from the last layout (hit-testing).
    pane_rects: Vec<(String, Rect)>,
    /// Agent chrome layout for the active tab (2B.1 §15–§23).
    agent_hits: Vec<AgentHit>,
    /// Sidebar agent rows (pane id + click rect); doubles as the info
    /// panel for the focused agent (§23).
    sidebar_agent_hits: Vec<(String, Rect)>,
    /// Agent filter for the dashboard (§37).
    agent_filter: AgentFilter,
    /// Command palette state (§37).
    palette_open: bool,
    palette_query: String,
    palette_selection: usize,
    /// Work view overlay (diff view) for focused agent.
    work_view_visible: bool,
    /// Review data backing the work view (§9).
    work_view: Option<WorkView>,
    /// Developer diagnostics panel (Phase 2C §32).
    diagnostics_visible: bool,
    /// Empty state / setup overlay.
    overlay_mode: Option<OverlayMode>,
    /// Provider configuration UI state.
    provider_setup: ProviderSetupState,
    /// Agent installation status check.
    agent_health: Vec<AgentHealthRow>,
    /// Phase 3A: selected row in the task dashboard (insertion order).
    tasks_selection: usize,
}

impl App {
    fn new(engine: Arc<Mutex<Multiplexer>>) -> Self {
        Self {
            window: None,
            renderer: None,
            engine,
            registry: CommandRegistry::with_defaults(),
            wake_proxy: None,
            selection_anchor: None,
            last_mouse_pos: None,
            modifiers: Modifiers::default(),
            cursor_style: CursorStyle::Block,
            clipboard: Clipboard::new().ok(),
            last_render: std::time::Instant::now(),
            pane_rects: Vec::new(),
            agent_hits: Vec::new(),
            sidebar_agent_hits: Vec::new(),
            agent_filter: AgentFilter::All,
            palette_open: false,
            palette_query: String::new(),
            palette_selection: 0,
            work_view_visible: false,
            work_view: None,
            diagnostics_visible: false,
            overlay_mode: None,
            provider_setup: ProviderSetupState::default(),
            agent_health: Vec::new(),
            tasks_selection: 0,
        }
    }

    fn setup_fonts() -> (terminal_text::FontLibrary, terminal_text::GlyphCache) {
        let mut fonts = terminal_text::FontLibrary::new();
        fonts.scan_system();
        let mut cache = terminal_text::GlyphCache::new(FONT_SIZE_PX, GLYPH_CACHE_BUDGET);
        if let Some(primary) = fonts.primary_monospace(None) {
            cache.set_font(primary);
        }
        (fonts, cache)
    }

    /// Restores persisted state or creates a fresh default workspace.
    fn init_engine(engine: &Arc<Mutex<Multiplexer>>) {
        let state_path = terminal_workspace::persist::default_state_path();
        let mut eng = engine.lock().expect("engine lock");
        match terminal_workspace::persist::load(&state_path) {
            Ok(state) => {
                tracing::info!(
                    "restoring {} workspaces from {}",
                    state.workspaces.len(),
                    state_path.display()
                );
                eng.restore(state);
            }
            Err(e) => {
                tracing::info!("no state to restore ({e}); creating default workspace");
                let cwd = std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                    .to_string_lossy()
                    .to_string();
                let _ = eng.create_workspace("Default", &cwd);
            }
        }
    }

    /// Saves the workspace structure (best-effort; failures are logged).
    fn persist(&self) {
        let path = terminal_workspace::persist::default_state_path();
        if let Ok(eng) = self.engine.lock() {
            if let Err(e) = eng.save(&path) {
                tracing::warn!("persist failed: {}", e);
            }
        }
    }

    /// Applies a window resize to the renderer; pane grids resize on the
    /// next frame from the computed layout.
    fn apply_resize(&mut self, physical: PhysicalSize<u32>) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.resize(physical);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn cell_at(&self, pos: winit::dpi::PhysicalPosition<f64>) -> Option<(u16, u16)> {
        let renderer = self.renderer.as_ref()?;
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return None;
        }
        // Coordinates relative to the pane area (below the tab strip, right
        // of the sidebar).
        let x = pos.x - SIDEBAR_W as f64;
        let y = pos.y - TAB_STRIP_H as f64;
        if x < 0.0 || y < 0.0 {
            return None;
        }
        Some((
            (x / cw as f64).floor() as u16,
            (y / ch as f64).floor() as u16,
        ))
    }

    /// The pane under a mouse position (hit-test against the last layout).
    fn pane_at(&self, pos: winit::dpi::PhysicalPosition<f64>) -> Option<String> {
        self.pane_rects
            .iter()
            .find(|(_, r)| r.contains(pos.x, pos.y))
            .map(|(id, _)| id.clone())
    }

    fn last_mouse_cell(&self) -> Option<(u16, u16)> {
        self.last_mouse_pos.and_then(|pos| self.cell_at(pos))
    }

    fn finish_selection(&mut self) {
        if let Some((pane_id, _, _)) = self.selection_anchor.take() {
            let text = {
                let mut eng = self.engine.lock().expect("engine lock");
                eng.state_for_pane_mut(&pane_id)
                    .map(|st| st.selection_text())
                    .unwrap_or_default()
            };
            if !text.is_empty() {
                if let Some(clip) = &mut self.clipboard {
                    if let Err(e) = clip.set_text(text.clone()) {
                        tracing::warn!("clipboard write failed: {}", e);
                    }
                }
            }
        }
    }

    /// Runs a Phase 1 command against the engine (§15, §16).
    fn run_command(&mut self, cmd: &Command) {
        // Phase 2C agent commands lock the engine internally so `&mut self`
        // helpers never alias an active `MutexGuard`.
        if matches!(
            cmd,
            Command::ShowAgents
                | Command::ShowAgentsNeedingAttention
                | Command::ShowFailedAgents
                | Command::ShowCompletedAgents
                | Command::FocusNextAgent
                | Command::FocusPreviousAgent
                | Command::FocusAgent(_)
                | Command::ToggleAgentWorkView
                | Command::ReviewAgentChanges
                | Command::OpenAgentLogs
                | Command::StopAgent
                | Command::RestartAgent
                | Command::ResumeAgent
                | Command::Approve
                | Command::Deny
                | Command::ToggleQuietMode
                | Command::ToggleCommandPalette
        ) {
            self.dispatch_agent_command(cmd);
            return;
        }
        // Phase 3A task commands: the task dashboard owns a selected task;
        // these lock the engine themselves (never alias a live guard).
        if matches!(
            cmd,
            Command::RunTasks
                | Command::ToggleTasks
                | Command::CancelSelectedTask
                | Command::RetrySelectedTask
                | Command::ApproveSelectedTask
                | Command::RejectSelectedTask
                | Command::OpenSelectedTaskAgent
        ) {
            self.dispatch_task_command(cmd);
            return;
        }
        let result: anyhow::Result<()> = {
            let mut eng = self.engine.lock().expect("engine lock");
            match cmd {
                Command::SplitHorizontal => eng.split_pane(SplitDirection::Horizontal).map(|_| ()),
                Command::SplitVertical => eng.split_pane(SplitDirection::Vertical).map(|_| ()),
                Command::ClosePane => {
                    if let Some(id) = eng.focused_pane() {
                        eng.close_pane(&id)
                    } else {
                        Ok(())
                    }
                }
                Command::FocusNext => eng.focus_next(),
                Command::FocusPrevious => eng.focus_previous(),
                Command::ZoomPane => {
                    if let Some(id) = eng.focused_pane() {
                        eng.zoom_pane(&id)
                    } else {
                        Ok(())
                    }
                }
                Command::ResizePaneLeft => {
                    let pane = eng.focused_pane().unwrap_or_default();
                    eng.resize_pane(&pane, -20.0)
                }
                Command::ResizePaneRight => {
                    let pane = eng.focused_pane().unwrap_or_default();
                    eng.resize_pane(&pane, 20.0)
                }
                Command::ResizePaneUp => {
                    let pane = eng.focused_pane().unwrap_or_default();
                    eng.resize_pane(&pane, -20.0)
                }
                Command::ResizePaneDown => {
                    let pane = eng.focused_pane().unwrap_or_default();
                    eng.resize_pane(&pane, 20.0)
                }
                Command::NewTab => eng.new_tab().map(|_| ()),
                Command::CloseTab => {
                    if let Some(id) = eng.active_tab_id() {
                        eng.close_tab(&id)
                    } else {
                        Ok(())
                    }
                }
                Command::NextTab => eng.next_tab(),
                Command::PreviousTab => eng.previous_tab(),
                Command::NewWorkspace => {
                    let cwd = std::env::current_dir()
                        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                        .to_string_lossy()
                        .to_string();
                    eng.create_workspace("New Workspace", &cwd).map(|_| ())
                }
                Command::CloseWorkspace => {
                    let id = eng.active_workspace().id.clone();
                    eng.close_workspace(&id)
                }
                Command::SwitchWorkspace(id) => eng.switch_workspace(id),
                _ => Ok(()),
            }
        };
        if let Err(e) = result {
            tracing::debug!("command {cmd:?} failed: {e}");
        }
        self.persist();
    }

    /// Phase 2C agent commands (§36–§37): each locks the engine itself, so
    /// `&mut self` helpers never alias an active `MutexGuard`.
    fn dispatch_agent_command(&mut self, cmd: &Command) {
        let result: anyhow::Result<()> = match cmd {
            Command::ShowAgents => {
                self.agent_dashboard_filter(AgentFilter::All);
                Ok(())
            }
            Command::ShowAgentsNeedingAttention => {
                self.agent_dashboard_filter(AgentFilter::NeedsAttention);
                Ok(())
            }
            Command::ShowFailedAgents => {
                self.agent_dashboard_filter(AgentFilter::Failed);
                Ok(())
            }
            Command::ShowCompletedAgents => {
                self.agent_dashboard_filter(AgentFilter::Completed);
                Ok(())
            }
            Command::FocusNextAgent => self.cycle_agent(true),
            Command::FocusPreviousAgent => self.cycle_agent(false),
            Command::FocusAgent(pid) => {
                let mut eng = self.engine.lock().expect("engine lock");
                if pid.is_empty() {
                    // Palette "Focus Agent": focus the first agent pane.
                    let target = eng
                        .agent_dashboard(AgentFilter::All)
                        .rows
                        .into_iter()
                        .find_map(|r| r.pane_id);
                    match target {
                        Some(pane) => eng.focus_pane(&pane).map(|_| ()),
                        None => Ok(()),
                    }
                } else {
                    eng.focus_pane(pid).map(|_| ())
                }
            }
            Command::ToggleAgentWorkView | Command::ReviewAgentChanges => {
                self.toggle_work_view_focused();
                Ok(())
            }
            Command::OpenAgentLogs => {
                self.open_logs_focused();
                Ok(())
            }
            Command::StopAgent => {
                self.agent_action_focused(AgentButton::Stop);
                Ok(())
            }
            Command::RestartAgent => {
                self.agent_action_focused(AgentButton::Restart);
                Ok(())
            }
            Command::ResumeAgent => {
                self.agent_action_focused(AgentButton::Resume);
                Ok(())
            }
            Command::Approve => {
                self.permission_focused(true);
                Ok(())
            }
            Command::Deny => {
                self.permission_focused(false);
                Ok(())
            }
            Command::ToggleQuietMode => {
                let mut eng = self.engine.lock().expect("engine lock");
                let prefs = eng.notification_prefs();
                let quiet = !(prefs.on_needs_me && prefs.on_failure);
                let new_prefs = if quiet {
                    terminal_workspace::notify::NotificationPrefs::default()
                } else {
                    terminal_workspace::notify::NotificationPrefs {
                        on_needs_me: false,
                        on_failure: false,
                        on_completion: false,
                        on_start: false,
                    }
                };
                eng.set_notification_prefs(&new_prefs);
                Ok(())
            }
            Command::ToggleCommandPalette => {
                self.toggle_palette();
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            tracing::debug!("agent command {cmd:?} failed: {e}");
        }
        self.persist();
    }

    /// Phase 3A task commands (§43, §55 — minimal UI): each locks the
    /// engine itself, so `&mut self` helpers never alias a live guard.
    fn dispatch_task_command(&mut self, cmd: &Command) {
        let result: anyhow::Result<()> = match cmd {
            Command::RunTasks => {
                let mut eng = self.engine.lock().expect("engine lock");
                eng.task_run();
                // Show the dashboard so progress is visible.
                self.overlay_mode = Some(OverlayMode::Tasks);
                self.tasks_selection = 0;
                Ok(())
            }
            Command::ToggleTasks => {
                if self.overlay_mode == Some(OverlayMode::Tasks) {
                    self.overlay_mode = None;
                } else {
                    self.overlay_mode = Some(OverlayMode::Tasks);
                    self.tasks_selection = 0;
                }
                Ok(())
            }
            Command::CancelSelectedTask => {
                let mut eng = self.engine.lock().expect("engine lock");
                match self.selected_task_id(&eng) {
                    Some(id) => {
                        let _ = eng.task_cancel(&id);
                        Ok(())
                    }
                    None => Ok(()),
                }
            }
            Command::RetrySelectedTask => {
                let mut eng = self.engine.lock().expect("engine lock");
                match self.selected_task_id(&eng) {
                    Some(id) => {
                        let _ = eng.task_retry(&id);
                        Ok(())
                    }
                    None => Ok(()),
                }
            }
            Command::ApproveSelectedTask => self.resolve_selected_review(true),
            Command::RejectSelectedTask => self.resolve_selected_review(false),
            Command::OpenSelectedTaskAgent => {
                let mut eng = self.engine.lock().expect("engine lock");
                match self.selected_task_id(&eng) {
                    Some(id) => {
                        let _ = eng.attach_task_agent_pane(&id);
                        Ok(())
                    }
                    None => Ok(()),
                }
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            tracing::debug!("task command {cmd:?} failed: {e}");
        }
        self.persist();
    }

    /// The task id selected in the dashboard (insertion order of the
    /// schedulers status — deterministic §10).
    fn selected_task_id(
        &self,
        eng: &Multiplexer,
    ) -> Option<terminal_session::orchestration::TaskId> {
        eng.scheduler_status()
            .states
            .get(self.tasks_selection)
            .map(|(id, _)| id.clone())
    }

    fn resolve_selected_review(&mut self, approve: bool) -> anyhow::Result<()> {
        let mut eng = self.engine.lock().expect("engine lock");
        if let Some(id) = self.selected_task_id(&eng) {
            let _ = eng.resolve_task_review(&id, approve);
        }
        Ok(())
    }

    /// Maps a winit key event to a `KeyChord` (macOS Cmd = super → ctrl).
    fn chord_from(&self, key: &Key) -> Option<KeyChord> {
        let m = self.modifiers.state();
        let name = match key {
            Key::Character(c) => c.to_string(),
            Key::Named(n) => format!("{n:?}"),
            _ => return None,
        };
        Some(KeyChord {
            ctrl: m.control_key() || m.super_key(),
            alt: m.alt_key(),
            shift: m.shift_key(),
            key: name,
        })
    }

    /// The window content rect minus chrome (sidebar + tab strip).
    fn content_rect(&self, window_size: PhysicalSize<u32>) -> Rect {
        Rect {
            x: SIDEBAR_W as i32,
            y: TAB_STRIP_H as i32,
            width: window_size.width.saturating_sub(SIDEBAR_W as u32),
            height: window_size.height.saturating_sub(TAB_STRIP_H as u32),
        }
    }

    fn drain_and_render(&mut self, now: std::time::Instant) {
        let window_size = self
            .window
            .as_ref()
            .map(|w| w.inner_size())
            .unwrap_or_default();
        let content = self.content_rect(window_size);

        // 1. Drain all sessions (fairness-aware, batched) — §27, §28.
        let changed = {
            let mut eng = self.engine.lock().expect("engine lock");
            let r = eng.drain_frame();
            // 2. Layout the active tab's panes.
            let pane_rects = eng.layout_active(content);
            self.pane_rects = pane_rects
                .iter()
                .map(|pr| (pr.pane_id.clone(), pr.rect))
                .collect();
            // 3. Agent panes (2B.1 §15–§23): reserve the chrome header, build
            //    hit targets, and mark the agents for the sidebar panel.
            let mut pane_info: HashMap<String, (String, bool)> = HashMap::new();
            let mut sidebar_agents: Vec<(String, String)> = Vec::new();
            if let Some(tab) = eng.active_tab() {
                let mut panes = Vec::new();
                tab.root.panes(&mut panes);
                for p in panes {
                    let is_agent = p.metadata.get("agent").is_some();
                    pane_info.insert(p.id.clone(), (p.execution_id.0.clone(), is_agent));
                    if is_agent {
                        sidebar_agents.push((p.id.clone(), p.execution_id.0.clone()));
                    }
                }
            }
            let mut viewport_rects: Vec<(String, Rect)> = Vec::with_capacity(self.pane_rects.len());
            let mut agent_hits: Vec<AgentHit> = Vec::new();
            for (pid, rect) in &self.pane_rects {
                let Some((eid, true)) = pane_info.get(pid) else {
                    viewport_rects.push((pid.clone(), *rect));
                    continue;
                };
                let header_rect = Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: AGENT_HEADER_H as u32,
                };
                viewport_rects.push((
                    pid.clone(),
                    Rect {
                        x: rect.x,
                        y: rect.y + AGENT_HEADER_H as i32,
                        width: rect.width,
                        height: rect.height.saturating_sub(AGENT_HEADER_H as u32),
                    },
                ));
                let snap = eng.agent_runtime().get_session(&ExecutionId(eid.clone()));
                let mut buttons = Vec::new();
                let mut bx = rect.x + rect.width as i32 - AGENT_BTN_W as i32 - 6;
                let by = rect.y + ((AGENT_HEADER_H - AGENT_BTN_H) / 2.0) as i32;
                for btn in header_buttons(snap.as_ref()) {
                    buttons.push((
                        btn,
                        Rect {
                            x: bx,
                            y: by,
                            width: AGENT_BTN_W as u32,
                            height: AGENT_BTN_H as u32,
                        },
                    ));
                    bx -= AGENT_BTN_W as i32 + 6;
                }
                let permission = if snap
                    .as_ref()
                    .map(|s| s.state == "NeedsApproval")
                    .unwrap_or(false)
                {
                    let py = rect.y + rect.height as i32 - AGENT_PERM_BAR_H as i32;
                    let allow = Rect {
                        x: rect.x + rect.width as i32 - 2 * AGENT_PERM_BTN_W as i32 - 12,
                        y: py + 4,
                        width: AGENT_PERM_BTN_W as u32,
                        height: AGENT_PERM_BTN_H as u32,
                    };
                    let deny = Rect {
                        x: allow.x + AGENT_PERM_BTN_W as i32 + 4,
                        y: py + 4,
                        width: AGENT_PERM_BTN_W as u32,
                        height: AGENT_PERM_BTN_H as u32,
                    };
                    Some((allow, deny))
                } else {
                    None
                };
                agent_hits.push(AgentHit {
                    pane_id: pid.clone(),
                    execution_id: eid.clone(),
                    pane_rect: *rect,
                    header_rect,
                    buttons,
                    permission,
                });
            }
            self.agent_hits = agent_hits;
            // 4. Resize pane grids to their viewport rects (§13 — fast
            //    ioctls only). Agent panes grid to the space under the
            //    chrome header.
            if let Some(renderer) = &self.renderer {
                let (cw, ch) = renderer.cell_size();
                for (pid, rect) in &viewport_rects {
                    let px = rect.width as f64;
                    let py = rect.height as f64;
                    if cw > 0.0 && ch > 0.0 && px >= cw as f64 && py >= ch as f64 {
                        let cols = (px / cw as f64).floor() as u16;
                        let rows = (py / ch as f64).floor() as u16;
                        let _ = eng.resize_pane_grid(pid, cols.max(1), rows.max(1));
                    }
                }
            }
            // 5+6. Chrome (sidebar + tab strip + agent chrome + focus border), then
            //    one frame of all pane viewports (snapshot + consumed dirty
            //    tracker, produced by the engine in a single borrow) rendered
            //    with the shared atlas — one surface present (§10, §28).
            let sidebar_hits = {
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                renderer.begin_chrome();
                let (_, ch_px) = renderer.cell_size();
                let focused = eng.focused_pane();
                let sidebar_hits =
                    App::draw_sidebar_chrome(renderer, &eng, window_size, ch_px, &sidebar_agents);
                App::draw_agent_chrome(renderer, &eng, &self.agent_hits);
                for (id, rect) in &self.pane_rects {
                    if Some(id) == focused.as_ref() {
                        renderer.chrome_border(
                            rect.x as f32,
                            rect.y as f32,
                            rect.width as f32,
                            rect.height as f32,
                            2.0,
                            FOCUS_BORDER,
                        );
                    }
                }
                let frames = eng.pane_frames(&viewport_rects);
                let mut viewports: Vec<ViewportRender> = Vec::with_capacity(frames.len());
                for f in &frames {
                    viewports.push(ViewportRender {
                        snapshot: &f.snapshot,
                        dirty: &f.dirty,
                        origin: f.origin,
                    });
                }
                let _ = renderer.render_multi(&viewports, now);
                sidebar_hits
            };
            self.sidebar_agent_hits = sidebar_hits;
            r.changed
        };

        // Window title = active workspace name.
        if let Some(window) = &self.window {
            let name = {
                let eng = self.engine.lock().expect("engine lock");
                eng.active_workspace().name.clone()
            };
            window.set_title(&format!("FlashTerminal — {name}"));
        }

        if changed {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }

        // Draw overlays on top of the normal rendering. Each overlay gathers
        // its data first (short-lived borrows of `self`), then draws with a
        // disjoint `&mut self.renderer` borrow.
        let window_size = self
            .window
            .as_ref()
            .map(|w| w.inner_size())
            .unwrap_or_default();
        // Command palette.
        if self.palette_open {
            let commands = self.registry.palette();
            let selection = self.palette_selection;
            let query = self.palette_query.clone();
            if let Some(renderer) = self.renderer.as_mut() {
                App::draw_palette(renderer, window_size, &commands, selection, &query);
            }
        }
        // Empty state overlay.
        if let Some(OverlayMode::EmptyState) = self.overlay_mode {
            let health = self.agent_health.clone();
            if let Some(renderer) = self.renderer.as_mut() {
                App::draw_empty_state(renderer, window_size, &health);
            }
        }
        // Provider setup overlay.
        if let Some(OverlayMode::ProviderSetup) = self.overlay_mode {
            let setup = self.provider_setup.clone();
            if let Some(renderer) = self.renderer.as_mut() {
                App::draw_provider_setup(renderer, window_size, &setup);
            }
        }
        // Phase 3A task dashboard overlay (§55 minimal UI).
        if let Some(OverlayMode::Tasks) = self.overlay_mode {
            let (tasks, status, selection) = {
                let eng = self.engine.lock().expect("engine lock");
                (
                    eng.task_list().into_iter().cloned().collect::<Vec<_>>(),
                    eng.scheduler_status(),
                    self.tasks_selection,
                )
            };
            if let Some(renderer) = self.renderer.as_mut() {
                App::draw_tasks(renderer, window_size, &tasks, &status, selection);
            }
        }
        // Diagnostics panel.
        if self.diagnostics_visible {
            let (dashboard, events_applied, latency_p95, subscribers) = {
                let eng = self.engine.lock().expect("engine lock");
                (
                    eng.agent_dashboard(AgentFilter::All),
                    eng.metrics.events_applied,
                    eng.metrics.apply_latency_p95_us(),
                    eng.events.subscriber_count(),
                )
            };
            if let Some(renderer) = self.renderer.as_mut() {
                App::draw_diagnostics(
                    renderer,
                    window_size,
                    &dashboard,
                    events_applied,
                    latency_p95,
                    subscribers,
                );
            }
        }
        // Work review view.
        if self.work_view_visible {
            let view = self.work_view.clone();
            if let Some(renderer) = self.renderer.as_mut() {
                if let Some(view) = &view {
                    App::draw_work_view(renderer, window_size, view);
                }
            }
        }
    }

    /// Sidebar (§17): workspaces on top, tabs of the active workspace below,
    /// then agent sessions of the active tab (2B.1 §23 info panel). Returns
    /// click rects for the agent rows (focus-on-click).
    fn draw_sidebar_chrome(
        renderer: &mut Renderer,
        eng: &Multiplexer,
        window_size: PhysicalSize<u32>,
        cell_h: f32,
        agents: &[(String, String)],
    ) -> Vec<(String, Rect)> {
        let mut agent_hits = Vec::new();
        let w = SIDEBAR_W;
        let h = window_size.height as f32;
        renderer.chrome_rect(0.0, 0.0, w, h, CHROME_BG);
        // Tab strip across the top (right of the sidebar).
        renderer.chrome_rect(
            w,
            0.0,
            window_size.width as f32 - w,
            TAB_STRIP_H,
            [0.09, 0.09, 0.11, 1.0],
        );

        let active_ws = eng.active_workspace();
        let mut y = 12.0_f32;
        renderer.chrome_text(10.0, y, "WORKSPACES", CHROME_FG_DIM);
        y += cell_h + 6.0;
        for ws in eng.workspaces() {
            let accent = if ws.id == active_ws.id {
                CHROME_ACCENT
            } else {
                CHROME_FG
            };
            renderer.chrome_text(14.0, y, &ws.name, accent);
            y += cell_h + 4.0;
        }
        y += cell_h;
        renderer.chrome_text(10.0, y, "TABS", CHROME_FG_DIM);
        y += cell_h + 6.0;
        for tab in &active_ws.tabs {
            let title = if tab.title.is_empty() {
                format!("tab {}", &tab.id[..tab.id.len().min(6)])
            } else {
                tab.title.clone()
            };
            let focused = active_ws.active_tab.as_deref() == Some(&tab.id);
            renderer.chrome_text(
                14.0,
                y,
                &title,
                if focused {
                    CHROME_ACCENT
                } else {
                    CHROME_FG_DIM
                },
            );
            y += cell_h + 2.0;
        }

        // Agent sessions of the active tab — state badge + info panel.
        y += cell_h;
        renderer.chrome_text(10.0, y, "AGENTS", CHROME_FG_DIM);
        y += cell_h + 6.0;
        // Workspace summary (§14): counts update live with agent state.
        let summary = eng.workspace_agent_summary();
        if summary.agents > 0 {
            renderer.chrome_text(
                10.0,
                y,
                &format!(
                    "{} agents · {} working · {} needs you · {} done · {} failed",
                    summary.agents,
                    summary.running,
                    summary.needs_you,
                    summary.completed,
                    summary.failed
                ),
                CHROME_FG_DIM,
            );
            y += cell_h + 4.0;
        }
        for (pane_id, eid) in agents {
            let snap = eng.agent_runtime().get_session(&ExecutionId(eid.clone()));
            let name = snap
                .as_ref()
                .map(|s| s.display_name.clone())
                .unwrap_or_else(|| "agent".into());
            let state = snap
                .as_ref()
                .map(|s| s.state.clone())
                .unwrap_or_else(|| "?".into());
            let color = snap
                .as_ref()
                .map(|s| agent_state_color(&s.state))
                .unwrap_or(CHROME_FG_DIM);
            renderer.chrome_rect(10.0, y + 3.0, 8.0, 8.0, color);
            renderer.chrome_text(
                24.0,
                y,
                &format!("{} ({state})", truncate(&name, 18)),
                CHROME_FG,
            );
            y += cell_h - 1.0;
            if let Some(s) = snap {
                let line = format!(
                    "{} · {} · src:{}",
                    s.provider_id.as_deref().unwrap_or("no-provider"),
                    s.model_id.as_deref().unwrap_or("default"),
                    s.state_source,
                );
                renderer.chrome_text(24.0, y, &truncate(&line, 24), CHROME_FG_DIM);
                y += cell_h + 1.0;
            } else {
                y += cell_h + 1.0;
            }
            agent_hits.push((
                pane_id.clone(),
                Rect {
                    x: 0,
                    y: (y - cell_h * 2.0) as i32,
                    width: w as u32,
                    height: (cell_h * 2.0 + 4.0) as u32,
                },
            ));
        }
        agent_hits
    }

    /// Agent pane chrome (§15–§23): header + state badge + controls, and
    /// the permission bar when the agent waits for approval.
    fn draw_agent_chrome(renderer: &mut Renderer, eng: &Multiplexer, hits: &[AgentHit]) {
        for hit in hits {
            let r = hit.header_rect;
            let snap = eng
                .agent_runtime()
                .get_session(&ExecutionId(hit.execution_id.clone()));
            let needs_approval = snap
                .as_ref()
                .map(|s| s.state == "NeedsApproval")
                .unwrap_or(false);
            renderer.chrome_rect(
                r.x as f32,
                r.y as f32,
                r.width as f32,
                r.height as f32,
                if needs_approval {
                    PERM_BAR_BG
                } else {
                    AGENT_HEADER_BG
                },
            );
            // State dot + name.
            let color = snap
                .as_ref()
                .map(|s| agent_state_color(&s.state))
                .unwrap_or(CHROME_FG_DIM);
            renderer.chrome_rect(r.x as f32 + 10.0, r.y as f32 + 8.0, 8.0, 8.0, color);
            let name = snap
                .as_ref()
                .map(|s| s.display_name.clone())
                .unwrap_or_else(|| "agent".into());
            // Room for name+info given the right-anchored buttons.
            let btn_w = hit.buttons.len() as f32 * (AGENT_BTN_W + 6.0) + 6.0;
            let room = ((r.width as f32 - 150.0 - btn_w - 40.0) / 7.5).max(6.0) as usize;
            let info = truncate(&name, room);
            let y = r.y as f32 + 5.0;
            renderer.chrome_text(r.x as f32 + 24.0, y, &info, CHROME_FG);
            // Info suffix: state · provider/model · exit code (§16, §22).
            if let Some(s) = &snap {
                let mut cx = r.x as f32 + 24.0 + info.chars().count() as f32 * 7.5;
                let base = format!(
                    " · {} · {}",
                    s.state,
                    s.model_id.as_deref().unwrap_or("default")
                );
                renderer.chrome_text(cx, y, &base, CHROME_FG_DIM);
                cx += base.chars().count() as f32 * 7.5;
                if let Some(code) = s.exit_code {
                    let (label, col) = match s.state.as_str() {
                        "Completed" => (format!(" · exited {code}"), STATE_DONE),
                        "Failed" => (format!(" · failed ({code})"), STATE_FAILED),
                        "Crashed" => (format!(" · crashed ({code})"), STATE_FAILED),
                        "Stopped" => (" · stopped".to_string(), STATE_STOPPED),
                        _ => (format!(" · exit {code}"), CHROME_FG_DIM),
                    };
                    renderer.chrome_text(cx, y, &label, col);
                }
            }
            // Buttons (§19): bordered boxes, capability-gated.
            for (btn, rect) in &hit.buttons {
                let label = match btn {
                    AgentButton::Stop => "Stop",
                    AgentButton::Restart => "Restart",
                    AgentButton::Resume => "Resume",
                };
                let bc = match btn {
                    AgentButton::Stop => STATE_FAILED,
                    AgentButton::Restart => CHROME_ACCENT,
                    AgentButton::Resume => STATE_WORKING,
                };
                renderer.chrome_border(
                    rect.x as f32,
                    rect.y as f32,
                    rect.width as f32,
                    rect.height as f32,
                    1.0,
                    bc,
                );
                renderer.chrome_text(rect.x as f32 + 6.0, rect.y as f32 + 1.0, label, bc);
            }
        }
        // Permission bar (§17–18): full-width strip at the pane bottom,
        // Allow/Deny click targets.
        for hit in hits {
            let Some((allow, deny)) = hit.permission else {
                continue;
            };
            let r = hit.pane_rect;
            let bar_y = (r.y + r.height as i32 - AGENT_PERM_BAR_H as i32) as f32;
            let bar_w = r.width as f32;
            renderer.chrome_rect(r.x as f32, bar_y, bar_w, AGENT_PERM_BAR_H, PERM_BAR_BG);
            renderer.chrome_text(
                r.x as f32 + 10.0,
                bar_y + 6.0,
                "agent requests permission",
                STATE_APPROVAL,
            );
            renderer.chrome_border(
                allow.x as f32,
                allow.y as f32,
                allow.width as f32,
                allow.height as f32,
                1.0,
                STATE_WORKING,
            );
            renderer.chrome_text(
                allow.x as f32 + 18.0,
                allow.y as f32 + 2.0,
                "Allow",
                STATE_WORKING,
            );
            renderer.chrome_border(
                deny.x as f32,
                deny.y as f32,
                deny.width as f32,
                deny.height as f32,
                1.0,
                STATE_FAILED,
            );
            renderer.chrome_text(
                deny.x as f32 + 18.0,
                deny.y as f32 + 2.0,
                "Deny",
                STATE_FAILED,
            );
        }
    }

    /// Chrome hit-testing: agent controls, permission bar, sidebar agent
    /// rows. Returns true when a chrome element consumed the click.
    fn chrome_click(&mut self, pos: winit::dpi::PhysicalPosition<f64>) -> bool {
        let picked = self
            .agent_hits
            .iter()
            .find_map(|hit| {
                if let Some((btn, _)) = hit.buttons.iter().find(|(_, r)| r.contains(pos.x, pos.y)) {
                    return Some((hit.pane_id.clone(), AgentControl::Button(*btn)));
                }
                if let Some((allow, deny)) = hit.permission {
                    if allow.contains(pos.x, pos.y) {
                        return Some((hit.pane_id.clone(), AgentControl::Permission(true)));
                    }
                    if deny.contains(pos.x, pos.y) {
                        return Some((hit.pane_id.clone(), AgentControl::Permission(false)));
                    }
                }
                None
            })
            .or_else(|| {
                self.sidebar_agent_hits
                    .iter()
                    .find(|(_, r)| r.contains(pos.x, pos.y))
                    .map(|(pane_id, _)| (pane_id.clone(), AgentControl::FocusPane))
            });
        if let Some((pane_id, control)) = picked {
            match control {
                AgentControl::Button(btn) => self.agent_action(&pane_id, btn),
                AgentControl::Permission(allow) => self.respond_permission(&pane_id, allow),
                AgentControl::FocusPane => {
                    let mut eng = self.engine.lock().expect("engine lock");
                    let _ = eng.focus_pane(&pane_id);
                    drop(eng);
                    self.persist();
                }
            }
            return true;
        }
        false
    }

    /// Capability-gated control execution (§19) — goes through the engine,
    /// never directly at the process.
    fn agent_action(&mut self, pane_id: &str, btn: AgentButton) {
        let result: anyhow::Result<()> = {
            let mut eng = self.engine.lock().expect("engine lock");
            let pid = pane_id.to_string();
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                tracing::debug!("agent action {btn:?} on {pane_id}: no execution");
                return;
            };
            match btn {
                AgentButton::Stop => eng.agent_runtime_mut().stop(&eid),
                AgentButton::Restart => eng.restart_agent_session(&eid),
                AgentButton::Resume => eng.resume_agent_session(&eid),
            }
        };
        if let Err(e) = result {
            tracing::debug!("agent action {btn:?} on {pane_id} failed: {e}");
        }
        self.persist();
    }

    /// Sets the agent dashboard filter (§37).
    fn agent_dashboard_filter(&mut self, filter: AgentFilter) {
        self.agent_filter = filter;
    }

    /// Cycles focus across agent panes of the active tab (§36).
    fn cycle_agent(&mut self, next: bool) -> anyhow::Result<()> {
        let mut eng = self.engine.lock().expect("engine lock");
        let mut agents: Vec<String> = Vec::new();
        if let Some(tab) = eng.active_tab() {
            let mut panes = Vec::new();
            tab.root.panes(&mut panes);
            for p in panes {
                if p.metadata.get("agent").is_some() {
                    agents.push(p.id.clone());
                }
            }
        }
        if agents.is_empty() {
            return Ok(());
        }
        let focused = eng.focused_pane();
        let idx = focused
            .as_ref()
            .and_then(|f| agents.iter().position(|a| a == f));
        let next_idx = match idx {
            Some(i) if next => (i + 1) % agents.len(),
            Some(i) => (i + agents.len() - 1) % agents.len(),
            None => 0,
        };
        eng.focus_pane(&agents[next_idx])?;
        Ok(())
    }

    /// Toggles the work view (diff review, §9/§25) for the focused pane.
    fn toggle_work_view_focused(&mut self) {
        let review = {
            let eng = self.engine.lock().expect("engine lock");
            let pid = eng.focused_pane().unwrap_or_default();
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                tracing::debug!("work view: focused pane {pid} has no execution");
                return;
            };
            eng.agent_review(&eid)
        };
        let Some(review) = review else {
            self.work_view_visible = false;
            self.work_view = None;
            return;
        };
        self.work_view = Some(WorkView {
            changed_files: review.files.iter().map(|f| f.path.clone()).collect(),
            selected_file: 0,
            diff_text: review
                .files
                .first()
                .and_then(|f| f.diff.clone())
                .unwrap_or_default(),
            success: true,
        });
        self.work_view_visible = true;
    }

    /// Opens the agent's working-directory log surface (§37). The desktop
    /// falls back to revealing the workspace directory in Finder when the
    /// runtime exposes no per-agent log file yet.
    fn open_logs_focused(&mut self) {
        let (pid, cwd) = {
            let eng = self.engine.lock().expect("engine lock");
            let pid = eng.focused_pane().unwrap_or_default();
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                return;
            };
            let cwd = eng
                .agent_runtime()
                .get_session(&eid)
                .map(|s| s.cwd.clone())
                .unwrap_or_default();
            (pid, cwd)
        };
        tracing::debug!("opening logs for focused agent pane {pid}");
        if !cwd.is_empty() {
            let _ = std::process::Command::new("open").arg(&cwd).spawn();
        }
    }

    /// Performs an agent action on the focused pane (§19).
    fn agent_action_focused(&mut self, btn: AgentButton) {
        let pid = {
            let eng = self.engine.lock().expect("engine lock");
            eng.focused_pane().unwrap_or_default().to_string()
        };
        let result: anyhow::Result<()> = {
            let mut eng = self.engine.lock().expect("engine lock");
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                tracing::debug!("agent action {btn:?} on {pid}: no execution");
                return;
            };
            match btn {
                AgentButton::Stop => eng.agent_runtime_mut().stop(&eid),
                AgentButton::Restart => eng.restart_agent_session(&eid),
                AgentButton::Resume => eng.resume_agent_session(&eid),
            }
        };
        if let Err(e) = result {
            tracing::debug!("agent action {btn:?} on {pid} failed: {e}");
        }
        self.persist();
    }

    /// Applies Allow/Deny to the focused agent's pending permission (§18).
    fn permission_focused(&mut self, allow: bool) {
        let pid = {
            let eng = self.engine.lock().expect("engine lock");
            eng.focused_pane().unwrap_or_default().to_string()
        };
        let result: anyhow::Result<()> = {
            let eng = self.engine.lock().expect("engine lock");
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                tracing::debug!("permission on {pid}: no execution");
                return;
            };
            let decision = if allow {
                PermissionDecision::AllowOnce
            } else {
                PermissionDecision::Deny
            };
            eng.agent_runtime().respond_permission(&eid, decision)
        };
        if let Err(e) = result {
            tracing::debug!("permission on {pid} failed: {e}");
        }
    }

    /// Toggles the command palette (§37).
    fn toggle_palette(&mut self) {
        self.palette_open = !self.palette_open;
        self.palette_selection = 0;
        if self.palette_open {
            self.refresh_palette_commands();
        }
    }

    fn refresh_palette_commands(&mut self) {
        // Palette matches are computed per-frame in `draw_palette` from the
        // query; this hook just resets navigation to the top (§38).
        self.palette_selection = 0;
    }

    /// Opens the empty state overlay (§26) with real agent health rows.
    fn open_empty_state(&mut self) {
        let health = {
            let eng = self.engine.lock().expect("engine lock");
            eng.agent_runtime().health()
        };
        self.agent_health = health;
        self.overlay_mode = Some(OverlayMode::EmptyState);
        self.diagnostics_visible = false;
        self.work_view_visible = false;
        self.palette_open = false;
    }

    /// Opens the provider setup overlay (§28).
    fn open_provider_setup(&mut self) {
        let providers = {
            let eng = self.engine.lock().expect("engine lock");
            eng.agent_runtime().provider_status().clone()
        };
        self.provider_setup = ProviderSetupState {
            selecting_provider: false,
            configured_providers: providers,
            error: None,
        };
        self.overlay_mode = Some(OverlayMode::ProviderSetup);
        self.diagnostics_visible = false;
        self.work_view_visible = false;
        self.palette_open = false;
    }

    /// Closes all overlays.
    fn close_overlays(&mut self) {
        self.overlay_mode = None;
        self.diagnostics_visible = false;
        self.work_view_visible = false;
        self.palette_open = false;
        self.palette_query.clear();
        self.palette_selection = 0;
    }

    /// Draws the command palette overlay (§37): live-query filtered command
    /// list from the registry, arrow-key navigation, Enter runs the command.
    fn draw_palette(
        renderer: &mut Renderer,
        window_size: PhysicalSize<u32>,
        commands: &[Command],
        selection: usize,
        query: &str,
    ) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (35.0_f32 * cw).min(600.0_f32);
        let h = (12.0_f32 * ch).min(400.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.5],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(x + 10.0, y + 8.0, &format!("> {query}"), CHROME_ACCENT);
        let q = query.to_lowercase();
        let matches: Vec<&Command> = commands
            .iter()
            .filter(|cmd| cmd.to_label().to_lowercase().contains(&q))
            .collect();
        let start = selection.saturating_sub(3);
        let end = (start + 10).min(matches.len());
        for (k, cmd) in matches.iter().enumerate().skip(start).take(end - start) {
            let item_y = y + 20.0 + (k as f32 * ch);
            if k == selection {
                renderer.chrome_rect(x + 5.0, item_y, w - 10.0, ch, CHROME_ACCENT);
                renderer.chrome_text(x + 10.0, item_y, cmd.to_label(), [0.0, 0.0, 0.0, 1.0]);
            } else {
                renderer.chrome_text(x + 10.0, item_y, cmd.to_label(), CHROME_FG);
            }
        }
        renderer.chrome_text(
            x + 10.0,
            y + h - 20.0,
            "↑↓ to navigate · Enter to run · Esc to close",
            CHROME_FG_DIM,
        );
    }

    /// Draws the empty state overlay (§26) from real agent health rows —
    /// never faked agents; rows come from the runtime's binary/auth probe.
    fn draw_empty_state(
        renderer: &mut Renderer,
        window_size: PhysicalSize<u32>,
        health: &[AgentHealthRow],
    ) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (44.0_f32 * cw).min(720.0_f32);
        let h = (14.0_f32 * ch).min(480.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.7],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(x + 20.0, y + 16.0, "AI Agents", CHROME_ACCENT);
        renderer.chrome_text(
            x + 20.0,
            y + 36.0,
            "Run an agent in this workspace.",
            CHROME_FG,
        );
        let mut y_pos = y + 62.0;
        for row in health {
            let status = if row.installed {
                if row.credential_configured {
                    "✓ Installed · Authenticated"
                } else {
                    "✓ Installed · auth required"
                }
            } else {
                "not installed"
            };
            let col = if row.installed && row.credential_configured {
                STATE_DONE
            } else if row.installed {
                STATE_APPROVAL
            } else {
                CHROME_FG_DIM
            };
            renderer.chrome_rect(x + 30.0, y_pos, w - 60.0, ch, [0.08, 0.08, 0.10, 1.0]);
            renderer.chrome_border(x + 30.0, y_pos, w - 60.0, ch, 1.0, CHROME_FG_DIM);
            let name = truncate(&row.display_name, 24);
            renderer.chrome_text(x + 40.0, y_pos, &name, CHROME_FG);
            renderer.chrome_text(
                x + 40.0 + name.chars().count() as f32 * 7.5 + 8.0,
                y_pos,
                status,
                col,
            );
            y_pos += ch + 8.0;
        }
        if health.is_empty() {
            renderer.chrome_text(
                x + 30.0,
                y_pos,
                "No agent definitions registered.",
                CHROME_FG_DIM,
            );
        }
        renderer.chrome_text(
            x + 20.0,
            y + h - 20.0,
            "Press Esc to return.",
            CHROME_FG_DIM,
        );
    }

    /// Draws the provider setup overlay (§28): per-provider credential
    /// presence (never secrets).
    fn draw_provider_setup(
        renderer: &mut Renderer,
        window_size: PhysicalSize<u32>,
        state: &ProviderSetupState,
    ) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (40.0_f32 * cw).min(600.0_f32);
        let h = (12.0_f32 * ch).min(400.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.7],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(x + 20.0, y + 20.0, "Configure AI Provider", CHROME_ACCENT);
        if state.selecting_provider {
            renderer.chrome_text(
                x + 20.0,
                y + 34.0,
                "Select a provider to configure…",
                CHROME_FG_DIM,
            );
        }
        let mut y_pos = y + 48.0;
        for (name, configured) in &state.configured_providers {
            let status = if *configured {
                "✓ Configured"
            } else {
                "Not configured"
            };
            renderer.chrome_text(
                x + 20.0,
                y_pos,
                &format!("{}: {}", truncate(name, 20), status),
                if *configured { STATE_DONE } else { CHROME_FG },
            );
            y_pos += ch + 10.0;
        }
        if let Some(err) = &state.error {
            renderer.chrome_text(
                x + 20.0,
                y + h - 32.0,
                truncate(err, 60).as_str(),
                STATE_FAILED,
            );
        }
        renderer.chrome_text(x + 20.0, y + h - 20.0, "Press Esc to close.", CHROME_FG_DIM);
    }

    /// Phase 3A §55 minimal task UI: live scheduler snapshot with an
    /// action hint bar. Reads are fresh per frame; actions happen via
    /// `dispatch_task_command` (never here).
    fn draw_tasks(
        renderer: &mut Renderer,
        window_size: PhysicalSize<u32>,
        tasks: &[Task],
        status: &terminal_workspace::terminal_session::orchestration::SchedulerStatus,
        selection: usize,
    ) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (72.0_f32 * cw).min(860.0_f32);
        let h = (24.0_f32 * ch).min(680.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.7],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(
            x + 20.0,
            y + 16.0,
            &format!(
                "TASKS — {} queued · {} running · {} completed · {} failed · {}¢ spent",
                status.queued.len(),
                status.running.len(),
                status.completed_count,
                status.failed_count,
                status.actual_cost_cents
            ),
            CHROME_ACCENT,
        );
        let start = selection.saturating_sub(9);
        for (row, t) in tasks.iter().skip(start).take(18).enumerate() {
            let row_y = y + 36.0 + (row as f32 * ch);
            let selected = row + start == selection;
            if selected {
                renderer.chrome_rect(x + 6.0, row_y - 2.0, w - 12.0, ch, CHROME_ACCENT);
            }
            let fg = if selected {
                [0.0, 0.0, 0.0, 1.0]
            } else {
                task_state_color(&t.status)
            };
            let exec = t
                .agent_execution_id
                .as_ref()
                .map(|e| format!("[{}]", truncate(&e.0, 8)))
                .unwrap_or_default();
            renderer.chrome_text(
                x + 12.0,
                row_y,
                &format!(
                    "{:<5} {:<14} {} · {} · {} attempt(s) {}",
                    t.status.label(),
                    truncate(&t.title, 14),
                    t.assigned_agent,
                    truncate(&t.id, 8),
                    t.attempt_count,
                    exec
                ),
                fg,
            );
            if let Some(err) = &t.error {
                renderer.chrome_text(
                    x + 12.0,
                    row_y + ch * 0.5,
                    &truncate(&err.message, 60),
                    STATE_FAILED,
                );
            }
        }
        renderer.chrome_text(
            x + 20.0,
            y + h - 20.0,
            "↑↓ select · Enter run all · c cancel · r retry · a approve · d reject · p open agent · Esc close",
            CHROME_FG_DIM,
        );
    }

    /// Draws the developer diagnostics panel (§32): engine metrics plus the
    /// agent dashboard. Never exposes secrets or credentials.
    fn draw_diagnostics(
        renderer: &mut Renderer,
        window_size: PhysicalSize<u32>,
        dashboard: &AgentDashboard,
        events_applied: u64,
        latency_p95_us: f64,
        subscribers: usize,
    ) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (64.0_f32 * cw).min(780.0_f32);
        let h = (22.0_f32 * ch).min(640.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.7],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(x + 20.0, y + 16.0, "Developer Diagnostics", CHROME_ACCENT);
        let mut line = y + 40.0;
        renderer.chrome_text(
            x + 20.0,
            line,
            &format!("Events applied: {events_applied}"),
            CHROME_FG,
        );
        line += ch;
        renderer.chrome_text(
            x + 20.0,
            line,
            &format!("Apply latency p95: {latency_p95_us:.0} µs"),
            CHROME_FG,
        );
        line += ch;
        renderer.chrome_text(
            x + 20.0,
            line,
            &format!("Event subscribers: {subscribers}"),
            CHROME_FG,
        );
        line += ch + 6.0;
        renderer.chrome_text(
            x + 20.0,
            line,
            &format!(
                "Agents: {} total · {} running · {} needs you · {} failed · {} completed",
                dashboard.total,
                dashboard.running,
                dashboard.needs_you,
                dashboard.failed,
                dashboard.completed
            ),
            CHROME_FG_DIM,
        );
        line += ch + 6.0;
        for row in dashboard.rows.iter().take(9) {
            let s = &row.snapshot;
            renderer.chrome_text(
                x + 20.0,
                line,
                &format!(
                    "{} · {} · {} · {} (conf {}) · {} evt · exit {:?}",
                    truncate(&s.display_name, 16),
                    truncate(&s.execution_id, 10),
                    s.state,
                    s.activity_kind,
                    s.activity_confidence,
                    s.events_emitted,
                    s.exit_code,
                ),
                CHROME_FG_DIM,
            );
            line += ch;
        }
        renderer.chrome_text(x + 20.0, y + h - 20.0, "Press Esc to close.", CHROME_FG_DIM);
    }

    /// Draws the work review/diff view overlay (§9): changed files with the
    /// selected file's bounded git diff.
    fn draw_work_view(renderer: &mut Renderer, window_size: PhysicalSize<u32>, view: &WorkView) {
        let (cw, ch) = renderer.cell_size();
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }
        let w = (64.0_f32 * cw).min(820.0_f32);
        let h = (26.0_f32 * ch).min(640.0_f32);
        let x = (window_size.width as f32 - w) / 2.0;
        let y = (window_size.height as f32 - h) / 2.0;
        renderer.chrome_rect(
            0.0,
            0.0,
            window_size.width as f32,
            window_size.height as f32,
            [0.0, 0.0, 0.0, 0.7],
        );
        renderer.chrome_rect(x, y, w, h, [0.12, 0.12, 0.15, 1.0]);
        renderer.chrome_border(x, y, w, h, 1.0, CHROME_ACCENT);
        renderer.chrome_text(x + 20.0, y + 16.0, "Review Changes", CHROME_ACCENT);
        let status = if view.success {
            "work loaded from agent runtime"
        } else {
            "no recorded work"
        };
        renderer.chrome_text(x + 20.0, y + 34.0, status, CHROME_FG_DIM);
        let mut line = y + 52.0;
        if view.changed_files.is_empty() {
            renderer.chrome_text(x + 20.0, line, "No changes to review.", CHROME_FG);
            line += ch;
            renderer.chrome_text(
                x + 20.0,
                line,
                "(no files_changed recorded for this work)",
                CHROME_FG_DIM,
            );
        } else {
            for (i, f) in view.changed_files.iter().take(6).enumerate() {
                let sel = i == view.selected_file;
                let label = if sel {
                    format!("▸ {f}")
                } else {
                    format!("  {f}")
                };
                renderer.chrome_text(
                    x + 20.0,
                    line,
                    &truncate(&label, 40),
                    if sel { CHROME_ACCENT } else { CHROME_FG_DIM },
                );
                line += ch;
            }
            line += ch;
            if view.diff_text.is_empty() {
                renderer.chrome_text(x + 20.0, line, "(no diff available)", CHROME_FG_DIM);
            } else {
                for dl in view.diff_text.lines().take(12) {
                    renderer.chrome_text(x + 20.0, line, &truncate(dl, 70), CHROME_FG);
                    line += ch;
                }
            }
        }
        renderer.chrome_text(x + 20.0, y + h - 20.0, "Press Esc to close.", CHROME_FG_DIM);
    }

    /// Permission decision (§18) — normalized by the runtime, translated
    /// by the agent's adapter.
    fn respond_permission(&mut self, pane_id: &str, allow: bool) {
        let result: anyhow::Result<()> = {
            let eng = self.engine.lock().expect("engine lock");
            let pid = pane_id.to_string();
            let Some(eid) = eng.execution_id_for_pane(&pid) else {
                tracing::debug!("permission response for {pane_id}: no execution");
                return;
            };
            let decision = if allow {
                PermissionDecision::AllowOnce
            } else {
                PermissionDecision::Deny
            };
            eng.agent_runtime().respond_permission(&eid, decision)
        };
        if let Err(e) = result {
            tracing::debug!("permission response for {pane_id} failed: {e}");
        }
    }

    fn on_key(&mut self, event: KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        // 1. Handle overlay keys first.
        if event.logical_key == NamedKey::Escape {
            // Always close overlays on Escape.
            self.close_overlays();
            return;
        }
        // 1b. Empty state: Enter opens provider setup for the chosen agent
        // (the provider overlay lists credential status per provider).
        if let Some(OverlayMode::EmptyState) = self.overlay_mode {
            if event.logical_key == NamedKey::Enter {
                self.open_provider_setup();
                return;
            }
            return;
        }
        // 1c. Task dashboard (Phase 3A §55): arrow navigation + one-key
        // actions; everything else is consumed by the overlay.
        if let Some(OverlayMode::Tasks) = self.overlay_mode {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    self.tasks_selection = self.tasks_selection.saturating_sub(1);
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let count = {
                        let eng = self.engine.lock().expect("engine lock");
                        eng.scheduler_status().states.len()
                    };
                    if count > 0 {
                        self.tasks_selection = (self.tasks_selection + 1).min(count - 1);
                    }
                }
                Key::Named(NamedKey::Enter) => {
                    let cmd = Command::RunTasks;
                    self.run_command(&cmd);
                }
                Key::Character(c) => match c.as_str() {
                    "c" => {
                        let cmd = Command::CancelSelectedTask;
                        self.run_command(&cmd);
                    }
                    "r" => {
                        let cmd = Command::RetrySelectedTask;
                        self.run_command(&cmd);
                    }
                    "a" => {
                        let cmd = Command::ApproveSelectedTask;
                        self.run_command(&cmd);
                    }
                    "d" => {
                        let cmd = Command::RejectSelectedTask;
                        self.run_command(&cmd);
                    }
                    "p" => {
                        let cmd = Command::OpenSelectedTaskAgent;
                        self.run_command(&cmd);
                    }
                    _ => {}
                },
                _ => {}
            }
            return;
        }
        // 2. Handle command palette interaction.
        if self.palette_open {
            match &event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    if self.palette_selection > 0 {
                        self.palette_selection -= 1;
                    }
                    return;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    let count = self
                        .registry
                        .palette()
                        .iter()
                        .filter(|cmd| {
                            cmd.to_label()
                                .to_lowercase()
                                .contains(&self.palette_query.to_lowercase())
                        })
                        .count();
                    if count > 0 {
                        self.palette_selection = (self.palette_selection + 1).min(count - 1);
                    }
                    return;
                }
                Key::Named(NamedKey::Enter) => {
                    let q = self.palette_query.to_lowercase();
                    let matches: Vec<Command> = self
                        .registry
                        .palette()
                        .into_iter()
                        .filter(|cmd| cmd.to_label().to_lowercase().contains(&q))
                        .collect();
                    if let Some(cmd) = matches.get(self.palette_selection) {
                        let cmd = cmd.clone();
                        self.run_command(&cmd);
                    }
                    self.close_overlays();
                    return;
                }
                _ => {
                    // Allow text input in palette.
                    if let Some(text) = &event.text {
                        self.palette_query.push_str(text);
                        self.palette_selection = 0;
                        self.refresh_palette_commands();
                        return;
                    }
                }
            }
        }
        // 3. App commands first (registry lookup).
        if let Some(chord) = self.chord_from(&event.logical_key) {
            if let Some(cmd) = self.registry.lookup(&chord) {
                let cmd = cmd.clone();
                self.run_command(&cmd);
                return;
            }
        }
        // 4. Otherwise forward the key to the focused pane's session (§9).
        let ctrl = self.modifiers.state().control_key();
        let alt = self.modifiers.state().alt_key();
        let app_keys = {
            let eng = self.engine.lock().expect("engine lock");
            eng.focused_pane()
                .and_then(|id| eng.state_for_pane(&id))
                .map(|st| st.modes.application_cursor_keys)
                .unwrap_or(false)
        };
        let bytes = event
            .text
            .as_deref()
            .filter(|t| !t.is_empty() && !ctrl && !alt)
            .map(|t| t.as_bytes().to_vec())
            .or_else(|| key_sequence(&event.logical_key, app_keys, ctrl, alt));
        if let Some(bytes) = bytes {
            let eng = self.engine.lock().expect("engine lock");
            eng.write_focused(&bytes);
        }
    }
}

/// Maps a logical key to the byte sequence a terminal application expects
/// (same mapping as Phase 0.5).
fn key_sequence(key: &Key, app_keys: bool, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let seq: Vec<u8> = match key {
        Key::Character(c) => {
            let ch = c.chars().next()?;
            if ctrl && ch.is_ascii() {
                vec![(ch.to_ascii_uppercase() as u8) & 0x1f]
            } else if alt && ch.is_ascii() {
                let mut v = vec![0x1b];
                v.extend_from_slice(ch.to_string().as_bytes());
                v
            } else if ch == ' ' {
                vec![b' ']
            } else {
                ch.to_string().into_bytes()
            }
        }
        Key::Named(named) => match named {
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Backspace => b"\x7f".to_vec(),
            NamedKey::Tab => b"\t".to_vec(),
            NamedKey::Escape => b"\x1b".to_vec(),
            NamedKey::ArrowUp => (if app_keys { b"\x1bOA" } else { b"\x1b[A" }).to_vec(),
            NamedKey::ArrowDown => (if app_keys { b"\x1bOB" } else { b"\x1b[B" }).to_vec(),
            NamedKey::ArrowRight => (if app_keys { b"\x1bOC" } else { b"\x1b[C" }).to_vec(),
            NamedKey::ArrowLeft => (if app_keys { b"\x1bOD" } else { b"\x1b[D" }).to_vec(),
            NamedKey::Home => (if app_keys { b"\x1bOH" } else { b"\x1b[H" }).to_vec(),
            NamedKey::End => (if app_keys { b"\x1bOF" } else { b"\x1b[F" }).to_vec(),
            NamedKey::PageUp => b"\x1b[5~".to_vec(),
            NamedKey::PageDown => b"\x1b[6~".to_vec(),
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            NamedKey::Insert => b"\x1b[2~".to_vec(),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => b"\x1b[15~".to_vec(),
            NamedKey::F6 => b"\x1b[17~".to_vec(),
            NamedKey::F7 => b"\x1b[18~".to_vec(),
            NamedKey::F8 => b"\x1b[19~".to_vec(),
            NamedKey::F9 => b"\x1b[20~".to_vec(),
            NamedKey::F10 => b"\x1b[21~".to_vec(),
            NamedKey::F11 => b"\x1b[23~".to_vec(),
            NamedKey::F12 => b"\x1b[24~".to_vec(),
            NamedKey::Space => b" ".to_vec(),
            _ => return None,
        },
        _ => return None,
    };
    Some(seq)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting FlashTerminal (Phase 1 multiplexer)...");

    let engine = Arc::new(Mutex::new(Multiplexer::new()?));
    let event_loop: EventLoop<AppEvent> = EventLoopBuilder::with_user_event().build()?;

    // Wake the loop whenever any session enqueues data (event-driven redraw).
    // `EventLoopProxy` is `Send` but not `Sync` on macOS, so it is wrapped in
    // a `Mutex` to satisfy the engine's `Send + Sync` wake callback.
    let proxy = std::sync::Mutex::new(event_loop.create_proxy());
    {
        let wake: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            // Lock poison is unrecoverable here; ignore rather than panic in
            // the reader-thread wake path.
            let _ = proxy.lock().map(|p| p.send_event(AppEvent::SessionData));
        });
        // Rebuild the engine with the wake callback (cheap: no sessions yet).
        *engine.lock().expect("engine lock") = Multiplexer::with_wake(Some(wake))?;
    }

    App::init_engine(&engine);

    // IPC control surface for the CLI (and future automation).
    let socket = ipc::default_socket_path();
    match ipc::serve(Arc::clone(&engine), &socket) {
        Ok(()) => tracing::info!("IPC control socket on {}", socket.display()),
        Err(e) => tracing::warn!("IPC serve failed: {e}"),
    }

    let mut app = App::new(engine);
    app.wake_proxy = Some(event_loop.create_proxy());

    // First-run experience (§26): show the empty state when no agent
    // sessions exist yet. Esc dismisses it for the session.
    {
        let has_agents = {
            let eng = app.engine.lock().expect("engine lock");
            eng.agent_runtime().session_count() > 0
        };
        if !has_agents {
            app.open_empty_state();
        }
    }

    event_loop.run(move |event, event_loop| {
        event_loop.set_control_flow(ControlFlow::Wait);
        match event {
            Event::Resumed => {
                if app.window.is_some() {
                    return;
                }
                let window = Arc::new(
                    WindowBuilder::new()
                        .with_title("FlashTerminal")
                        .with_inner_size(winit::dpi::LogicalSize::new(1200, 760))
                        .build(event_loop)
                        .expect("failed to create window"),
                );
                let (fonts, cache) = App::setup_fonts();
                let mut renderer =
                    pollster::block_on(Renderer::new(Arc::clone(&window), fonts, cache));
                renderer.set_cursor_style(app.cursor_style);
                app.window = Some(Arc::clone(&window));
                app.renderer = Some(renderer);
                app.apply_resize(window.inner_size());
                window.request_redraw();
            }
            Event::UserEvent(AppEvent::SessionData) => {
                if let Some(window) = &app.window {
                    window.request_redraw();
                }
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    app.persist();
                    event_loop.exit();
                }
                WindowEvent::Resized(physical) => {
                    app.apply_resize(physical);
                }
                WindowEvent::RedrawRequested => {
                    app.drain_and_render(std::time::Instant::now());
                }
                WindowEvent::KeyboardInput { event, .. } => {
                    app.on_key(event);
                }
                WindowEvent::ModifiersChanged(modifiers) => {
                    app.modifiers = modifiers;
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    if button == MouseButton::Left {
                        if state == ElementState::Pressed {
                            let pos = app.last_mouse_pos.unwrap_or_default();
                            // Chrome first: agent controls, permission bar,
                            // sidebar agent rows (§17–§19, §23).
                            if app.chrome_click(pos) {
                                if let Some(window) = &app.window {
                                    window.request_redraw();
                                }
                                return;
                            }
                            if let Some(pane) = app.pane_at(pos) {
                                {
                                    let mut eng = app.engine.lock().expect("engine lock");
                                    let _ = eng.focus_pane(&pane);
                                    if let Some(st) = eng.state_for_pane_mut(&pane) {
                                        st.clear_selection();
                                    }
                                }
                                if let Some((row, col)) = app.last_mouse_cell() {
                                    app.selection_anchor = Some((pane.clone(), row, col));
                                    {
                                        let mut eng = app.engine.lock().expect("engine lock");
                                        if let Some(st) = eng.state_for_pane_mut(&pane) {
                                            st.set_selection((row, col), (row, col));
                                        }
                                    }
                                }
                                app.persist();
                            }
                        } else {
                            app.finish_selection();
                        }
                    }
                    if let Some(window) = &app.window {
                        window.request_redraw();
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    app.last_mouse_pos = Some(position);
                    if let Some((pane, anchor_row, anchor_col)) = app.selection_anchor.clone() {
                        if let Some((row, col)) = app.last_mouse_cell() {
                            let mut eng = app.engine.lock().expect("engine lock");
                            if let Some(st) = eng.state_for_pane_mut(&pane) {
                                st.set_selection((anchor_row, anchor_col), (row, col));
                            }
                            if let Some(window) = &app.window {
                                window.request_redraw();
                            }
                        }
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y as i32,
                        MouseScrollDelta::PixelDelta(p) => (p.y / 20.0) as i32,
                    };
                    if lines != 0 {
                        let pane = app
                            .pane_at(app.last_mouse_pos.unwrap_or_default())
                            .or_else(|| app.engine.lock().ok().and_then(|e| e.focused_pane()));
                        if let Some(pane) = pane {
                            let mut eng = app.engine.lock().expect("engine lock");
                            if let Some(st) = eng.state_for_pane_mut(&pane) {
                                st.scroll_view(-lines);
                            }
                        }
                        if let Some(window) = &app.window {
                            window.request_redraw();
                        }
                    }
                }
                _ => {}
            },
            Event::AboutToWait => {
                let now = std::time::Instant::now();
                let needs_blink = app.renderer.is_some()
                    && now.duration_since(app.last_render).as_millis() as u64 >= BLINK_INTERVAL_MS;
                let pending = {
                    app.engine
                        .lock()
                        .ok()
                        .map(|e| e.terminal_session_count() > 0)
                        .unwrap_or(false)
                };
                if needs_blink || pending {
                    if let Some(window) = &app.window {
                        window.request_redraw();
                    }
                }
                if pending {
                    // Fallback timer so PTY output between events cannot
                    // stall the display (≤100 ms staleness).
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        now + std::time::Duration::from_millis(100),
                    ));
                }
            }
            _ => {}
        }
    })?;
    Ok(())
}
