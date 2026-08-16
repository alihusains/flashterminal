//! Agent Runtime (Phase 2B): real bidirectional execution.
//!
//! ```text
//! AgentSession
//!    ↓
//! AgentAdapter (policy only: executable, args, env names, heuristics)
//!    ↓
//! Session (the ONE PTY implementation, shared with terminal sessions)
//!    ↓
//! reader thread → terminal events → pane TerminalState   (same path as a shell)
//!             └→ tap → pump thread → redacted Output events + activity state
//! ```
//!
//! The runtime keeps one [`Session`] per agent so every pane-level
//! operation (input, resize, drain, fairness caps, exit detection, wakes)
//! is exactly the terminal path — no second PTY implementation exists.
//! A lightweight pump thread per agent consumes a raw-output tap, redacts
//! it, refines the activity state via adapter heuristics, and pushes
//! semantic [`AgentEvent`]s to the engine.

use crate::adapters::generic::GenericCliAdapter;
use crate::adapters::{build_spec, resolve_binary, AgentAdapterImpl};
use crate::credential::{CredentialRef, CredentialStore};
use crate::execution::{
    AgentActivity, AgentEvent, AgentState, ExecutionId, ExecutionMetadata, StateProvenance,
};
use crate::launch::AgentLaunchConfig;
use crate::provider::ProviderRegistry;
use crate::redact::Redactor;
use crate::work::{
    self, attention_for, collect_git_files, now_ms, observe_command, observe_file, ActivityKind,
    ActivitySource, AgentActivityState, AgentHealthRow, AgentWork, AttentionReason, ErrorKind,
    HealthStatus, TimelineKind, WorkError, WorkStatus,
};
use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use pty::PtyManager;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The normalized user decision for an agent permission prompt (§18).
/// The runtime translates it into the agent's expected keystrokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Deny,
    AllowOnce,
    Allow,
}

/// Verified capabilities of an agent adapter (§7). The UI only shows
/// actions whose capability is true — nothing here is faked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentCapabilities {
    pub spawn: bool,
    pub interactive: bool,
    pub stop: bool,
    pub restart: bool,
    pub resize: bool,
    pub resume: bool,
    pub pause: bool,
    /// Heuristic *detection* of approval prompts on the terminal stream.
    pub approval_detection: bool,
    pub structured_events: bool,
    pub usage: bool,
    pub cost: bool,
    // --- Phase 2C observability (§8, §10) ---
    /// Files-changed tracking is reliably observable for this agent.
    pub files_tracked: bool,
    /// Command execution is reliably observable for this agent.
    pub commands_tracked: bool,
}

/// A provider-neutral definition of an agent. Contains no API keys and no
/// model state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub id: String,
    pub name: String,
    pub display_name: String,
    /// Binary name or absolute path to launch.
    pub command: String,
    /// Default arguments (adapter policy can extend these).
    #[serde(default)]
    pub args: Vec<String>,
    /// e.g. "cli" (Phase 2B supports raw terminal compatibility as the
    /// baseline; structured protocols are adapter-isolated and unverified).
    #[serde(default)]
    pub protocol: String,
    #[serde(default)]
    pub documentation_url: Option<String>,
    /// Human install hint used in "executable not found" errors.
    #[serde(default)]
    pub install_hint: Option<String>,
}

impl AgentDefinition {
    pub fn new_cli(id: &str, name: &str, command: &str) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            display_name: name.to_string(),
            command: command.to_string(),
            args: vec![],
            protocol: "cli".to_string(),
            documentation_url: None,
            install_hint: None,
        }
    }
}

/// One agent session record. Shared between the pump thread (writer) and
/// the engine (reader) via a mutex inside the runtime.
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub metadata: ExecutionMetadata,
    pub definition_id: String,
    pub state: AgentState,
    /// Where the current `state` was observed from (Phase 2B.1 §14).
    pub provenance: StateProvenance,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub exit_code: Option<i32>,
    /// The launch this session was spawned with (persistable, no secrets).
    pub launch: AgentLaunchConfig,
    pub metrics: Arc<AgentMetrics>,
}

impl AgentSession {
    pub fn new(definition_id: String, launch: AgentLaunchConfig) -> Self {
        let mut metadata =
            ExecutionMetadata::new(crate::execution::ExecutionKind::Agent, launch.cwd.clone());
        metadata.agent_definition_id = Some(definition_id.clone());
        Self {
            metadata,
            definition_id,
            state: AgentState::Created,
            provenance: StateProvenance::PROCESS,
            started_at: None,
            completed_at: None,
            exit_code: None,
            launch,
            metrics: Arc::new(AgentMetrics::default()),
        }
    }

    pub fn with_id(
        execution_id: ExecutionId,
        definition_id: String,
        launch: AgentLaunchConfig,
    ) -> Self {
        let mut s = Self::new(definition_id, launch);
        s.metadata.id = execution_id;
        s
    }

    pub fn transition_state(&mut self, new_state: AgentState) -> AgentEvent {
        self.transition_state_with(new_state, StateProvenance::PROCESS)
    }

    pub fn transition_state_with(
        &mut self,
        new_state: AgentState,
        provenance: StateProvenance,
    ) -> AgentEvent {
        self.state = new_state;
        self.provenance = provenance;
        self.metadata.state = match new_state {
            AgentState::Completed => crate::execution::ExecutionState::Stopped,
            AgentState::Failed | AgentState::Crashed => crate::execution::ExecutionState::Failed,
            _ => crate::execution::ExecutionState::Running,
        };
        self.metrics.state_changes.fetch_add(1, Ordering::Relaxed);
        AgentEvent::StateChanged {
            new_state: self.state,
            provenance: Some(provenance),
        }
    }

    /// The activity model shown in the UI (§26).
    pub fn activity(&self) -> AgentActivity {
        self.state.into()
    }

    /// Duration since start (completed or still running).
    pub fn duration(&self) -> chrono::Duration {
        let start = self.started_at.unwrap_or_else(chrono::Utc::now);
        let end = self.completed_at.unwrap_or_else(chrono::Utc::now);
        end.signed_duration_since(start)
            .max(chrono::Duration::zero())
    }
}

/// Instrumentation counters for agent sessions (Phase 2B.1 §5).
#[derive(Debug, Default)]
pub struct AgentMetrics {
    /// Semantic `AgentEvent`s generated by the pump.
    pub events_emitted: AtomicU64,
    /// Raw output bytes read from the PTY.
    pub bytes_output: AtomicU64,
    /// State transitions applied.
    pub state_changes: AtomicU64,
    /// Output events queued into the runtime event channel (bounded).
    pub events_queued: AtomicU64,
    /// Output events drained by the engine.
    pub events_consumed: AtomicU64,
}

/// Immutable snapshot of an agent session for the UI/IPC. All fields are
/// secret-free.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSnapshot {
    pub execution_id: String,
    pub definition_id: String,
    pub display_name: String,
    pub state: String,
    pub activity: String,
    pub cwd: String,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub credential_ref: Option<String>,
    pub exit_code: Option<i32>,
    pub started_at_ms: Option<i64>,
    pub duration_secs: Option<i64>,
    pub capabilities: AgentCapabilities,
    pub launch: AgentLaunchConfig,
    /// State provenance: how `state` was observed (Phase 2B.1 §14).
    pub state_source: String,
    pub state_confidence: String,
    pub events_emitted: u64,
    pub state_changes: u64,
    // --- Phase 2C work/activity/attention/cost (§3–§20) ---
    /// The `AgentWork` id for this session.
    #[serde(default)]
    pub work_id: String,
    #[serde(default)]
    pub work_status: String,
    /// Attention reason when the agent needs the user (§12).
    #[serde(default)]
    pub attention: Option<AttentionReason>,
    #[serde(default)]
    pub activity_kind: String,
    #[serde(default)]
    pub activity_source: String,
    #[serde(default)]
    pub activity_confidence: u8,
    #[serde(default)]
    pub activity_detail: String,
    #[serde(default)]
    pub files_changed: u32,
    #[serde(default)]
    pub commands_run: u32,
    #[serde(default)]
    pub tests_passed: Option<u32>,
    #[serde(default)]
    pub usage_input_tokens: Option<u64>,
    #[serde(default)]
    pub usage_output_tokens: Option<u64>,
    #[serde(default)]
    pub usage_cached_tokens: Option<u64>,
    /// Estimated cost in cents; `None` when pricing is unknown (§18–§19).
    #[serde(default)]
    pub estimated_cost_cents: Option<u64>,
    #[serde(default)]
    pub timeline_len: usize,
    /// Latest activity wall-clock ms (UI "last activity").
    #[serde(default)]
    pub last_activity_at_ms: Option<u64>,
    /// Error classification when failed (§11).
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
}

/// The Agent Registry: definitions + adapter resolution.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    definitions: HashMap<String, AgentDefinition>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        let mut r = Self::default();
        for def in builtin_definitions() {
            r.register(def);
        }
        r
    }

    pub fn register(&mut self, definition: AgentDefinition) {
        self.definitions.insert(definition.id.clone(), definition);
    }

    pub fn get(&self, id: &str) -> Option<&AgentDefinition> {
        self.definitions.get(id)
    }

    pub fn list(&self) -> Vec<&AgentDefinition> {
        let mut v: Vec<_> = self.definitions.values().collect();
        v.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        v
    }

    /// Resolves the adapter for a definition id. Unknown definitions fall
    /// back to the generic CLI adapter with the stored definition.
    pub fn find_adapter(&self, id: &str) -> Option<Arc<dyn AgentAdapterImpl>> {
        match id {
            "claude-code" => Some(Arc::new(crate::adapters::claude::ClaudeCodeAdapter::new())),
            "codex" => Some(Arc::new(crate::adapters::codex::CodexAdapter::new())),
            "opencode" => Some(Arc::new(crate::adapters::opencode::OpenCodeAdapter::new())),
            "pi" => Some(Arc::new(crate::adapters::pi::PiAdapter::new())),
            "fake-agent" => Some(Arc::new(crate::adapters::fake::FakeAgentAdapter::new())),
            // Demo aliases (desktop `--demo`): deterministic fake behavior
            // (working/approval/completion/failure fixtures) under
            // provider-looking names. Never used outside demo mode — no
            // builtin definition uses the `demo-` prefix.
            id if id.starts_with("demo-") => {
                Some(Arc::new(crate::adapters::fake::FakeAgentAdapter::new()))
            }
            _ => self.get(id).map(|def| {
                Arc::new(GenericCliAdapter::for_definition(def.clone()))
                    as Arc<dyn AgentAdapterImpl>
            }),
        }
    }
}

fn builtin_definitions() -> Vec<AgentDefinition> {
    // Definitions mirror the adapters; the registry stays the single source
    // the UI lists.
    let mut defs = Vec::new();
    for adapter in [
        Arc::new(crate::adapters::claude::ClaudeCodeAdapter::new()) as Arc<dyn AgentAdapterImpl>,
        Arc::new(crate::adapters::codex::CodexAdapter::new()) as Arc<dyn AgentAdapterImpl>,
        Arc::new(crate::adapters::opencode::OpenCodeAdapter::new()) as Arc<dyn AgentAdapterImpl>,
        Arc::new(crate::adapters::pi::PiAdapter::new()) as Arc<dyn AgentAdapterImpl>,
        Arc::new(crate::adapters::fake::FakeAgentAdapter::new()) as Arc<dyn AgentAdapterImpl>,
    ] {
        defs.push(adapter.definition().clone());
    }
    defs
}

const EVENT_CAPACITY: usize = 1024;
const TAP_CAPACITY: usize = 256;

/// The raw-output tap callback type: `Fn(&[u8]) + Send`, invoked from the
/// PTY reader thread (must stay fast).
type TapCallback = Box<dyn Fn(&[u8]) + Send>;

/// A running agent: PTY session + control flag.
struct RunningAgent {
    session: Arc<crate::Session>,
    stop: Arc<AtomicBool>,
}

/// The Agent Runtime: spawns, controls, and observes agent sessions.
pub struct AgentRuntime {
    registry: Arc<AgentRegistry>,
    providers: ProviderRegistry,
    pty: Arc<PtyManager>,
    store: CredentialStore,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    sessions: HashMap<ExecutionId, Arc<Mutex<AgentSession>>>,
    adapters: HashMap<ExecutionId, Arc<dyn AgentAdapterImpl>>,
    ptys: HashMap<ExecutionId, Arc<RunningAgent>>,
    event_tx: Sender<(ExecutionId, AgentEvent)>,
    event_rx: Receiver<(ExecutionId, AgentEvent)>,
    /// Lock-free counters per session, decoupled from `sessions` so the
    /// drain path never takes a lock the pump may hold while blocked on
    /// the (bounded) event channel (2B.1 §2–3 deadlock fix).
    metrics_by_eid: HashMap<ExecutionId, Arc<AgentMetrics>>,
    /// Phase 2C: per-session work records (§3) — what the agent is trying
    /// to accomplish, separate from the process (`AgentSession`).
    work_by_eid: HashMap<ExecutionId, Arc<Mutex<AgentWork>>>,
    /// Phase 2C: provider/model pricing table (§19).
    pricing: crate::work::PricingRegistry,
}

impl AgentRuntime {
    pub fn new(
        registry: Arc<AgentRegistry>,
        providers: ProviderRegistry,
        pty: Arc<PtyManager>,
        store: CredentialStore,
        wake: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let (event_tx, event_rx) = bounded(EVENT_CAPACITY);
        Self {
            registry,
            providers,
            pty,
            store,
            wake,
            sessions: HashMap::new(),
            adapters: HashMap::new(),
            ptys: HashMap::new(),
            event_tx,
            event_rx,
            metrics_by_eid: HashMap::new(),
            work_by_eid: HashMap::new(),
            pricing: crate::work::PricingRegistry::new(),
        }
    }

    // ------------------------------------------------------------------
    // Spawn / control
    // ------------------------------------------------------------------

    /// Spawns an agent from a launch config in a real PTY.
    /// Returns `(ExecutionId, Session)` — the engine registers the Session
    /// with its TerminalState so rendering/input work exactly as terminal
    /// panes. Network access is NEVER used on this path.
    pub fn spawn(
        &mut self,
        launch: AgentLaunchConfig,
        cols: u16,
        rows: u16,
    ) -> Result<(ExecutionId, Arc<crate::Session>)> {
        self.spawn_impl(launch, cols, rows, None)
    }

    fn spawn_impl(
        &mut self,
        launch: AgentLaunchConfig,
        cols: u16,
        rows: u16,
        reuse_id: Option<ExecutionId>,
    ) -> Result<(ExecutionId, Arc<crate::Session>)> {
        launch.validate()?;
        let def = self
            .registry
            .get(&launch.definition_id)
            .with_context(|| format!("unknown agent definition `{}`", launch.definition_id))?;
        let adapter = self
            .registry
            .find_adapter(&launch.definition_id)
            .with_context(|| format!("no adapter for definition `{}`", launch.definition_id))?;

        // Resolve the executable (fake agent has its own resolution).
        let exe: PathBuf = if launch.definition_id == "fake-agent" {
            crate::adapters::fake::FakeAgentAdapter::resolve_binary()?
        } else {
            resolve_binary(adapter.as_ref(), def)?
        };

        let mut spec = build_spec(adapter.as_ref(), def, &launch);

        // ------------------------------------------------------------------
        // Credential injection (never persisted, never logged, never IPC'd).
        // ------------------------------------------------------------------
        let provider_id: Option<String> = launch.provider_id.clone().or_else(|| {
            launch
                .credential_ref
                .as_deref()
                .and_then(CredentialRef::parse)
                .and_then(|r| r.provider_id().map(|s| s.to_string()))
        });
        if let Some(pid) = &provider_id {
            let env_var = adapter
                .credential_env_var(pid)
                .map(|s| s.to_string())
                .or_else(|| {
                    self.providers
                        .credential_env_var(pid)
                        .map(|s| s.to_string())
                });
            if let Some(var) = env_var {
                match self.store.get_api_key(pid) {
                    Ok(Some(key)) => {
                        Redactor::register_secret(&key);
                        spec.env.push((var, key));
                    }
                    Ok(None) => {
                        let local = self
                            .providers
                            .get_provider(pid)
                            .map(|p| {
                                p.base_url
                                    .as_deref()
                                    .map(|u| {
                                        u.starts_with("http://localhost")
                                            || u.starts_with("http://127.0.0.1")
                                    })
                                    .unwrap_or(false)
                                    || p.is_custom
                            })
                            .unwrap_or(false);
                        if !local {
                            tracing::warn!(
                                "no credential configured for provider {pid} — \
                                 the agent may fail authentication"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("could not read credential for {pid}: {e}");
                    }
                }
            }
        }

        // ------------------------------------------------------------------
        // Spawn through the shared PTY session.
        // ------------------------------------------------------------------
        let (_event_tx, _dst_eid) = (self.event_tx.clone(), reuse_id.clone());
        let (tap_tx, tap_rx) = bounded::<Vec<u8>>(TAP_CAPACITY);
        let wake = self.wake.clone().map(|w| {
            let w2 = w.clone();
            Box::new(move || w2()) as Box<dyn Fn() + Send>
        });
        // Raw-chunk tap: forwards bytes to the pump for redaction +
        // activity detection. The reader thread stays fast (a bounded
        // blocking send only when the pump is backed up).
        let tap: Option<TapCallback> = Some(Box::new(move |chunk: &[u8]| {
            let _ = tap_tx.send(chunk.to_vec());
        }));

        let command = exe.to_string_lossy().to_string();
        let (session, _pid) = crate::Session::spawn_with_options(
            Arc::clone(&self.pty),
            &command,
            &spec.args,
            &spec.cwd,
            &spec.env,
            cols,
            rows,
            wake,
            tap,
        )
        .with_context(|| {
            let mut msg = format!(
                "failed to spawn {}: {}",
                def.display_name,
                Redactor::redact(&command)
            );
            // Do not leak resolved credentials into spawn errors.
            if let Some(pid) = &provider_id {
                msg.push_str(&format!(" (provider {pid})"));
            }
            Redactor::redact(&msg)
        })?;

        // ------------------------------------------------------------------
        // Register + start the pump thread.
        // ------------------------------------------------------------------
        let eid = reuse_id.unwrap_or_else(|| ExecutionId(session.id().to_string()));
        // The session keeps a redacted copy of the launch config: snapshots
        // and IPC (`AgentList`) must never expose secret material, while
        // provider/credential *references* stay intact so restart re-resolves
        // credentials from the store (Phase 2B.1 §28–29).
        let mut stored_launch = launch.clone();
        stored_launch.redact();
        let session = Arc::new(session);
        let agent_session = Arc::new(Mutex::new(AgentSession::with_id(
            eid.clone(),
            def.id.clone(),
            stored_launch,
        )));
        {
            let mut s = agent_session.lock().unwrap();
            s.started_at = Some(chrono::Utc::now());
            s.state = AgentState::Starting;
            s.metadata.id = eid.clone();
        }
        let session_metrics = agent_session.lock().unwrap().metrics.clone();
        let stop = Arc::new(AtomicBool::new(false));

        let running = RunningAgent {
            session: Arc::clone(&session),
            stop: Arc::clone(&stop),
        };

        // Phase 2C §3: one AgentWork per session (process ≠ work). Created
        // before the pump so the pump can record observations into it.
        let work = Arc::new(Mutex::new(AgentWork::new(
            eid.0.clone(),
            def.display_name.clone(),
        )));
        {
            let mut w = work.lock().unwrap();
            w.description = def.name.clone();
            w.files_observable = adapter.capabilities().files_tracked;
            w.commands_observable = adapter.capabilities().commands_tracked;
            w.timeline.push(
                TimelineKind::Started,
                format!("{} session started", def.display_name),
            );
        }
        let pump_work = Arc::clone(&work);

        spawn_pump(PumpContext {
            pty: Arc::clone(&self.pty),
            session: Arc::clone(&session),
            tap_rx,
            event_tx: self.event_tx.clone(),
            eid: eid.clone(),
            agent_session: Arc::clone(&agent_session),
            work: pump_work,
            adapter: Arc::clone(&adapter),
            stop,
        });

        self.work_by_eid.insert(eid.clone(), work);
        self.sessions.insert(eid.clone(), agent_session);
        self.metrics_by_eid.insert(eid.clone(), session_metrics);
        self.adapters.insert(eid.clone(), adapter);
        self.ptys.insert(eid.clone(), Arc::new(running));
        Ok((eid, session))
    }

    /// Stops a running agent (SIGKILL to the PTY process group, same as
    /// terminal sessions). The session record stays (state `Stopped`) so
    /// status and restart work.
    pub fn stop(&mut self, eid: &ExecutionId) -> Result<()> {
        let Some(running) = self.ptys.get(eid) else {
            anyhow::bail!("agent {} not running", eid);
        };
        running.stop.store(true, Ordering::SeqCst);
        running.session.terminate();
        if let Some(s) = self.sessions.get(eid) {
            let mut s = s.lock().unwrap();
            if !matches!(
                s.state,
                AgentState::Completed | AgentState::Failed | AgentState::Crashed
            ) {
                let ev = s.transition_state(AgentState::Stopped);
                let _ = self.event_tx.try_send((eid.clone(), ev));
            }
        }
        Ok(())
    }

    /// Restarts a stopped/exit agent with its stored launch config.
    /// Returns the fresh `Session` (the engine swaps it into the pane).
    pub fn restart(
        &mut self,
        eid: &ExecutionId,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<crate::Session>> {
        let launch = {
            let s = self
                .sessions
                .get(eid)
                .with_context(|| format!("agent {} not found", eid))?;
            let s = s.lock().unwrap();
            if matches!(
                s.state,
                AgentState::Starting
                    | AgentState::Working
                    | AgentState::Waiting
                    | AgentState::NeedsApproval
            ) {
                anyhow::bail!("agent {} is still running — stop it first", eid);
            }
            s.launch.clone()
        };
        // Tear down the previous process + pump.
        self.remove_internal(eid);
        let (_new_eid, session) = self.spawn_impl(launch, cols, rows, Some(eid.clone()))?;
        Ok(session)
    }

    /// Resume where the agent left off (capability-gated; only Claude Code
    /// with its documented `--resume` flag in Phase 2B).
    pub fn resume(
        &mut self,
        eid: &ExecutionId,
        cols: u16,
        rows: u16,
    ) -> Result<Arc<crate::Session>> {
        let caps = self
            .capabilities(eid)
            .with_context(|| format!("agent {} not found", eid))?;
        anyhow::ensure!(caps.resume, "this agent does not support resume");
        let mut launch = {
            let s = self.sessions.get(eid).context("agent not found")?;
            let s = s.lock().unwrap();
            s.launch.clone()
        };
        // An empty resume id makes `claude --resume` open the session picker.
        if launch.resume_id.is_none() {
            launch.resume_id = Some(String::new());
        }
        self.remove_internal(eid);
        let session = self.spawn_impl(launch, cols, rows, Some(eid.clone()))?.1;
        Ok(session)
    }

    /// Pause is intentionally unsupported until a real mechanism exists —
    /// no fake capability surfaces.
    pub fn pause(&self, eid: &ExecutionId) -> Result<()> {
        let caps = self
            .capabilities(eid)
            .with_context(|| format!("agent {} not found", eid))?;
        anyhow::ensure!(caps.pause, "this agent does not support pause");
        anyhow::bail!("pause is not implemented for this agent")
    }

    fn remove_internal(&mut self, eid: &ExecutionId) {
        if let Some(r) = self.ptys.remove(eid) {
            r.stop.store(true, Ordering::SeqCst);
            r.session.terminate();
        }
        self.adapters.remove(eid);
        self.sessions.remove(eid);
        self.metrics_by_eid.remove(eid);
        self.work_by_eid.remove(eid);
    }

    /// Fully removes an agent (pane closed). Kills the process group and
    /// drops all state.
    pub fn remove(&mut self, eid: &ExecutionId) {
        self.remove_internal(eid);
    }

    // ------------------------------------------------------------------
    // Input / resize / observation
    // ------------------------------------------------------------------

    pub fn send_input(&self, eid: &ExecutionId, data: &[u8]) -> Result<()> {
        let r = self
            .ptys
            .get(eid)
            .with_context(|| format!("agent {} not running", eid))?;
        r.session.write(data);
        Ok(())
    }

    /// Responds to a `PermissionRequested` prompt (Phase 2B.1 §17–18).
    ///
    /// The user's decision is normalized here and translated by the agent's
    /// adapter into whatever keystrokes the agent's approval prompt expects.
    /// The UI never writes directly to the process — the runtime is the
    /// security boundary. `remember` is accepted for protocol compatibility;
    /// no adapter implements persistent permission memory yet, so currently
    /// it behaves like a one-time allow.
    pub fn respond_permission(
        &self,
        eid: &ExecutionId,
        decision: PermissionDecision,
    ) -> Result<()> {
        let r = self
            .ptys
            .get(eid)
            .with_context(|| format!("agent {} not running", eid))?;
        let adapter = self
            .adapters
            .get(eid)
            .with_context(|| format!("agent {} not found", eid))?;
        if !matches!(
            self.sessions.get(eid).map(|s| s.lock().unwrap().state),
            Some(AgentState::NeedsApproval)
        ) {
            tracing::debug!(
                "permission response for {} sent while not in NeedsApproval state",
                eid
            );
        }
        let bytes = adapter.permission_response(decision);
        r.session.write(&bytes);
        Ok(())
    }

    pub fn resize(&self, eid: &ExecutionId, cols: u16, rows: u16) -> Result<()> {
        let r = self
            .ptys
            .get(eid)
            .with_context(|| format!("agent {} not running", eid))?;
        r.session.resize(cols, rows);
        Ok(())
    }

    /// The live `Session` for an agent (shared with the engine map).
    pub fn session(&self, eid: &ExecutionId) -> Option<Arc<crate::Session>> {
        self.ptys.get(eid).map(|r| Arc::clone(&r.session))
    }

    pub fn has_exited(&self, eid: &ExecutionId) -> bool {
        self.ptys
            .get(eid)
            .map(|r| r.session.has_exited())
            .unwrap_or(true)
    }

    /// Verified capabilities for an execution id (UI gating).
    pub fn capabilities(&self, eid: &ExecutionId) -> Option<AgentCapabilities> {
        self.adapters.get(eid).map(|a| a.capabilities())
    }

    // ------------------------------------------------------------------
    // Events / snapshots
    // ------------------------------------------------------------------

    /// Drains all pending agent events (engine calls this per frame).
    /// Counts consumed events into each session's metrics (Phase 2B.1 §5).
    pub fn drain_events(&mut self) -> Vec<(ExecutionId, AgentEvent)> {
        let mut out = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            // Lock-free: the pump may be blocked on a full channel while
            // holding the session lock — taking it here would deadlock
            // (2B.1 deadlock fix; counters live in `metrics_by_eid`).
            if let Some(m) = self.metrics_by_eid.get(&ev.0) {
                m.events_consumed.fetch_add(1, Ordering::Relaxed);
            }
            out.push(ev);
        }
        out
    }

    pub fn has_pending_events(&self) -> bool {
        !self.event_rx.is_empty()
    }

    /// Raw lifecycle state for an execution (Phase 3A scheduler view — the
    /// scheduler decides from the enum, not display strings).
    pub fn raw_state(&self, eid: &ExecutionId) -> Option<AgentState> {
        self.sessions
            .get(eid)
            .and_then(|s| s.lock().ok())
            .map(|s| s.state)
    }

    /// Whether an agent definition is registered (task-create validation,
    /// 3a.md §8: unknown agent references are typed errors).
    pub fn definition_exists(&self, id: &str) -> bool {
        self.registry.get(id).is_some()
    }

    /// Adapter lookup for task launches (3a.md §20 — scheduler → adapter).
    pub fn find_adapter(&self, id: &str) -> Option<Arc<dyn crate::adapters::AgentAdapterImpl>> {
        self.registry.find_adapter(id)
    }

    /// The agent registry (3b.md §5 — planner context building and §14
    /// plan validation consult the engine's authoritative registry).
    pub fn registry(&self) -> &AgentRegistry {
        &self.registry
    }

    /// Mutable registry access (definition registration — e.g. demo mode
    /// registering provider-named definitions backed by fixtures). The
    /// runtime is the registry's single owner (the engine moves it in at
    /// construction and never clones it), so `Arc::get_mut` always hires.
    pub fn registry_mut(&mut self) -> &mut AgentRegistry {
        let inner = Arc::get_mut(&mut self.registry)
            .expect("registry Arc is uniquely owned by the agent runtime");
        inner
    }

    /// Provider ids the user may select for the planner (3b.md §35 — the
    /// planner reuses the existing ProviderRegistry, never a second one).
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers
            .list_providers()
            .iter()
            .map(|p| p.id.clone())
            .collect()
    }

    pub fn get_session(&self, eid: &ExecutionId) -> Option<AgentSnapshot> {
        // Locks are taken sequentially (work first, then session) — never
        // nested (2B.1 deadlock rule). The pump follows the same order.
        let work = self.work_by_eid.get(eid).map(|w| w.lock().unwrap().clone());
        let (mut snap, provider, model) = {
            let s = self.sessions.get(eid)?.lock().unwrap();
            let adapter = self.adapters.get(eid);
            let display_name = self
                .registry
                .get(&s.definition_id)
                .map(|d| d.display_name.clone())
                .unwrap_or_else(|| s.definition_id.clone());
            (
                snapshot(
                    &s,
                    eid,
                    adapter.map(|a| a.capabilities()).unwrap_or_default(),
                    display_name,
                    work.as_ref(),
                ),
                s.launch.provider_id.clone(),
                s.launch.model_id.clone(),
            )
        };
        if let (Some(p), Some(m)) = (provider, model) {
            if let Some(w) = &work {
                snap.estimated_cost_cents = self.pricing.estimate_cents(&p, &m, &w.usage);
            }
        }
        Some(snap)
    }

    pub fn list_sessions(&self) -> Vec<AgentSnapshot> {
        let registry = &self.registry;
        self.sessions
            .iter()
            .map(|(eid, s)| {
                let work = self.work_by_eid.get(eid).map(|w| w.lock().unwrap().clone());
                let (mut snap, provider, model) = {
                    let s = s.lock().unwrap();
                    let adapter = self.adapters.get(eid);
                    let display_name = registry
                        .get(&s.definition_id)
                        .map(|d| d.display_name.clone())
                        .unwrap_or_else(|| s.definition_id.clone());
                    (
                        snapshot(
                            &s,
                            eid,
                            adapter.map(|a| a.capabilities()).unwrap_or_default(),
                            display_name,
                            work.as_ref(),
                        ),
                        s.launch.provider_id.clone(),
                        s.launch.model_id.clone(),
                    )
                };
                if let (Some(p), Some(m)) = (provider, model) {
                    if let Some(w) = &work {
                        snap.estimated_cost_cents = self.pricing.estimate_cents(&p, &m, &w.usage);
                    }
                }
                snap
            })
            .collect()
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// All live agent execution ids.
    pub fn execution_ids(&self) -> Vec<ExecutionId> {
        self.sessions.keys().cloned().collect()
    }

    // ------------------------------------------------------------------
    // Phase 2C: work / attention / health / usage
    // ------------------------------------------------------------------

    /// The live work record for an execution (Phase 2C §3).
    pub fn work(&self, eid: &ExecutionId) -> Option<Arc<Mutex<AgentWork>>> {
        self.work_by_eid.get(eid).cloned()
    }

    /// Clone of the work record (UI/IPC snapshot).
    pub fn get_work(&self, eid: &ExecutionId) -> Option<AgentWork> {
        Some(self.work_by_eid.get(eid)?.lock().unwrap().clone())
    }

    pub fn list_works(&self) -> Vec<(ExecutionId, AgentWork)> {
        self.work_by_eid
            .iter()
            .map(|(eid, w)| (eid.clone(), w.lock().unwrap().clone()))
            .collect()
    }

    /// Computes estimated cost for a session when pricing is known (§18–§19).
    /// Returns `None` when the provider/model/usage is unknown.
    ///
    /// Lock discipline (2B.1 rule): the work and session locks are taken
    /// SEQUENTIALLY, never nested — the pump holds them one at a time too.
    pub fn estimated_cost_cents(&self, eid: &ExecutionId) -> Option<u64> {
        let usage = self.work_by_eid.get(eid)?.lock().unwrap().usage.clone();
        let (provider, model) = {
            let s = self.sessions.get(eid)?.lock().unwrap();
            (s.launch.provider_id.clone(), s.launch.model_id.clone())
        };
        let (Some(provider), Some(model)) = (provider, model) else {
            return None;
        };
        self.pricing.estimate_cents(&provider, &model, &usage)
    }

    /// Phase 2C §28: per-provider credential configuration state (never
    /// secrets — just presence). Used by the provider screen/UX.
    pub fn provider_status(&self) -> Vec<(String, bool)> {
        self.providers
            .list_providers()
            .into_iter()
            .map(|p| (p.id.clone(), self.credential_configured_for(&p.id)))
            .collect()
    }

    /// Whether a provider has a usable credential source (local endpoints
    /// always count — they need no key).
    fn credential_configured_for(&self, provider_id: &str) -> bool {
        let Some(p) = self.providers.get_provider(provider_id) else {
            return false;
        };
        p.is_custom
            || p.base_url
                .as_deref()
                .map(|u| u.starts_with("http://localhost") || u.starts_with("http://127.0.0.1"))
                .unwrap_or(false)
            || self
                .store
                .get_api_key(p.id.as_str())
                .ok()
                .flatten()
                .is_some()
    }

    /// Agent health rows (§30): binary presence + credential state, per
    /// definition. Never downloads software; never runs auth flows.
    pub fn health(&self) -> Vec<AgentHealthRow> {
        use crate::adapters::{find_executable, is_executable};
        let mut rows = Vec::new();
        for def in self.registry.list() {
            let Some(adapter) = self.registry.find_adapter(&def.id) else {
                continue;
            };
            let mut candidates: Vec<&str> = adapter.candidate_binaries().to_vec();
            if !def.command.is_empty() {
                candidates.insert(0, def.command.as_str());
            }
            let binary = find_executable(&candidates)
                .filter(|p| is_executable(p))
                .or_else(|| {
                    if let Ok(p) = resolve_binary(adapter.as_ref(), def) {
                        p.is_file().then_some(p)
                    } else {
                        None
                    }
                });
            let provider_id = def.name.to_ascii_lowercase().split(' ').next().map(|f| {
                if f.starts_with("claude") {
                    "anthropic".to_string()
                } else if f == "codex" || f == "opencode" {
                    "openai".to_string()
                } else if f == "pi" {
                    "anthropic".to_string()
                } else {
                    f.to_string()
                }
            });
            let credential_configured = provider_id
                .as_ref()
                .map(|p| self.credential_configured_for(p))
                .unwrap_or(false);
            let (status, detail) = match &binary {
                Some(path) if credential_configured => (
                    HealthStatus::Authenticated,
                    format!(
                        "installed at {}; credential configured",
                        path.to_string_lossy()
                    ),
                ),
                Some(path) => (
                    HealthStatus::Available,
                    format!(
                        "installed at {}; no saved credential (local CLI auth may still work)",
                        path.to_string_lossy()
                    ),
                ),
                None => (
                    HealthStatus::Unavailable,
                    def.install_hint
                        .clone()
                        .unwrap_or_else(|| "executable not found on PATH".into()),
                ),
            };
            rows.push(AgentHealthRow {
                definition_id: def.id.clone(),
                display_name: def.display_name.clone(),
                binary_path: binary.as_ref().map(|p| p.to_string_lossy().to_string()),
                installed: binary.is_some(),
                status,
                credential_configured,
                detail,
            });
        }
        rows
    }
}

impl Drop for AgentRuntime {
    fn drop(&mut self) {
        let ids: Vec<ExecutionId> = self.ptys.keys().cloned().collect();
        for id in ids {
            self.remove_internal(&id);
        }
    }
}

fn snapshot(
    s: &AgentSession,
    eid: &ExecutionId,
    caps: AgentCapabilities,
    display_name: String,
    work: Option<&AgentWork>,
) -> AgentSnapshot {
    let state = s.state;
    let attention = attention_for(state);
    let act = work.and_then(|w| w.current_activity().cloned());
    let error = work
        .and_then(|w| w.errors.last())
        .map(|e| (e.kind.label().to_string(), e.message.clone()));
    AgentSnapshot {
        execution_id: eid.0.clone(),
        definition_id: s.definition_id.clone(),
        display_name,
        state: format!("{:?}", s.state),
        activity: s.activity().to_string(),
        cwd: s.metadata.cwd.clone(),
        provider_id: s.launch.provider_id.clone(),
        model_id: s.launch.model_id.clone(),
        credential_ref: s.launch.credential_ref.clone(),
        exit_code: s.exit_code,
        started_at_ms: s.started_at.map(|t| t.timestamp_millis()),
        duration_secs: Some(s.duration().num_seconds()),
        capabilities: caps,
        launch: s.launch.clone(),
        state_source: format!("{:?}", s.provenance.source),
        state_confidence: format!("{:?}", s.provenance.confidence),
        events_emitted: s.metrics.events_emitted.load(Ordering::Relaxed),
        state_changes: s.metrics.state_changes.load(Ordering::Relaxed),
        work_id: work.map(|w| w.id.clone()).unwrap_or_default(),
        work_status: work
            .map(|w| w.status.label().to_string())
            .unwrap_or_default(),
        attention,
        activity_kind: act
            .as_ref()
            .map(|a| a.kind.label().to_string())
            .unwrap_or_default(),
        activity_source: act
            .as_ref()
            .map(|a| a.source.label().to_string())
            .unwrap_or_default(),
        activity_confidence: act.as_ref().map(|a| a.confidence).unwrap_or(0),
        activity_detail: act.as_ref().map(|a| a.detail.clone()).unwrap_or_default(),
        files_changed: work.map(|w| w.files_changed.len() as u32).unwrap_or(0),
        commands_run: work.map(|w| w.commands.len() as u32).unwrap_or(0),
        tests_passed: work.map(|w| w.summary().tests_passed).unwrap_or(None),
        usage_input_tokens: work.and_then(|w| w.usage.input_tokens),
        usage_output_tokens: work.and_then(|w| w.usage.output_tokens),
        usage_cached_tokens: work.and_then(|w| w.usage.cached_tokens),
        estimated_cost_cents: None, // computed by the runtime (needs pricing)
        timeline_len: work.map(|w| w.timeline.len()).unwrap_or(0),
        last_activity_at_ms: act.map(|a| a.at_ms),
        error_kind: error.as_ref().map(|(k, _)| k.clone()),
        error_message: error.as_ref().map(|(_, m)| m.clone()),
    }
}

// ---------------------------------------------------------------------------
// Pump thread
// ---------------------------------------------------------------------------

/// Everything the per-agent pump thread needs (kept in one context so the
/// thread body stays a single unit).
struct PumpContext {
    pty: Arc<PtyManager>,
    session: Arc<crate::Session>,
    tap_rx: Receiver<Vec<u8>>,
    event_tx: Sender<(ExecutionId, AgentEvent)>,
    eid: ExecutionId,
    agent_session: Arc<Mutex<AgentSession>>,
    work: Arc<Mutex<AgentWork>>,
    adapter: Arc<dyn AgentAdapterImpl>,
    stop: Arc<AtomicBool>,
}

fn spawn_pump(ctx: PumpContext) {
    let session_id = ctx.session.id().to_string();
    std::thread::Builder::new()
        .name(format!(
            "agent-pump-{}",
            &session_id[..session_id.len().min(8)]
        ))
        .spawn(move || {
            let mut tracker = PumpTracker {
                last_state: None,
                last_kind: None,
                last_activity_emit_ms: 0,
            };
            let event_tx = &ctx.event_tx;
            let eid = &ctx.eid;
            let agent_session = &ctx.agent_session;
            let starting_ev = {
                let mut s = agent_session.lock().unwrap();
                s.transition_state_with(AgentState::Starting, StateProvenance::PROCESS)
            };
            // Sends happen outside the session lock: the bounded channel
            // may be full and the pump blocks here — never while holding
            // a lock the drain path needs (2B.1 deadlock fix).
            let _ = event_tx.send((eid.clone(), starting_ev));
            let _ = event_tx.send((eid.clone(), AgentEvent::Started));
            loop {
                let chunk = match ctx.tap_rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(chunk) => Some(chunk),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => None,
                };
                if let Some(chunk) = chunk {
                    process_chunk(
                        &chunk,
                        ctx.adapter.as_ref(),
                        event_tx,
                        eid,
                        agent_session,
                        &ctx.work,
                        &mut tracker,
                    );
                }
                if ctx.session.has_exited() {
                    let code = poll_exit_code(&ctx.pty, &session_id);
                    if ctx.stop.load(Ordering::SeqCst) {
                        // User stop: state was already transitioned by
                        // `stop()`; just report the exit.
                        let _ = event_tx.send((eid.clone(), AgentEvent::Exited { code }));
                    } else {
                        finish(event_tx, eid, agent_session, &ctx.work, code);
                    }
                    break;
                }
                if ctx.stop.load(Ordering::SeqCst) {
                    // Stop requested before EOF (SIGKILL in flight): reap
                    // the status (bounded) and report the exit.
                    let code = poll_exit_code(&ctx.pty, &session_id);
                    let _ = event_tx.send((eid.clone(), AgentEvent::Exited { code }));
                    break;
                }
            }
        })
        .expect("spawn agent pump thread");
}

/// Phase 2C §4: line → activity-kind heuristic (fallback layer under any
/// future structured protocol). Conservative: unknown lines → `Unknown`.
fn detect_activity_kind(
    line: &str,
    hint: Option<crate::adapters::ActivityHint>,
) -> (ActivityKind, String) {
    if let Some(h) = hint {
        return match h {
            crate::adapters::ActivityHint::NeedsApproval => {
                (ActivityKind::WaitingForPermission, line.trim().to_string())
            }
            crate::adapters::ActivityHint::Waiting => {
                (ActivityKind::WaitingForInput, String::new())
            }
            crate::adapters::ActivityHint::Working => (ActivityKind::Unknown, String::new()),
        };
    }
    let l = line.trim();
    let lower = l.to_ascii_lowercase();
    if lower.contains("running tests")
        || lower.contains("test suite")
        || lower.contains("passing")
        || lower.contains("failed tests")
        || lower.contains("npm test")
        || lower.contains("cargo test")
    {
        return (ActivityKind::RunningTests, String::new());
    }
    if let Some(cmd) = observe_command(l) {
        return (ActivityKind::RunningCommand, cmd);
    }
    if lower.contains("reading") || lower.contains("scanning") || lower.contains("exploring") {
        return (ActivityKind::Reading, String::new());
    }
    if lower.contains("planning") || lower.contains("designing") {
        return (ActivityKind::Planning, String::new());
    }
    if lower.contains("thinking") {
        return (ActivityKind::Thinking, String::new());
    }
    if lower.contains("reviewing") {
        return (ActivityKind::Reviewing, String::new());
    }
    if lower.contains("finishing") || lower.contains("finalizing") {
        return (ActivityKind::Finishing, String::new());
    }
    if lower.contains("editing")
        || lower.contains("modifying")
        || lower.contains("wrote")
        || lower.contains("updated ")
        || lower.starts_with("→")
    {
        let detail = if let Some(rest) = l
            .strip_prefix("→")
            .or_else(|| l.strip_prefix("Wrote "))
            .or_else(|| l.strip_prefix("Updated "))
            .or_else(|| l.strip_prefix("Modified "))
        {
            rest.split_whitespace().next().unwrap_or("").to_string()
        } else {
            String::new()
        };
        return (ActivityKind::Editing, detail);
    }
    if lower.starts_with("starting") {
        return (ActivityKind::Starting, String::new());
    }
    (ActivityKind::Unknown, String::new())
}

/// Per-pump heuristic tracking: last state/kind/emission time, so chunk
/// processing stays stateless apart from this one mutable unit.
struct PumpTracker {
    last_state: Option<AgentState>,
    last_kind: Option<ActivityKind>,
    last_activity_emit_ms: u64,
}

fn process_chunk(
    chunk: &[u8],
    adapter: &dyn AgentAdapterImpl,
    event_tx: &Sender<(ExecutionId, AgentEvent)>,
    eid: &ExecutionId,
    agent_session: &Arc<Mutex<AgentSession>>,
    work: &Arc<Mutex<AgentWork>>,
    tracker: &mut PumpTracker,
) {
    {
        let s = agent_session.lock().unwrap();
        s.metrics
            .bytes_output
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
    }
    // Activity detection (heuristic refinement; output remains authoritative).
    let text = String::from_utf8_lossy(chunk);
    let cwd = agent_session.lock().unwrap().launch.cwd.clone();
    for line in text.lines() {
        let hint = adapter.detect_activity(line);
        // §4/§12: state + activity-kind refinement.
        if let Some(hint) = hint {
            let state = hint.to_state();
            if tracker.last_state != Some(state) {
                tracker.last_state = Some(state);
                let provenance = if state == AgentState::NeedsApproval {
                    StateProvenance::HEURISTIC_APPROVAL
                } else {
                    StateProvenance::HEURISTIC
                };
                let needs_approval = state == AgentState::NeedsApproval;
                let (kind, detail) = detect_activity_kind(line, Some(hint));
                {
                    let mut w = work.lock().unwrap();
                    w.push_activity(AgentActivityState {
                        kind,
                        source: ActivitySource::Heuristic,
                        confidence: if needs_approval { 25 } else { 60 },
                        detail: Redactor::redact(&detail),
                        at_ms: now_ms(),
                        count: 1,
                    });
                    if needs_approval {
                        w.timeline
                            .push(TimelineKind::Approval, Redactor::redact(line.trim()));
                    } else {
                        w.timeline.push(TimelineKind::Activity, kind.label());
                    }
                }
                let ev = {
                    let mut s = agent_session.lock().unwrap();
                    s.metrics.events_emitted.fetch_add(1, Ordering::Relaxed);
                    s.metrics.events_queued.fetch_add(1, Ordering::Relaxed);
                    s.transition_state_with(state, provenance)
                };
                // Outside the lock: the channel may be full (2B.1 fix).
                let _ = event_tx.send((eid.clone(), ev));
                if needs_approval {
                    let _ = event_tx.send((
                        eid.clone(),
                        AgentEvent::PermissionRequested {
                            action: "permission".to_string(),
                            context: Redactor::redact(line.trim()),
                        },
                    ));
                }
            }
        } else {
            let (kind, detail) = detect_activity_kind(line, None);
            if kind != ActivityKind::Unknown && tracker.last_kind != Some(kind) {
                tracker.last_kind = Some(kind);
                let now = now_ms();
                {
                    let mut w = work.lock().unwrap();
                    w.push_activity(AgentActivityState {
                        kind,
                        source: ActivitySource::Heuristic,
                        confidence: 60,
                        detail: Redactor::redact(&detail),
                        at_ms: now,
                        count: 1,
                    });
                    if kind.timeline_worthy() {
                        w.timeline.push(TimelineKind::Activity, kind.label());
                    }
                    if kind == ActivityKind::Editing {
                        if let Some(f) = observe_file(line, &cwd) {
                            let f = Redactor::redact(&f);
                            if w.files_changed.insert(f.clone()) {
                                w.timeline.push(TimelineKind::File, f);
                            }
                        }
                    }
                }
                // Throttled emission (§23): at most one activity event per
                // coalescing window. Sends happen outside locks.
                if now.saturating_sub(tracker.last_activity_emit_ms) >= work::ACTIVITY_COALESCE_MS {
                    tracker.last_activity_emit_ms = now;
                    let _ = event_tx.send((
                        eid.clone(),
                        AgentEvent::Activity {
                            kind,
                            source: ActivitySource::Heuristic,
                            confidence: 60,
                            detail: Redactor::redact(&detail),
                        },
                    ));
                }
            } else if let Some(cmd) = observe_command(line) {
                let cmd = Redactor::redact(&cmd);
                let mut w = work.lock().unwrap();
                if w.commands.len() < 512 && w.commands.last().map(|c| c != &cmd).unwrap_or(true) {
                    w.commands.push(cmd.clone());
                    w.timeline.push(TimelineKind::Command, cmd);
                }
            }
        }
    }
    // Redacted output event.
    let redacted = Redactor::redact(&text);
    {
        let s = agent_session.lock().unwrap();
        s.metrics.events_emitted.fetch_add(1, Ordering::Relaxed);
        s.metrics.events_queued.fetch_add(1, Ordering::Relaxed);
    }
    let _ = event_tx.send((eid.clone(), AgentEvent::Output { text: redacted }));
}

/// Waits (bounded) for the child to be reaped and returns its exit code.
fn poll_exit_code(pty: &PtyManager, session_id: &str) -> Option<i32> {
    for _ in 0..100 {
        match pty.try_wait(session_id) {
            Ok(Some(status)) => {
                // portable-pty 0.8: exit_code() is u32 and signal deaths
                // report 1; the fake `crash` scenario exits 139 directly.
                return Some(status.exit_code() as i32);
            }
            _ => {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    None
}

fn finish(
    event_tx: &Sender<(ExecutionId, AgentEvent)>,
    eid: &ExecutionId,
    agent_session: &Arc<Mutex<AgentSession>>,
    work: &Arc<Mutex<AgentWork>>,
    code: Option<i32>,
) {
    let final_state = match code {
        Some(0) => AgentState::Completed,
        Some(c) if (128..192).contains(&c) => AgentState::Crashed,
        Some(_) => AgentState::Failed,
        None => AgentState::Failed,
    };
    let (ev, cwd) = {
        let mut s = agent_session.lock().unwrap();
        // A user stop already moved the session to Stopped — do not
        // override it with a failure classification.
        let terminal = matches!(
            s.state,
            AgentState::Completed | AgentState::Failed | AgentState::Crashed | AgentState::Stopped
        );
        let ev = if terminal {
            None
        } else {
            // Exit-code classification is authoritative process lifecycle.
            Some(s.transition_state_with(final_state, StateProvenance::PROCESS))
        };
        s.exit_code = code;
        s.completed_at = Some(chrono::Utc::now());
        s.metrics.events_emitted.fetch_add(1, Ordering::Relaxed);
        s.metrics.events_queued.fetch_add(1, Ordering::Relaxed);
        (ev, s.launch.cwd.clone())
    };
    // Phase 2C §3/§7/§11: finalize the work record (deterministic summary;
    // git file snapshot once at completion, never continuously).
    {
        let mut w = work.lock().unwrap();
        match final_state {
            AgentState::Completed => {
                w.finish(WorkStatus::Completed);
                if w.files_observable {
                    for f in collect_git_files(&cwd) {
                        w.files_changed.insert(Redactor::redact(&f));
                    }
                }
                w.timeline.push(TimelineKind::Completed, "work complete");
            }
            AgentState::Failed | AgentState::Crashed => {
                w.finish(WorkStatus::Failed);
                w.push_error(WorkError::new(
                    ErrorKind::AgentFailure,
                    format!(
                        "agent exited with code {}",
                        code.map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    ),
                ));
                w.timeline.push(TimelineKind::Error, "agent failed");
            }
            _ => {}
        }
    }
    // Outside the lock: the channel may be full (2B.1 deadlock fix).
    if let Some(ev) = ev {
        let _ = event_tx.send((eid.clone(), ev));
    }
    if final_state == AgentState::Completed {
        let _ = event_tx.send((eid.clone(), AgentEvent::Completed));
    }
    let _ = event_tx.send((eid.clone(), AgentEvent::Exited { code }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::MemoryBackend;
    use crate::provider::MockProviderConnection;

    #[test]
    fn builtin_registry_lists_agents() {
        let r = AgentRegistry::new();
        let ids: Vec<&str> = r.list().iter().map(|d| d.id.as_str()).collect();
        for id in ["claude-code", "codex", "opencode", "pi", "fake-agent"] {
            assert!(ids.contains(&id), "missing {id}");
        }
        assert!(r.find_adapter("claude-code").is_some());
        assert!(r.find_adapter("nope").is_none());
    }

    #[test]
    fn adapter_capabilities_are_honest() {
        let r = AgentRegistry::new();
        for id in ["claude-code", "codex", "opencode", "pi", "fake-agent"] {
            let a = r.find_adapter(id).unwrap();
            let caps = a.capabilities();
            assert!(caps.spawn && caps.interactive && caps.stop && caps.restart && caps.resize);
            // Nothing claims structured events/usage/cost until verified.
            assert!(!caps.structured_events && !caps.usage && !caps.cost);
            assert!(!caps.pause);
        }
        // Claude is the only resume-capable agent in Phase 2B.
        assert!(r.find_adapter("claude-code").unwrap().capabilities().resume);
        assert!(!r.find_adapter("codex").unwrap().capabilities().resume);
    }

    #[test]
    fn launch_without_provider_spawns_no_env() {
        let pty = Arc::new(PtyManager::new().unwrap());
        let store = CredentialStore::with_backend(Arc::new(MemoryBackend::new()));
        let mut runtime = AgentRuntime::new(
            Arc::new(AgentRegistry::new()),
            ProviderRegistry::new(),
            pty,
            store,
            None,
        );
        let launch = AgentLaunchConfig {
            definition_id: "fake-agent".into(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            arguments: vec!["--scenario".into(), "completion".into()],
            provider_id: None,
            model_id: None,
            credential_ref: None,
            resume_id: None,
            environment: vec![],
        };
        // Spawn requires the fake-agent binary; without it this is a skip.
        if crate::adapters::fake::FakeAgentAdapter::resolve_binary().is_err() {
            eprintln!("skipped: fake-agent binary not built");
            return;
        }
        // Environment injection is exercised in the integration suite;
        // here we only assert spawn wiring works.
        let (_eid, session) = runtime.spawn(launch, 80, 24).unwrap();
        assert!(!session.has_exited());
        drop(runtime);
        let _ = MockProviderConnection::new(crate::provider::MockMode::Ok);
    }
}
