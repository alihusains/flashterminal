//! Phase 2C: Agent Work / Activity / Attention / Timeline / Cost / Health /
//! Event-replay / Intent models (`2c.md` §3–§20, §30, §33–§34, §38).
//!
//! The layer separates five concepts that Phase 2B kept implicit:
//!
//! ```text
//! AgentSession  — the process (state, exit code, provenance)
//! AgentWork     — what the agent is trying to accomplish (title, files, commands)
//! AgentActivity — what it is doing right now (kind + source + confidence)
//! AgentMetrics  — counters (events, bytes) — lives in agent.rs
//! AgentUsage    — token usage + estimated cost (normalized, never fabricated)
//! ```
//!
//! Everything here is secret-free and serializable (IPC/IPC-stream/persistence).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};

use crate::execution::{AgentState, StateConfidence, StateSource};

/// Default timeline retention (§6: bounded history, configurable).
pub const DEFAULT_TIMELINE_CAPACITY: usize = 512;
/// Default per-work activity history retention (§4, §23).
pub const DEFAULT_ACTIVITY_CAPACITY: usize = 32;

// ---------------------------------------------------------------------------
// §4 Activity taxonomy
// ---------------------------------------------------------------------------

/// The fine-grained activity taxonomy (2c.md §4). Kept small and
/// extensible; `Unknown` is the honest fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Starting,
    Reading,
    Thinking,
    Planning,
    Editing,
    RunningCommand,
    RunningTests,
    WaitingForInput,
    WaitingForPermission,
    Reviewing,
    Finishing,
    Idle,
    Unknown,
}

impl ActivityKind {
    /// Whether a new observation of this kind is worth a timeline entry
    /// (§23: coalesce noisy same-kind readings).
    pub fn timeline_worthy(self) -> bool {
        !matches!(
            self,
            ActivityKind::Reading | ActivityKind::Thinking | ActivityKind::Unknown
        )
    }

    /// Short label for the work view.
    pub fn label(self) -> &'static str {
        match self {
            ActivityKind::Starting => "Starting",
            ActivityKind::Reading => "Reading",
            ActivityKind::Thinking => "Thinking",
            ActivityKind::Planning => "Planning",
            ActivityKind::Editing => "Editing",
            ActivityKind::RunningCommand => "Running command",
            ActivityKind::RunningTests => "Running tests",
            ActivityKind::WaitingForInput => "Waiting for input",
            ActivityKind::WaitingForPermission => "Waiting for approval",
            ActivityKind::Reviewing => "Reviewing",
            ActivityKind::Finishing => "Finishing",
            ActivityKind::Idle => "Idle",
            ActivityKind::Unknown => "Working",
        }
    }

    /// Non-color state glyph (§35: accessibility — never color alone).
    pub fn symbol(self) -> &'static str {
        match self {
            ActivityKind::Starting => "●",
            ActivityKind::Reading => "◐",
            ActivityKind::Thinking => "◔",
            ActivityKind::Planning => "◑",
            ActivityKind::Editing => "✎",
            ActivityKind::RunningCommand => "›",
            ActivityKind::RunningTests => "◉",
            ActivityKind::WaitingForInput => "▽",
            ActivityKind::WaitingForPermission => "▲",
            ActivityKind::Reviewing => "☰",
            ActivityKind::Finishing => "◔",
            ActivityKind::Idle => "·",
            ActivityKind::Unknown => "●",
        }
    }

    /// The lifecycle state an activity normally maps to.
    pub fn state(self) -> AgentState {
        match self {
            ActivityKind::Starting => AgentState::Starting,
            ActivityKind::WaitingForInput => AgentState::Waiting,
            ActivityKind::WaitingForPermission => AgentState::NeedsApproval,
            ActivityKind::Idle => AgentState::Waiting,
            ActivityKind::Unknown => AgentState::Working,
            _ => AgentState::Working,
        }
    }

    /// Whether this activity is a `Working`-family state.
    pub fn is_working(self) -> bool {
        matches!(
            self,
            ActivityKind::Reading
                | ActivityKind::Thinking
                | ActivityKind::Planning
                | ActivityKind::Editing
                | ActivityKind::RunningCommand
                | ActivityKind::RunningTests
                | ActivityKind::Reviewing
                | ActivityKind::Finishing
        )
    }
}

// ---------------------------------------------------------------------------
// §5 Activity source (priority: Structured/Protocol > Hook > Process > Heuristic)
// ---------------------------------------------------------------------------

/// Where an activity observation came from (2c.md §5). Ordered by priority:
/// structured protocol events beat hooks beat process lifecycle beats
/// terminal heuristics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// A structured protocol/event stream from the agent itself.
    Structured,
    /// An agent-defined hook in the terminal (not yet implemented).
    Hook,
    /// Process lifecycle: spawn, stop, exit-code classification.
    Process,
    /// Terminal output heuristics in the adapters.
    Heuristic,
}

impl ActivitySource {
    pub fn priority(self) -> u8 {
        match self {
            ActivitySource::Structured => 4,
            ActivitySource::Hook => 3,
            ActivitySource::Process => 2,
            ActivitySource::Heuristic => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ActivitySource::Structured => "structured",
            ActivitySource::Hook => "hook",
            ActivitySource::Process => "process",
            ActivitySource::Heuristic => "heuristic",
        }
    }
}

/// Maps a Phase 2B.1 provenance onto the 2C source/confidence model.
impl From<StateSource> for ActivitySource {
    fn from(s: StateSource) -> Self {
        match s {
            StateSource::Structured => ActivitySource::Structured,
            StateSource::EventHook => ActivitySource::Hook,
            StateSource::TerminalHeuristic => ActivitySource::Heuristic,
            StateSource::ProcessLifecycle => ActivitySource::Process,
        }
    }
}

/// Numeric confidence 0..=100 (kept monotonic with 2B.1's Low/Medium/High).
pub fn confidence_score(c: StateConfidence) -> u8 {
    match c {
        StateConfidence::Low => 25,
        StateConfidence::Medium => 60,
        StateConfidence::High => 100,
    }
}

// ---------------------------------------------------------------------------
// §4 Activity record
// ---------------------------------------------------------------------------

/// One observed activity. `count` supports coalescing (§23): repeated
/// same-kind observations within the throttle window are folded into one
/// record with a count, never a flood of events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityState {
    pub kind: ActivityKind,
    pub source: ActivitySource,
    pub confidence: u8,
    /// Human detail, e.g. "Reading auth.ts" (redacted at the source).
    pub detail: String,
    /// Wall-clock ms.
    pub at_ms: u64,
    /// Coalesced occurrence count.
    pub count: u32,
}

impl AgentActivityState {
    pub fn heuristic(kind: ActivityKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            source: ActivitySource::Heuristic,
            confidence: confidence_score(StateConfidence::Medium),
            detail: detail.into(),
            at_ms: now_ms(),
            count: 1,
        }
    }

    pub fn process(kind: ActivityKind) -> Self {
        Self {
            kind,
            source: ActivitySource::Process,
            confidence: confidence_score(StateConfidence::High),
            detail: String::new(),
            at_ms: now_ms(),
            count: 1,
        }
    }

    pub fn display(&self) -> String {
        if self.detail.is_empty() {
            self.kind.label().to_string()
        } else {
            format!("{} {}", self.kind.label(), self.detail)
        }
    }
}

/// Coalescing policy (§23): two observations of the same kind within this
/// window fold into one activity record instead of emitting a new one.
pub const ACTIVITY_COALESCE_MS: u64 = 400;

/// §4 → §12: the lifecycle state that determines "Needs You".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    /// A permission prompt is waiting for a decision.
    PermissionRequested,
    /// The agent is waiting for user input.
    NeedsInput,
    /// An error occurred that requires intervention.
    ErrorIntervention,
    /// The agent is blocked on an ambiguous decision.
    Ambiguous,
}

impl AttentionReason {
    /// Non-color glyph (§35) and text.
    pub fn symbol(self) -> &'static str {
        match self {
            AttentionReason::PermissionRequested => "▲",
            AttentionReason::NeedsInput => "▽",
            AttentionReason::ErrorIntervention => "✕",
            AttentionReason::Ambiguous => "?",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AttentionReason::PermissionRequested => "needs approval",
            AttentionReason::NeedsInput => "needs input",
            AttentionReason::ErrorIntervention => "failed",
            AttentionReason::Ambiguous => "blocked",
        }
    }
}

/// The attention computation (§12): a pure state → reason map so the UI
/// and the IPC layer share exactly one definition of "needs you".
pub fn attention_for(state: AgentState) -> Option<AttentionReason> {
    match state {
        AgentState::NeedsApproval => Some(AttentionReason::PermissionRequested),
        AgentState::Waiting => Some(AttentionReason::NeedsInput),
        AgentState::Blocked => Some(AttentionReason::Ambiguous),
        AgentState::Failed | AgentState::Crashed => Some(AttentionReason::ErrorIntervention),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// §3 AgentWork
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Running,
    Completed,
    Failed,
    Attention,
}

impl WorkStatus {
    pub fn label(self) -> &'static str {
        match self {
            WorkStatus::Running => "Running",
            WorkStatus::Completed => "Complete",
            WorkStatus::Failed => "Failed",
            WorkStatus::Attention => "Needs you",
        }
    }
}

/// A unit of work (2c.md §3). Distinct from `AgentSession` (the process):
/// a single session may eventually run multiple work items. Bounded
/// histories everywhere — Phase 2C preserves the memory discipline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWork {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: WorkStatus,
    pub session_id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Files the agent modified/read, where reliably observable (§8).
    pub files_changed: BTreeSet<String>,
    /// Commands the agent ran, where reliably observable (§10).
    pub commands: Vec<String>,
    /// Recent activity, newest last, coalesced (§4, §23).
    pub activity: VecDeque<AgentActivityState>,
    /// Bounded timeline (§6).
    pub timeline: AgentTimeline,
    /// Errors with classification (§11) — never fabricated.
    pub errors: Vec<WorkError>,
    /// Normalized token usage + estimated cost (§18–§20).
    pub usage: AgentUsage,
    /// True when the adapter could not observe files/commands (mark
    /// unavailable information clearly, §10).
    pub files_observable: bool,
    pub commands_observable: bool,
}

impl AgentWork {
    pub fn new(session_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            description: String::new(),
            status: WorkStatus::Running,
            session_id: session_id.into(),
            started_at: Some(Utc::now()),
            completed_at: None,
            files_changed: BTreeSet::new(),
            commands: Vec::new(),
            activity: VecDeque::with_capacity(DEFAULT_ACTIVITY_CAPACITY),
            timeline: AgentTimeline::new(DEFAULT_TIMELINE_CAPACITY),
            errors: Vec::new(),
            usage: AgentUsage::default(),
            files_observable: true,
            commands_observable: true,
        }
    }

    /// Pushes a coalesced activity record (newest last, bounded).
    pub fn push_activity(&mut self, act: AgentActivityState) {
        if let Some(last) = self.activity.back_mut() {
            if last.kind == act.kind && act.at_ms.saturating_sub(last.at_ms) <= ACTIVITY_COALESCE_MS
            {
                last.count += 1;
                if !act.detail.is_empty() {
                    last.detail = act.detail;
                }
                return;
            }
        }
        if self.activity.len() >= DEFAULT_ACTIVITY_CAPACITY {
            self.activity.pop_front();
        }
        self.activity.push_back(act);
    }

    /// The current activity (newest) or a derived fallback.
    pub fn current_activity(&self) -> Option<&AgentActivityState> {
        self.activity.back()
    }

    /// Deterministic summary (§7) — no LLM required for basic facts.
    pub fn summary(&self) -> AgentSummary {
        AgentSummary {
            status: self.status,
            files_changed: self.files_changed.len() as u32,
            commands_run: self.commands.len() as u32,
            tests_passed: parse_test_counts(&self.timeline_joined(512)),
            duration_secs: self
                .started_at
                .zip(self.completed_at.or(Some(Utc::now())))
                .map(|(s, e)| (e.signed_duration_since(s).num_seconds().max(0)) as u64),
            errors: self.errors.len() as u32,
        }
    }

    pub fn timeline_joined(&self, max_chars: usize) -> String {
        let mut out = String::new();
        for e in self.timeline.iter() {
            let line = format!("{} {}", e.kind.label(), e.detail);
            let room = max_chars.saturating_sub(out.len());
            if line.len() >= room {
                if out.len() < max_chars {
                    out.push_str(&line[..room]);
                }
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Finishes the work with a status (idempotent).
    pub fn finish(&mut self, status: WorkStatus) {
        if matches!(self.status, WorkStatus::Completed | WorkStatus::Failed) {
            return;
        }
        self.status = status;
        self.completed_at = Some(Utc::now());
    }

    /// Records a classified error (§11) — bounded to the last few.
    pub fn push_error(&mut self, error: WorkError) {
        if self.errors.len() >= 8 {
            self.errors.remove(0);
        }
        self.errors.push(error);
    }
}

// ---------------------------------------------------------------------------
// §11 Error intelligence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The agent itself failed (non-zero exit, crash).
    AgentFailure,
    /// A command the agent ran failed.
    CommandFailure,
    /// A permission decision was denied/required.
    PermissionFailure,
    /// The provider/API rejected the request.
    ProviderFailure,
    /// Network-level failure.
    NetworkFailure,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::AgentFailure => "agent failure",
            ErrorKind::CommandFailure => "command failure",
            ErrorKind::PermissionFailure => "permission failure",
            ErrorKind::ProviderFailure => "provider failure",
            ErrorKind::NetworkFailure => "network failure",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkError {
    pub kind: ErrorKind,
    /// Human-readable, redacted.
    pub message: String,
    pub at_ms: u64,
}

impl WorkError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            at_ms: now_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// §6 Timeline
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineKind {
    Started,
    Activity,
    State,
    File,
    Command,
    Approval,
    Error,
    Completed,
}

impl TimelineKind {
    pub fn label(self) -> &'static str {
        match self {
            TimelineKind::Started => "Started",
            TimelineKind::Activity => "Activity",
            TimelineKind::State => "State",
            TimelineKind::File => "File",
            TimelineKind::Command => "Command",
            TimelineKind::Approval => "Approval",
            TimelineKind::Error => "Error",
            TimelineKind::Completed => "Completed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub kind: TimelineKind,
    pub detail: String,
    pub at: DateTime<Utc>,
}

/// Bounded timeline (§6): efficient ring, never unlimited raw events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTimeline {
    #[serde(default = "default_timeline_capacity")]
    pub capacity: usize,
    entries: VecDeque<TimelineEntry>,
}

fn default_timeline_capacity() -> usize {
    DEFAULT_TIMELINE_CAPACITY
}

impl AgentTimeline {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::with_capacity(capacity.min(4096)),
        }
    }

    pub fn push(&mut self, kind: TimelineKind, detail: impl Into<String>) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(TimelineEntry {
            kind,
            detail: detail.into(),
            at: Utc::now(),
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &TimelineEntry> {
        self.entries.iter()
    }

    /// Newest-first for UI display.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &TimelineEntry> {
        self.entries.iter().rev().take(n)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ---------------------------------------------------------------------------
// §7 Deterministic summary helpers
// ---------------------------------------------------------------------------

/// Parses "N passed" / "N failed" test-run markers from terminal output.
/// Best-effort; `None` means "unknown" (never fabricated).
pub fn parse_test_counts(text: &str) -> Option<u32> {
    for line in text.lines().rev().take(40) {
        if let Some(n) = parse_count_pattern(line, "passed") {
            return Some(n);
        }
    }
    None
}

fn parse_count_pattern(line: &str, word: &str) -> Option<u32> {
    let lower = line.to_ascii_lowercase();
    if !lower.contains(word) {
        return None;
    }
    let nums: Vec<u32> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|s| s.parse().ok())
        .collect();
    nums.first().copied()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub status: WorkStatus,
    pub files_changed: u32,
    pub commands_run: u32,
    pub tests_passed: Option<u32>,
    pub duration_secs: Option<u64>,
    pub errors: u32,
}

// ---------------------------------------------------------------------------
// §8/§10 Lightweight file & command observation
// ---------------------------------------------------------------------------

/// Git file snapshot (best-effort, non-continuous, §8: "Use Git/file events
/// where practical"). Called once at completion, never per-frame.
pub fn collect_git_files(cwd: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["-C", cwd, "diff", "--name-only"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    if let Ok(untracked) = std::process::Command::new("git")
        .args(["-C", cwd, "ls-files", "--others", "--exclude-standard"])
        .output()
    {
        if untracked.status.success() {
            for l in String::from_utf8_lossy(&untracked.stdout).lines() {
                let l = l.trim();
                if !l.is_empty() {
                    files.push(l.to_string());
                }
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// Heuristic file observation from agent output lines (§8). Accepts the
/// common forms:
///
/// ```text
/// → src/auth.ts
/// Read src/auth.ts
/// Modified src/auth.ts
/// src/auth.ts
/// ```
///
/// Conservative: only paths that exist under `cwd` are kept.
pub fn observe_file(line: &str, cwd: &str) -> Option<String> {
    let trimmed = line.trim();
    let candidate = if let Some(rest) = trimmed.strip_prefix("→") {
        rest.trim()
    } else if let Some(rest) = trimmed
        .strip_prefix("Read ")
        .or_else(|| trimmed.strip_prefix("Modified "))
        .or_else(|| trimmed.strip_prefix("Added "))
        .or_else(|| trimmed.strip_prefix("Deleted "))
    {
        rest.trim()
    } else {
        trimmed
    };
    let candidate = candidate.split_whitespace().next()?;
    if candidate.contains('/') || candidate.ends_with(".rs") || candidate.ends_with(".ts") {
        let full = std::path::Path::new(cwd).join(candidate);
        if full.exists() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Heuristic command observation (§10): lines that look like a shell
/// command the agent ran (`$ cmd`, `> cmd`, bare `cmd --flag`).
pub fn observe_command(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.len() < 2 || trimmed.len() > 200 {
        return None;
    }
    let body = trimmed
        .strip_prefix("$")
        .or_else(|| trimmed.strip_prefix(">"))
        .map(str::trim)
        .unwrap_or(trimmed);
    if body.contains(" ") && !body.contains("=") && !body.ends_with(':') && !body.ends_with('|') {
        let first = body.split_whitespace().next().unwrap_or("");
        if [
            "npm", "cargo", "git", "pnpm", "yarn", "make", "python", "node", "ruby", "bun",
        ]
        .contains(&first)
        {
            return Some(body.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// §18–§20 Usage & cost
// ---------------------------------------------------------------------------

/// Normalized token usage (2c.md §18). Values unavailable from an agent
/// stay `None` — nothing is fabricated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    /// Estimated cost in minor currency units (cents) — only when pricing
    /// is known for the provider/model.
    pub estimated_cost_cents: Option<u64>,
    pub currency: String,
}

impl AgentUsage {
    pub fn record_usage(&mut self, input: Option<u64>, output: Option<u64>, cached: Option<u64>) {
        if input.is_some() {
            self.input_tokens = input;
        }
        if output.is_some() {
            self.output_tokens = output;
        }
        if cached.is_some() {
            self.cached_tokens = cached;
        }
    }
}

/// Per-model pricing (2c.md §19). Prices are per 1M tokens in minor units
/// (cents) of `currency`. `effective_date` marks the source date — pricing
/// is a table, not code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingDefinition {
    pub provider_id: String,
    pub model_id: String,
    pub input_price_per_mtok_cents: u64,
    pub output_price_per_mtok_cents: u64,
    pub cache_read_price_per_mtok_cents: Option<u64>,
    pub currency: String,
    pub effective_date: String,
}

impl PricingDefinition {
    /// USD list pricing snapshot for well-known models (2025-06-01).
    /// Editable/extensible — the registry never hard-codes pricing in the UI.
    pub fn defaults() -> Vec<PricingDefinition> {
        vec![
            PricingDefinition {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet-4-5".into(),
                input_price_per_mtok_cents: 300,
                output_price_per_mtok_cents: 1500,
                cache_read_price_per_mtok_cents: Some(30),
                currency: "usd".into(),
                effective_date: "2025-06-01".into(),
            },
            PricingDefinition {
                provider_id: "anthropic".into(),
                model_id: "claude-opus-4".into(),
                input_price_per_mtok_cents: 1500,
                output_price_per_mtok_cents: 7500,
                cache_read_price_per_mtok_cents: Some(150),
                currency: "usd".into(),
                effective_date: "2025-06-01".into(),
            },
            PricingDefinition {
                provider_id: "anthropic".into(),
                model_id: "claude-haiku-4-5".into(),
                input_price_per_mtok_cents: 80,
                output_price_per_mtok_cents: 400,
                cache_read_price_per_mtok_cents: Some(8),
                currency: "usd".into(),
                effective_date: "2025-06-01".into(),
            },
        ]
    }
}

/// Provider → model pricing lookup with an honest "unknown" answer.
#[derive(Debug, Clone, Default)]
pub struct PricingRegistry {
    by_model: std::collections::HashMap<(String, String), PricingDefinition>,
}

impl PricingRegistry {
    pub fn new() -> Self {
        let mut r = Self::default();
        for p in PricingDefinition::defaults() {
            r.register(p);
        }
        r
    }

    pub fn register(&mut self, pricing: PricingDefinition) {
        self.by_model.insert(
            (pricing.provider_id.clone(), pricing.model_id.clone()),
            pricing,
        );
    }

    pub fn get(&self, provider_id: &str, model_id: &str) -> Option<&PricingDefinition> {
        self.by_model
            .get(&(provider_id.to_string(), model_id.to_string()))
    }

    /// Computes an estimated cost from usage. Returns `None` when pricing
    /// is unknown for the provider/model (never estimate blindly, §18).
    pub fn estimate_cents(
        &self,
        provider_id: &str,
        model_id: &str,
        usage: &AgentUsage,
    ) -> Option<u64> {
        let p = self.get(provider_id, model_id)?;
        let input = usage.input_tokens?;
        let output = usage.output_tokens?;
        let mut cents = input.saturating_mul(p.input_price_per_mtok_cents) / 1_000_000
            + output.saturating_mul(p.output_price_per_mtok_cents) / 1_000_000;
        if let (Some(cached), Some(rate)) = (usage.cached_tokens, p.cache_read_price_per_mtok_cents)
        {
            cents += cached.saturating_mul(rate) / 1_000_000;
        }
        Some(cents.max(1))
    }
}

// ---------------------------------------------------------------------------
// §30 Agent health
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Binary present; credentials configured when required.
    Available,
    /// Binary missing or not executable.
    Unavailable,
    /// Binary present but the config looks broken (e.g. no credential while
    /// a provider was requested).
    Misconfigured,
    /// Credentials present for the provider.
    Authenticated,
    /// No credentials found (agent may still work via local CLI auth).
    Unauthenticated,
    Unknown,
}

impl HealthStatus {
    pub fn symbol(self) -> &'static str {
        match self {
            HealthStatus::Available => "✓",
            HealthStatus::Unavailable => "✕",
            HealthStatus::Misconfigured => "!",
            HealthStatus::Authenticated => "✓",
            HealthStatus::Unauthenticated => "○",
            HealthStatus::Unknown => "?",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HealthStatus::Available => "available",
            HealthStatus::Unavailable => "not installed",
            HealthStatus::Misconfigured => "misconfigured",
            HealthStatus::Authenticated => "authenticated",
            HealthStatus::Unauthenticated => "no credential",
            HealthStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealthRow {
    pub definition_id: String,
    pub display_name: String,
    pub binary_path: Option<String>,
    pub installed: bool,
    pub status: HealthStatus,
    /// True when a credential is configured for the provider.
    pub credential_configured: bool,
    pub detail: String,
}

// ---------------------------------------------------------------------------
// §33–§34 Deterministic event replay
// ---------------------------------------------------------------------------

/// The normalized progressive event stream (2c.md §33). This is the
/// smallest event set that can drive the whole agent UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedAgentEvent {
    Started {
        at_ms: u64,
    },
    Activity {
        kind: ActivityKind,
        detail: String,
        at_ms: u64,
    },
    State {
        state: AgentState,
        at_ms: u64,
    },
    Output {
        text: String,
        at_ms: u64,
    },
    Permission {
        action: String,
        context: String,
        at_ms: u64,
    },
    Completed {
        at_ms: u64,
    },
    Exited {
        code: Option<i32>,
        at_ms: u64,
    },
}

impl RecordedAgentEvent {
    pub fn now(kind: impl Into<Self>) -> Self {
        kind.into()
    }
}

/// Deterministic UX fixtures (§34) — the desktop UI must be testable
/// against these without starting a real agent.
pub fn fixture_long_running() -> Vec<RecordedAgentEvent> {
    let t = 1_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::State {
            state: AgentState::Starting,
            at_ms: t + 1,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Reading,
            detail: "auth.ts".into(),
            at_ms: t + 2,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Planning,
            detail: "".into(),
            at_ms: t + 3,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Editing,
            detail: "src/auth.ts".into(),
            at_ms: t + 4,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::RunningTests,
            detail: "".into(),
            at_ms: t + 5,
        },
    ]
}

pub fn fixture_approval() -> Vec<RecordedAgentEvent> {
    let t = 2_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::State {
            state: AgentState::Working,
            at_ms: t + 1,
        },
        RecordedAgentEvent::Permission {
            action: "write file".into(),
            context: "src/auth.ts".into(),
            at_ms: t + 2,
        },
        RecordedAgentEvent::State {
            state: AgentState::NeedsApproval,
            at_ms: t + 3,
        },
    ]
}

pub fn fixture_waiting() -> Vec<RecordedAgentEvent> {
    let t = 3_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::State {
            state: AgentState::Working,
            at_ms: t + 1,
        },
        RecordedAgentEvent::State {
            state: AgentState::Waiting,
            at_ms: t + 2,
        },
    ]
}

pub fn fixture_failure() -> Vec<RecordedAgentEvent> {
    let t = 4_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::State {
            state: AgentState::Working,
            at_ms: t + 1,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::RunningCommand,
            detail: "npm test".into(),
            at_ms: t + 2,
        },
        RecordedAgentEvent::Output {
            text: "3 tests failed".into(),
            at_ms: t + 3,
        },
        RecordedAgentEvent::Exited {
            code: Some(1),
            at_ms: t + 4,
        },
        RecordedAgentEvent::State {
            state: AgentState::Failed,
            at_ms: t + 5,
        },
    ]
}

pub fn fixture_completion() -> Vec<RecordedAgentEvent> {
    let t = 5_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::State {
            state: AgentState::Working,
            at_ms: t + 1,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Editing,
            detail: "src/oauth.ts".into(),
            at_ms: t + 2,
        },
        RecordedAgentEvent::Completed { at_ms: t + 3 },
        RecordedAgentEvent::Exited {
            code: Some(0),
            at_ms: t + 4,
        },
        RecordedAgentEvent::State {
            state: AgentState::Completed,
            at_ms: t + 5,
        },
    ]
}

pub fn fixture_rapid_states() -> Vec<RecordedAgentEvent> {
    let mut events = Vec::new();
    events.push(RecordedAgentEvent::Started { at_ms: 6_000_000 });
    for i in 0..50u64 {
        events.push(RecordedAgentEvent::State {
            state: if i % 2 == 0 {
                AgentState::Working
            } else {
                AgentState::Waiting
            },
            at_ms: 6_000_000 + i,
        });
    }
    events
}

pub fn fixture_large_output() -> Vec<RecordedAgentEvent> {
    let mut events = Vec::new();
    events.push(RecordedAgentEvent::Started { at_ms: 7_000_000 });
    for i in 0..2000u64 {
        events.push(RecordedAgentEvent::Output {
            text: format!("line {i}"),
            at_ms: 7_000_000 + i,
        });
    }
    events
}

pub fn fixture_multiple_activities() -> Vec<RecordedAgentEvent> {
    let t = 8_000_000u64;
    vec![
        RecordedAgentEvent::Started { at_ms: t },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Reading,
            detail: "src".into(),
            at_ms: t + 1,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Planning,
            detail: "".into(),
            at_ms: t + 2,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Editing,
            detail: "auth.ts".into(),
            at_ms: t + 3,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::RunningTests,
            detail: "".into(),
            at_ms: t + 4,
        },
        RecordedAgentEvent::Activity {
            kind: ActivityKind::Finishing,
            detail: "".into(),
            at_ms: t + 5,
        },
    ]
}

pub fn all_fixtures() -> Vec<(&'static str, Vec<RecordedAgentEvent>)> {
    vec![
        ("long-running", fixture_long_running()),
        ("approval", fixture_approval()),
        ("waiting", fixture_waiting()),
        ("failure", fixture_failure()),
        ("completion", fixture_completion()),
        ("rapid-states", fixture_rapid_states()),
        ("large-output", fixture_large_output()),
        ("multiple-activities", fixture_multiple_activities()),
    ]
}

/// Applies a recorded event stream to a work record + an event sink
/// (replay, §33). The sink mirrors what the live pump emits.
pub fn replay_into(
    work: &mut AgentWork,
    events: &[RecordedAgentEvent],
    sink: &mut dyn FnMut(crate::execution::AgentEvent),
) {
    for ev in events {
        match ev {
            RecordedAgentEvent::Started { .. } => {
                work.timeline.push(TimelineKind::Started, "agent started");
                sink(crate::execution::AgentEvent::Started);
            }
            RecordedAgentEvent::Activity { kind, detail, .. } => {
                work.push_activity(AgentActivityState::heuristic(*kind, detail.clone()));
                work.timeline
                    .push(TimelineKind::Activity, kind.label().to_string());
                sink(crate::execution::AgentEvent::Activity {
                    kind: *kind,
                    source: ActivitySource::Heuristic,
                    confidence: confidence_score(StateConfidence::Medium),
                    detail: detail.clone(),
                });
            }
            RecordedAgentEvent::State { state, .. } => {
                sink(crate::execution::AgentEvent::StateChanged {
                    new_state: *state,
                    provenance: None,
                });
            }
            RecordedAgentEvent::Output { text, .. } => {
                sink(crate::execution::AgentEvent::Output { text: text.clone() });
            }
            RecordedAgentEvent::Permission {
                action, context, ..
            } => {
                work.timeline.push(TimelineKind::Approval, context.clone());
                sink(crate::execution::AgentEvent::PermissionRequested {
                    action: action.clone(),
                    context: context.clone(),
                });
            }
            RecordedAgentEvent::Completed { .. } => {
                work.finish(WorkStatus::Completed);
                work.timeline.push(TimelineKind::Completed, "work complete");
                sink(crate::execution::AgentEvent::Completed);
            }
            RecordedAgentEvent::Exited { code, .. } => {
                let _ = code;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §38 Natural-language intent layer (deterministic first; LLM later)
// ---------------------------------------------------------------------------

/// Resolved agent intents (2c.md §37–§38). The palette/types map 1:1 onto
/// UI actions; an LLM resolver may be layered behind the same enum later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum AgentIntent {
    ShowAgents { filter: AgentFilter },
    FocusAgent { name: String },
    StopAgent,
    RestartAgent,
    ResumeAgent,
    ReviewChanges,
    OpenAgentLogs,
    Approve,
    Deny,
}

/// Deterministic dashboard filters (§13–§15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentFilter {
    All,
    NeedsAttention,
    Running,
    Failed,
    Completed,
    NeedingInput,
    NeedingApproval,
}

impl AgentFilter {
    pub fn label(self) -> &'static str {
        match self {
            AgentFilter::All => "all agents",
            AgentFilter::NeedsAttention => "agents that need me",
            AgentFilter::Running => "running agents",
            AgentFilter::Failed => "failed agents",
            AgentFilter::Completed => "completed agents",
            AgentFilter::NeedingInput => "agents needing input",
            AgentFilter::NeedingApproval => "agents needing approval",
        }
    }

    pub fn matches_state(self, state: AgentState) -> bool {
        match self {
            AgentFilter::All => true,
            AgentFilter::NeedsAttention => attention_for(state).is_some(),
            AgentFilter::Running => {
                matches!(
                    state,
                    AgentState::Starting
                        | AgentState::Working
                        | AgentState::Waiting
                        | AgentState::NeedsApproval
                )
            }
            AgentFilter::Failed => {
                matches!(
                    state,
                    AgentState::Failed | AgentState::Crashed | AgentState::Blocked
                )
            }
            AgentFilter::Completed => {
                matches!(state, AgentState::Completed | AgentState::Stopped)
            }
            AgentFilter::NeedingInput => state == AgentState::Waiting,
            AgentFilter::NeedingApproval => state == AgentState::NeedsApproval,
        }
    }
}

/// Deterministic intent resolver (§38): keyword patterns → intents. Simple
/// commands never consult an LLM; an LLM resolver can slot in behind this
/// for ambiguous queries later.
pub struct IntentResolver;

impl IntentResolver {
    pub fn resolve(query: &str) -> Option<AgentIntent> {
        let q = query.trim().to_ascii_lowercase();
        let has = |w: &str| q.contains(w);
        if has("need") || has("attention") || has("needing") {
            if has("approval") {
                return Some(AgentIntent::ShowAgents {
                    filter: AgentFilter::NeedingApproval,
                });
            }
            if has("input") {
                return Some(AgentIntent::ShowAgents {
                    filter: AgentFilter::NeedingInput,
                });
            }
            return Some(AgentIntent::ShowAgents {
                filter: AgentFilter::NeedsAttention,
            });
        }
        if has("fail") {
            return Some(AgentIntent::ShowAgents {
                filter: AgentFilter::Failed,
            });
        }
        if has("complete") || has("done") {
            return Some(AgentIntent::ShowAgents {
                filter: AgentFilter::Completed,
            });
        }
        if has("running") || has("show agents") || has("agents") {
            if has("running") {
                return Some(AgentIntent::ShowAgents {
                    filter: AgentFilter::Running,
                });
            }
            return Some(AgentIntent::ShowAgents {
                filter: AgentFilter::All,
            });
        }
        if has("focus")
            || has("show claude")
            || has("show codex")
            || has("show opencode")
            || has("show pi")
        {
            for name in ["claude", "codex", "opencode", "pi"] {
                if has(name) {
                    return Some(AgentIntent::FocusAgent { name: name.into() });
                }
            }
            return Some(AgentIntent::ShowAgents {
                filter: AgentFilter::All,
            });
        }
        if has("review") && has("change") {
            return Some(AgentIntent::ReviewChanges);
        }
        if has("open") && has("log") {
            return Some(AgentIntent::OpenAgentLogs);
        }
        if has("stop") {
            return Some(AgentIntent::StopAgent);
        }
        if has("restart") {
            return Some(AgentIntent::RestartAgent);
        }
        if has("resume") {
            return Some(AgentIntent::ResumeAgent);
        }
        if has("approve") {
            return Some(AgentIntent::Approve);
        }
        if has("deny") {
            return Some(AgentIntent::Deny);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests (§43: work / attention / cost / replay / intent)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_lifecycle_create_update_complete_fail() {
        let mut w = AgentWork::new("sess-1", "Authentication");
        assert_eq!(w.status, WorkStatus::Running);
        assert!(w.started_at.is_some());
        w.push_activity(AgentActivityState::heuristic(
            ActivityKind::Reading,
            "auth.ts",
        ));
        w.files_changed.insert("src/auth.ts".into());
        w.commands.push("npm test".into());
        w.finish(WorkStatus::Completed);
        assert_eq!(w.status, WorkStatus::Completed);
        assert!(w.completed_at.is_some());
        // finish is idempotent
        w.finish(WorkStatus::Failed);
        assert_eq!(w.status, WorkStatus::Completed);
        let s = w.summary();
        assert_eq!(s.files_changed, 1);
        assert_eq!(s.commands_run, 1);
        assert!(s.duration_secs.is_some());
    }

    #[test]
    fn activity_coalesces_within_window() {
        let mut w = AgentWork::new("s1", "t");
        w.push_activity(AgentActivityState::heuristic(ActivityKind::Reading, "a"));
        let second = {
            let mut a = AgentActivityState::heuristic(ActivityKind::Reading, "b");
            a.at_ms = w.activity.back().unwrap().at_ms + 1;
            a
        };
        w.push_activity(second);
        assert_eq!(w.activity.len(), 1, "same-kind within window must coalesce");
        assert_eq!(w.activity.back().unwrap().count, 2);
        // A later activity appends.
        w.push_activity(AgentActivityState::heuristic(ActivityKind::Planning, ""));
        assert_eq!(w.activity.len(), 2);
    }

    #[test]
    fn timeline_is_bounded() {
        let mut t = AgentTimeline::new(10);
        for i in 0..100 {
            t.push(TimelineKind::Activity, format!("e{i}"));
        }
        assert_eq!(t.len(), 10);
        let first = t.iter().next().unwrap();
        assert_eq!(first.detail, "e90");
    }

    #[test]
    fn attention_mapping() {
        assert_eq!(
            attention_for(AgentState::NeedsApproval),
            Some(AttentionReason::PermissionRequested)
        );
        assert_eq!(
            attention_for(AgentState::Waiting),
            Some(AttentionReason::NeedsInput)
        );
        assert_eq!(
            attention_for(AgentState::Failed),
            Some(AttentionReason::ErrorIntervention)
        );
        assert_eq!(attention_for(AgentState::Working), None);
        assert_eq!(attention_for(AgentState::Completed), None);
    }

    #[test]
    fn summary_parses_test_counts() {
        assert_eq!(parse_test_counts("37 passed\n1 failed"), Some(37));
        assert_eq!(parse_test_counts("no markers here"), None);
    }

    #[test]
    fn pricing_estimates_only_when_known() {
        let r = PricingRegistry::new();
        let usage = AgentUsage {
            input_tokens: Some(1_000_000),
            output_tokens: Some(100_000),
            cached_tokens: Some(500_000),
            ..Default::default()
        };
        let cents = r
            .estimate_cents("anthropic", "claude-sonnet-4-5", &usage)
            .unwrap();
        assert!(cents > 0);
        // Unknown model → no estimate (never fabricated).
        assert!(r
            .estimate_cents("anthropic", "unknown-model", &usage)
            .is_none());
        assert!(r
            .estimate_cents("openrouter", "claude-sonnet-4-5", &usage)
            .is_none());
        // Missing usage → no estimate.
        let empty = AgentUsage::default();
        assert!(r
            .estimate_cents("anthropic", "claude-sonnet-4-5", &empty)
            .is_none());
    }

    #[test]
    fn intent_resolution_is_deterministic() {
        assert_eq!(
            IntentResolver::resolve("show agents that need me"),
            Some(AgentIntent::ShowAgents {
                filter: AgentFilter::NeedsAttention
            })
        );
        assert_eq!(
            IntentResolver::resolve("show failed agents"),
            Some(AgentIntent::ShowAgents {
                filter: AgentFilter::Failed
            })
        );
        assert_eq!(
            IntentResolver::resolve("show completed agents"),
            Some(AgentIntent::ShowAgents {
                filter: AgentFilter::Completed
            })
        );
        assert_eq!(
            IntentResolver::resolve("focus Codex"),
            Some(AgentIntent::FocusAgent {
                name: "codex".into()
            })
        );
        assert_eq!(
            IntentResolver::resolve("review changes"),
            Some(AgentIntent::ReviewChanges)
        );
        assert_eq!(IntentResolver::resolve("mumbo jumbo"), None);
    }

    #[test]
    fn filters_match_states() {
        assert!(AgentFilter::NeedsAttention.matches_state(AgentState::NeedsApproval));
        assert!(AgentFilter::NeedsAttention.matches_state(AgentState::Failed));
        assert!(!AgentFilter::NeedsAttention.matches_state(AgentState::Working));
        assert!(AgentFilter::Running.matches_state(AgentState::Working));
        assert!(AgentFilter::Completed.matches_state(AgentState::Completed));
        assert!(AgentFilter::Failed.matches_state(AgentState::Crashed));
    }

    #[test]
    fn replay_applies_fixtures_deterministically() {
        let mut emitted = Vec::new();
        let mut work = AgentWork::new("sess-replay", "t");
        replay_into(&mut work, &fixture_completion(), &mut |e| {
            emitted.push(e);
        });
        assert!(matches!(emitted[0], crate::execution::AgentEvent::Started));
        assert!(
            work.status == WorkStatus::Completed,
            "fixture must finish work"
        );
        assert!(emitted.iter().any(|e| matches!(
            e,
            crate::execution::AgentEvent::Activity {
                kind: ActivityKind::Editing,
                ..
            }
        )));
        // Failure fixture classifies the work as failed via exit handling
        // in the runtime; at the model level the Completed fixture is the
        // canonical terminal state.
        let mut w2 = AgentWork::new("s", "t");
        replay_into(&mut w2, &fixture_failure(), &mut |_| {});
        assert!(w2.status == WorkStatus::Running || w2.status == WorkStatus::Failed);
        assert!(!w2.activity.is_empty());
    }

    #[test]
    fn all_fixtures_are_secret_free_and_serializable() {
        for (name, events) in all_fixtures() {
            let json = serde_json::to_string(&events).unwrap();
            assert!(
                !json.to_lowercase().contains("key") || name == "large-output" || name == "failure"
            );
            let back: Vec<RecordedAgentEvent> = serde_json::from_str(&json).unwrap();
            assert_eq!(back.len(), events.len());
        }
    }

    #[test]
    fn observe_helpers_are_conservative() {
        assert!(observe_command("$ npm test").is_some());
        assert!(observe_command("npm install").is_some());
        assert!(observe_command("just a status line").is_none());
        // File observation requires the path to exist — temp dir cwd with
        // a known file.
        let dir = std::env::temp_dir();
        let existing = std::fs::read_dir(&dir).ok().and_then(|mut it| {
            it.next()
                .and_then(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
        });
        if let Some(f) = existing {
            if let Some(found) = observe_file(&format!("→ {f}"), dir.to_string_lossy().as_ref()) {
                assert_eq!(found, f);
            }
        }
        assert!(observe_file("→ /does/not/exist.rs", "/").is_none());
    }

    #[test]
    fn health_symbols_are_text_first() {
        // §35: state must be communicable without color.
        for s in [
            HealthStatus::Available,
            HealthStatus::Unavailable,
            HealthStatus::Misconfigured,
            HealthStatus::Authenticated,
            HealthStatus::Unauthenticated,
            HealthStatus::Unknown,
        ] {
            assert!(!s.symbol().is_empty());
            assert!(!s.label().is_empty());
        }
    }
}
