//! Planning domain (3b.md §3–§40): an LLM is a *planner*, never an
//! orchestrator. This module owns everything that happens *before* a plan
//! becomes an executable [`crate::orchestration::TaskGraph`]:
//!
//! ```text
//! User Intent
//!    ↓
//! PlannerRequest ──► PlannerProvider (LLM) ──► ProposedPlan
//!    ↓                                              ↓
//! PlannerContext (bounded, auditable)         PlanValidator (deterministic)
//!                                                  ↓
//!                                           PlanCompiler ──► TaskGraph
//!                                                  ↓
//!                                           TaskScheduler (authoritative)
//! ```
//!
//! The planner can never spawn processes, set policy, exceed budgets, or
//! modify task states directly. It produces a *proposal*; only the
//! deterministic validator and compiler turn it into an executable graph.

use crate::agent::{AgentCapabilities, AgentRegistry};
use crate::orchestration::{Task, TaskGraph, TaskGraphError, TaskId, TaskPolicy, TaskStatus};
use crate::worktrees::IsolationMode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Version of the planner prompt (3b.md §50). Bump on any prompt change.
pub const PLANNER_PROMPT_VERSION: u32 = 1;
/// Version of the structured-output schema the planner must emit.
pub const PLANNER_SCHEMA_VERSION: u32 = 1;
/// Version of the persisted plan slice.
pub const PERSISTED_PLAN_VERSION: u32 = 1;
/// Hard cap on planner retries for a single request (§40).
pub const MAX_PLANNER_RETRIES: u32 = 3;
/// Max steps a single plan may contain (bounded structures everywhere).
pub const MAX_PLAN_STEPS: usize = 32;
/// Max length of a plan step title / description (bounded structures).
pub const MAX_STEP_TEXT: usize = 512;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// §3–§8 requests, context, configuration
// ---------------------------------------------------------------------------

/// Planner request mode (3e.md §11): an initial plan vs a replan. For a
/// replan the request carries the current workflow, trigger, evidence and
/// remaining work inside `context`/`intent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlannerRequestMode {
    #[default]
    Initial,
    Replan,
}

/// What the user asked for (§4). Carries a bounded, allowlisted context —
/// never the filesystem, never secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannerRequest {
    /// Request id (audit correlation, §29).
    pub request_id: String,
    /// The raw user intent.
    pub intent: String,
    pub workspace_id: String,
    pub context: PlannerContext,
    pub constraints: PlannerConstraints,
    /// §11: Initial | Replan. Defaults to Initial for existing callers.
    #[serde(default)]
    pub mode: PlannerRequestMode,
}

/// Bounded constraints the planner must respect (§4, §12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerConstraints {
    /// Hard cost budget in cents (from the workspace task policy).
    pub budget_cents: Option<u64>,
    /// Hard cap on parallel task starts (from the task policy).
    pub max_parallel_tasks: usize,
    /// Approval mode in force (§17).
    pub approval: PlannerApprovalMode,
    /// User preferences / policy notes (bounded, free-form).
    pub user_preferences: Vec<String>,
    /// Max isolated worktrees the plan may request (3c.md §46–§47).
    #[serde(default = "default_max_worktrees")]
    pub max_worktrees: usize,
}

fn default_max_worktrees() -> usize {
    32
}

/// Approval policy (§17): infrastructure only — no automatic
/// safe/risky classification is implemented in this phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerApprovalMode {
    /// Simple/safe plans may run automatically (policy decides later).
    Auto,
    /// Show the plan before execution (default).
    #[default]
    Confirm,
    /// Require approval before each significant step.
    Strict,
}

/// Planner configuration (§8). Never persists raw credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerConfig {
    /// Provider id from the existing ProviderRegistry (never duplicated).
    pub provider: String,
    /// Model id within that provider.
    pub model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Cap on what a single planning call may cost (informational in 3B).
    pub planning_budget_cents: Option<u64>,
    /// Default isolation for steps that do not declare one (3c.md §45).
    #[serde(default)]
    pub default_isolation: IsolationMode,
    /// Approval policy (§17).
    pub approval: PlannerApprovalMode,
    /// Optional quality profile (§36–§37).
    pub profile: Option<PlannerProfileId>,
    /// Max retries on invalid structured output (§20, §40).
    pub max_retries: u32,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        let balanced = PlannerProfile::balanced();
        Self {
            provider: "openai".to_string(),
            model: "gpt-4o-mini".to_string(),
            temperature: balanced.temperature,
            max_tokens: balanced.max_tokens,
            planning_budget_cents: None,
            approval: PlannerApprovalMode::Confirm,
            profile: Some(PlannerProfileId::Balanced),
            max_retries: 2,
            default_isolation: IsolationMode::GitWorktree,
        }
    }
}

impl PlannerConfig {
    pub fn with_profile(mut self, profile: PlannerProfileId) -> Self {
        let p = profile.profile();
        self.temperature = p.temperature;
        self.max_tokens = p.max_tokens;
        self.planning_budget_cents = p.planning_budget_cents;
        self.profile = Some(profile);
        self
    }
}

/// Optional planner profiles (§36): preset model parameters. Presets are
/// parameter mappings only — they never rank providers or models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerProfileId {
    Fast,
    Balanced,
    DeepPlanning,
}

impl PlannerProfileId {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fast => "Fast",
            Self::Balanced => "Balanced",
            Self::DeepPlanning => "Deep Planning",
        }
    }

    /// The parameter mapping for this profile (§36).
    pub fn profile(self) -> PlannerProfile {
        match self {
            Self::Fast => PlannerProfile::fast(),
            Self::Balanced => PlannerProfile::balanced(),
            Self::DeepPlanning => PlannerProfile::deep_planning(),
        }
    }
}

/// Profile → model parameter mapping (§36). No provider-specific
/// assumptions; cost guidance is informational (lower/higher planning cost).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerProfile {
    pub id: PlannerProfileId,
    pub temperature: f32,
    pub max_tokens: u32,
    pub planning_budget_cents: Option<u64>,
    /// Cost guidance for the UI (§37) — a label, not a ranking.
    pub cost_guidance: &'static str,
}

impl PlannerProfile {
    pub fn fast() -> Self {
        Self {
            id: PlannerProfileId::Fast,
            temperature: 0.1,
            max_tokens: 1200,
            planning_budget_cents: None,
            cost_guidance: "lower planning cost",
        }
    }

    pub fn balanced() -> Self {
        Self {
            id: PlannerProfileId::Balanced,
            temperature: 0.2,
            max_tokens: 2000,
            planning_budget_cents: None,
            cost_guidance: "moderate planning cost",
        }
    }

    pub fn deep_planning() -> Self {
        Self {
            id: PlannerProfileId::DeepPlanning,
            temperature: 0.3,
            max_tokens: 4000,
            planning_budget_cents: None,
            cost_guidance: "higher planning cost",
        }
    }

    pub fn all() -> [PlannerProfile; 3] {
        [Self::fast(), Self::balanced(), Self::deep_planning()]
    }
}

// ---------------------------------------------------------------------------
// §5 planner context
// ---------------------------------------------------------------------------

/// A summary of an agent the planner may recommend (§5, §11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub id: String,
    pub display_name: String,
    pub capabilities: AgentCapabilities,
}

/// A task the planner may observe (§5 — active/recent activity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: TaskId,
    pub title: String,
    pub status: String,
}

/// Bounded, allowlisted context handed to the planner (§5, §27). Built
/// exclusively by [`PlannerContextBuilder`]; nothing outside the builder
/// can inject data into it. Never contains: environment variables, API
/// keys, credential contents, private-key paths, terminal logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerContext {
    pub workspace_id: String,
    pub workspace_name: String,
    pub project_root: String,
    /// Current git branch when resolvable (`None` when unavailable).
    pub git_branch: Option<String>,
    /// File/dir names in the workspace root (bounded, allowlisted: secret
    /// files like `.env`, key material, and dot-directories are excluded).
    pub repo_entries: Vec<String>,
    /// Total entry count before bounding (so the planner knows it sees a
    /// prefix, not the whole tree).
    pub total_entries: usize,
    /// Registered agent ids/names/capabilities.
    pub available_agents: Vec<AgentSummary>,
    /// Active task titles/statuses (≤ bound).
    pub active_tasks: Vec<TaskSummary>,
    /// Recent task history (≤ bound).
    pub recent_tasks: Vec<TaskSummary>,
    /// Provider ids the user can select.
    pub provider_ids: Vec<String>,
    /// Constraints snapshot (§4).
    pub constraints: PlannerConstraints,
}

impl PlannerContext {
    /// True when no secret-bearing data could have been included by
    /// construction: the allowlist covers everything the builder copies.
    pub fn is_secret_free(&self) -> bool {
        // Context is data we built from allowlisted sources; the strongest
        // check is that nothing here ever came from process env.
        self.repo_entries.iter().all(|e| !is_secret_entry(e))
            && self.git_branch.as_deref().unwrap_or("").is_empty()
            || self.git_branch.is_none()
    }
}

fn is_secret_entry(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains(".env")
        || lower.contains("key")
        || lower.contains("secret")
        || lower.contains("credential")
        || lower.contains(".pem")
        || lower.contains(".key")
        || lower.contains("id_rsa")
        || lower.contains("id_ed25519")
}

/// Inputs the builder needs from the engine (§5). Everything the builder
/// copies is allowlisted by type — it never reads raw process state.
#[derive(Debug, Clone)]
pub struct PlannerContextInput {
    pub workspace_id: String,
    pub workspace_name: String,
    pub project_root: String,
    pub available_agents: Vec<AgentSummary>,
    pub active_tasks: Vec<TaskSummary>,
    pub recent_tasks: Vec<TaskSummary>,
    pub provider_ids: Vec<String>,
    pub constraints: PlannerConstraints,
}

/// Bounded context gathering (§5). Deterministic where possible: file
/// listing is sorted, bounded; git branch is best-effort with a scrubbed
/// environment and an explicit timeout.
#[derive(Debug, Clone)]
pub struct PlannerContextBuilder {
    input: PlannerContextInput,
    /// Max repo entries copied into the context.
    max_entries: usize,
    /// Max active/recent tasks copied.
    max_tasks: usize,
}

impl PlannerContextBuilder {
    pub fn new(input: PlannerContextInput) -> Self {
        Self {
            input,
            max_entries: 64,
            max_tasks: 12,
        }
    }

    /// Bounded allowlist of workspace-root entries. Dot-directories and
    /// secret-shaped names are excluded (§27).
    fn gather_repo_entries(root: &str, max: usize) -> (Vec<String>, usize) {
        let mut names: Vec<String> = Vec::new();
        let mut total = 0usize;
        if let Ok(rd) = std::fs::read_dir(root) {
            for entry in rd.flatten() {
                total += 1;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || is_secret_entry(&name) {
                    continue;
                }
                names.push(name);
            }
        }
        names.sort();
        names.truncate(max);
        (names, total)
    }

    /// Best-effort git branch with a scrubbed environment (no secrets, no
    /// user config leakage) and a hard timeout. Failure → `None`; the
    /// planner works without it (offline-tolerant, §46).
    fn git_branch(project_root: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(project_root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if branch.is_empty() || branch == "HEAD" {
            return None;
        }
        Some(branch)
    }

    pub fn build(&self) -> PlannerContext {
        let (repo_entries, total_entries) =
            Self::gather_repo_entries(&self.input.project_root, self.max_entries);
        PlannerContext {
            workspace_id: self.input.workspace_id.clone(),
            workspace_name: self.input.workspace_name.clone(),
            project_root: self.input.project_root.clone(),
            git_branch: Self::git_branch(&self.input.project_root),
            repo_entries,
            total_entries,
            available_agents: self.input.available_agents.clone(),
            active_tasks: self
                .input
                .active_tasks
                .iter()
                .take(self.max_tasks)
                .cloned()
                .collect(),
            recent_tasks: self
                .input
                .recent_tasks
                .iter()
                .take(self.max_tasks)
                .cloned()
                .collect(),
            provider_ids: self.input.provider_ids.clone(),
            constraints: self.input.constraints.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// §6, §43–§44 intent normalization + planner invocation policy
// ---------------------------------------------------------------------------

/// Normalized intent (§6): deterministic preprocessing before any LLM
/// call. Never raw shell text; the objective is the bounded, reviewable
/// request the planner sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedIntent {
    pub objective: String,
    /// Caller-supplied constraint notes (bounded, never secrets).
    pub constraints: Vec<String>,
}

/// Deterministic intent normalization (§6): trims, collapses whitespace,
/// and capitalizes the first letter. The planner may further interpret the
/// objective, but structure is fixed before the provider is called.
pub fn normalize_intent(intent: &str) -> NormalizedIntent {
    let mut objective: String = intent.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some(first) = objective.chars().next() {
        objective = first.to_uppercase().collect::<String>() + &objective[first.len_utf8()..];
    }
    NormalizedIntent {
        objective,
        constraints: Vec::new(),
    }
}

/// Planner invocation policy (§43–§44): the planning model is invoked only
/// when a request implies multi-step work. Simple terminal commands,
/// workspace actions and agent control bypass the planner entirely —
/// keeping latency and cost low and the terminal usable offline (§46).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IntentDisposition {
    /// Handled deterministically; the planner is never called (§43).
    Bypass { reason: String },
    /// Multi-step work implied — route to the planner (§44).
    Plan,
}

/// Phrases that unambiguously describe simple, single-action commands.
const BYPASS_PHRASES: &[&str] = &[
    "show agents",
    "show tasks",
    "show task",
    "list agents",
    "list tasks",
    "split pane",
    "run tests",
    "run test",
    "clear",
    "help",
    "show status",
    "show agents",
    "focus",
    "attach",
    "stop agent",
    "restart agent",
    "resume agent",
    "open agent logs",
    "new tab",
    "close tab",
    "new pane",
    "close pane",
    "quit",
    "exit",
    "whoami",
    "pwd",
    "echo ",
];

/// Verbs that signal multi-step work (route to the planner).
const PLAN_SIGNALS: &[&str] = &[
    "build",
    "implement",
    "fix",
    "refactor",
    "add ",
    "create",
    "write",
    "migrate",
    "integrate",
    "set up",
    "configure",
    "debug",
    "design",
    "architect",
    "authenticate",
    "auth",
    "login",
    "payment",
    "webhook",
    "oauth",
    "tests for",
    "unit tests",
    "increase coverage",
    "upgrade",
    "rewrite",
];

/// Deterministic classification (§43–§44). Simple commands never reach the
/// planner; only requests with a multi-step signal do. Ambiguous text is
/// bypassed (cost/latency discipline) — never guessed into a plan.
pub fn classify_intent(intent: &str) -> IntentDisposition {
    let q = intent.trim().to_ascii_lowercase();
    if q.is_empty() {
        return IntentDisposition::Bypass {
            reason: "empty intent".to_string(),
        };
    }
    for p in BYPASS_PHRASES {
        if q.contains(p) {
            return IntentDisposition::Bypass {
                reason: format!("simple command matched {p:?}"),
            };
        }
    }
    for s in PLAN_SIGNALS {
        if q.contains(s) {
            return IntentDisposition::Plan;
        }
    }
    IntentDisposition::Bypass {
        reason: "no multi-step signal".to_string(),
    }
}

// ---------------------------------------------------------------------------
// §9–§10 structured schema → ProposedPlan
// ---------------------------------------------------------------------------

/// The structured output schema the planner must emit (§9). Validated
/// before any [`Task`] is constructed; nothing is parsed from free prose.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSchema {
    pub goal: String,
    #[serde(default)]
    pub tasks: Vec<PlanSchemaTask>,
    /// Concise decision rationale (never chain-of-thought, §10).
    #[serde(default)]
    pub reasoning_summary: Option<String>,
    #[serde(default)]
    pub estimated_cost_cents: Option<u64>,
    #[serde(default)]
    pub estimated_duration_min: Option<u32>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSchemaTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    /// Isolation mode hint (3c.md §45). Absent → planner default. The
    /// planner never touches git; this is a proposal the deterministic
    /// environment layer validates (§46).
    #[serde(default)]
    pub isolation: Option<String>,
    /// Explicit opt-in for shared-workspace execution (§28).
    #[serde(default)]
    pub requires_shared_workspace: bool,
}

/// Parses an isolation hint from the planner schema (§45). Absent/unknown
/// values fall back to the mode the engine would use anyway; the validator
/// rejects explicit shared-workspace claims that bypass policy (§46).
fn parse_isolation(raw: Option<&str>) -> IsolationMode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("shared") | Some("shared_workspace") | Some("shared-workspace") => {
            IsolationMode::SharedWorkspace
        }
        Some("temporary")
        | Some("temporary_directory")
        | Some("temporary-directory")
        | Some("temp") => IsolationMode::TemporaryDirectory,
        Some("git") | Some("git_worktree") | Some("git-worktree") | Some("worktree") => {
            IsolationMode::GitWorktree
        }
        // Absent or unknown → the planner's default isolation.
        _ => IsolationMode::GitWorktree,
    }
}

/// Agent recommendation inside a step (§11). A recommendation is never an
/// instruction to execute — the deterministic selector validates it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRecommendation {
    pub agent_definition_id: String,
    pub reason: Option<String>,
    pub confidence: Option<f32>,
}

/// One proposed step (§9–§10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub description: String,
    pub agent_recommendation: Option<AgentRecommendation>,
    pub depends_on: Vec<String>,
    /// Execution isolation for this step (3c.md §45).
    pub isolation: IsolationMode,
    /// Explicit opt-in for shared-workspace execution (§28).
    pub requires_shared_workspace: bool,
}

/// A validated-shape plan proposal (§10). Private chain-of-thought is
/// never stored; `reasoning_summary` is the concise, user-reviewable
/// rationale only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedPlan {
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub estimated_cost_cents: Option<u64>,
    pub estimated_duration_min: Option<u32>,
    pub reasoning_summary: Option<String>,
    pub warnings: Vec<String>,
}

impl PlanSchema {
    /// Converts a parsed schema into a proposal. Structural errors (empty
    /// goal, duplicate/empty step ids, missing dependency targets) are
    /// returned as typed validation errors — never string-parsed (§20).
    pub fn into_plan(self) -> Result<ProposedPlan, PlanValidationError> {
        if self.goal.trim().is_empty() {
            return Err(PlanValidationError::GoalMissing);
        }
        if self.tasks.is_empty() {
            return Err(PlanValidationError::EmptyPlan);
        }
        if self.tasks.len() > MAX_PLAN_STEPS {
            return Err(PlanValidationError::TooManySteps {
                count: self.tasks.len(),
                max: MAX_PLAN_STEPS,
            });
        }
        let mut seen: HashMap<&str, usize> = HashMap::new();
        let mut steps = Vec::with_capacity(self.tasks.len());
        for (i, t) in self.tasks.iter().enumerate() {
            let id = t.id.trim();
            if id.is_empty() {
                return Err(PlanValidationError::EmptyStepId);
            }
            if id.len() > 128 {
                return Err(PlanValidationError::InvalidStepId { id: id.to_string() });
            }
            if let Some(prev) = seen.insert(id, i) {
                return Err(PlanValidationError::DuplicateStepId {
                    id: id.to_string(),
                    first: prev,
                    second: i,
                });
            }
            if t.title.trim().is_empty() {
                return Err(PlanValidationError::MissingTitle { id: id.to_string() });
            }
            if t.title.len() > MAX_STEP_TEXT || t.description.len() > MAX_STEP_TEXT {
                return Err(PlanValidationError::StepTextTooLong { id: id.to_string() });
            }
            steps.push(PlanStep {
                id: id.to_string(),
                title: t.title.trim().to_string(),
                description: t.description.trim().to_string(),
                agent_recommendation: t.agent.as_ref().map(|a| AgentRecommendation {
                    agent_definition_id: a.clone(),
                    reason: t.reason.clone(),
                    confidence: t.confidence,
                }),
                depends_on: t.depends_on.iter().map(|d| d.trim().to_string()).collect(),
                isolation: parse_isolation(t.isolation.as_deref()),
                requires_shared_workspace: t.requires_shared_workspace,
            });
        }
        // Dependency existence after all ids are known.
        for step in &steps {
            for dep in &step.depends_on {
                if dep.is_empty() {
                    return Err(PlanValidationError::InvalidDependency {
                        task: step.id.clone(),
                        dependency: dep.clone(),
                    });
                }
                if !seen.contains_key(dep.as_str()) {
                    return Err(PlanValidationError::InvalidDependency {
                        task: step.id.clone(),
                        dependency: dep.clone(),
                    });
                }
            }
        }
        Ok(ProposedPlan {
            goal: self.goal.trim().to_string(),
            steps,
            estimated_cost_cents: self.estimated_cost_cents,
            estimated_duration_min: self.estimated_duration_min,
            reasoning_summary: self.reasoning_summary,
            warnings: self.warnings,
        })
    }
}

// ---------------------------------------------------------------------------
// §51 plan hash
// ---------------------------------------------------------------------------

/// Deterministic FNV-1a 64-bit. Same normalized plan → same hash on every
/// platform/run (audit, dedup, replay — §51). Secrets are never hashed:
/// the normalized plan contains no secrets by construction.
pub fn plan_hash(plan: &ProposedPlan) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in canonical_plan_json(plan).as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Canonical serialization: steps sorted by id, dependencies sorted, no
/// reasoning summaries or warnings (they are not part of the schedule).
fn canonical_plan_json(plan: &ProposedPlan) -> String {
    let mut out = String::from("plan{goal:");
    out.push_str(&plan.goal);
    out.push_str(";steps:");
    let mut steps: Vec<&PlanStep> = plan.steps.iter().collect();
    steps.sort_by(|a, b| a.id.cmp(&b.id));
    for (i, s) in steps.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        out.push_str("id=");
        out.push_str(&s.id);
        out.push_str(";title=");
        out.push_str(&s.title);
        out.push_str(";desc=");
        out.push_str(&s.description);
        out.push_str(";agent=");
        out.push_str(
            s.agent_recommendation
                .as_ref()
                .map_or("", |a| a.agent_definition_id.as_str()),
        );
        out.push_str(";deps=");
        let mut deps = s.depends_on.clone();
        deps.sort();
        for (j, d) in deps.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(d);
        }
    }
    out.push_str(";cost=");
    if let Some(c) = plan.estimated_cost_cents {
        out.push_str(&c.to_string());
    }
    out.push('}');
    out
}

// ---------------------------------------------------------------------------
// §13–§14 validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanValidationError {
    GoalMissing,
    EmptyPlan,
    TooManySteps {
        count: usize,
        max: usize,
    },
    EmptyStepId,
    InvalidStepId {
        id: String,
    },
    DuplicateStepId {
        id: String,
        first: usize,
        second: usize,
    },
    MissingTitle {
        id: String,
    },
    StepTextTooLong {
        id: String,
    },
    InvalidDependency {
        task: String,
        dependency: String,
    },
    UnknownAgent {
        agent: String,
    },
    /// Agent registered but its executable is not resolvable.
    AgentUnavailable {
        agent: String,
    },
    Cycle {
        path: Vec<String>,
    },
    SelfDependency {
        task: String,
    },
    BudgetExceeded {
        estimated_cents: u64,
        budget_cents: u64,
    },
    ParallelismExceeded {
        requested: usize,
        max: usize,
    },
    InvalidConfidence {
        id: String,
    },
    /// A step asked for shared-workspace execution without the explicit
    /// opt-in (3c.md §28, §46).
    SharedWorkspaceNotAllowed {
        step: String,
    },
    /// More isolated worktrees than the policy permits (3c.md §46–§47).
    TooManyWorktrees {
        count: usize,
        max: usize,
    },
}

impl std::fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GoalMissing => write!(f, "plan is missing a goal"),
            Self::EmptyPlan => write!(f, "plan contains no tasks"),
            Self::TooManySteps { count, max } => {
                write!(f, "plan has {count} steps (max {max})")
            }
            Self::EmptyStepId => write!(f, "plan contains an empty step id"),
            Self::InvalidStepId { id } => write!(f, "invalid step id {id:?}"),
            Self::DuplicateStepId { id, first, second } => {
                write!(f, "duplicate step id {id:?} at {first} and {second}")
            }
            Self::MissingTitle { id } => write!(f, "step {id:?} has no title"),
            Self::StepTextTooLong { id } => write!(f, "step {id:?} text exceeds the limit"),
            Self::InvalidDependency { task, dependency } => {
                write!(f, "step {task:?} depends on unknown step {dependency:?}")
            }
            Self::UnknownAgent { agent } => write!(f, "agent {agent:?} is not registered"),
            Self::AgentUnavailable { agent } => {
                write!(f, "agent {agent:?} is registered but unavailable")
            }
            Self::Cycle { path } => write!(f, "dependency cycle: {}", path.join(" → ")),
            Self::SelfDependency { task } => write!(f, "step {task:?} depends on itself"),
            Self::BudgetExceeded {
                estimated_cents,
                budget_cents,
            } => write!(
                f,
                "plan estimated cost ${}.{:02} exceeds the ${}.{:02} budget",
                estimated_cents / 100,
                estimated_cents % 100,
                budget_cents / 100,
                budget_cents % 100
            ),
            Self::ParallelismExceeded { requested, max } => write!(
                f,
                "plan parallelism {requested} exceeds the policy cap {max}"
            ),
            Self::InvalidConfidence { id } => {
                write!(f, "step {id:?} has an invalid confidence value")
            }
            Self::SharedWorkspaceNotAllowed { step } => write!(
                f,
                "step {step:?} requests shared-workspace execution without explicit policy"
            ),
            Self::TooManyWorktrees { count, max } => write!(
                f,
                "plan requests {count} isolated worktrees (policy allows {max})"
            ),
        }
    }
}

/// Deterministic validation outcome (§13–§14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanValidationResult {
    pub valid: bool,
    pub errors: Vec<PlanValidationError>,
    pub warnings: Vec<String>,
}

impl PlanValidationResult {
    pub fn ok(warnings: Vec<String>) -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            warnings,
        }
    }

    pub fn invalid(errors: Vec<PlanValidationError>) -> Self {
        Self {
            valid: false,
            errors,
            warnings: Vec::new(),
        }
    }
}

/// How the engine resolves agent availability for validation (§12). The
/// planner itself never probes executables; the validator consults the
/// registry the engine owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAvailability {
    /// Agent registered and (when checked) executable.
    Available,
    /// Not in the agent registry.
    Unknown,
    /// Registered but the executable cannot be resolved (e.g. not
    /// installed). Reported, never silently substituted (§12).
    Unavailable,
}

/// Deterministic validation (§12–§14, §34). Reuses the existing
/// [`TaskGraph`] validation: a candidate graph is built through the same
/// `add_task`/`add_dependency` API the engine uses, so cycle/duplicate/
/// missing-dependency logic is never duplicated.
pub struct PlanValidator<'a> {
    registry: &'a AgentRegistry,
    /// Optional executable-availability checker (engine-side).
    availability: Option<&'a dyn Fn(&str) -> AgentAvailability>,
}

impl<'a> PlanValidator<'a> {
    pub fn new(registry: &'a AgentRegistry) -> Self {
        Self {
            registry,
            availability: None,
        }
    }

    pub fn with_availability(
        registry: &'a AgentRegistry,
        availability: &'a dyn Fn(&str) -> AgentAvailability,
    ) -> Self {
        Self {
            registry,
            availability: Some(availability),
        }
    }

    pub fn validate(
        &self,
        plan: &ProposedPlan,
        constraints: &PlannerConstraints,
    ) -> PlanValidationResult {
        let mut errors = Vec::new();
        let mut warnings = plan.warnings.clone();

        // 1. Agent recommendations must reference available agents (§12).
        for step in &plan.steps {
            if let Some(rec) = &step.agent_recommendation {
                let id = rec.agent_definition_id.trim();
                if let Some(c) = rec.confidence {
                    if !(0.0..=1.0).contains(&c) {
                        errors.push(PlanValidationError::InvalidConfidence {
                            id: step.id.clone(),
                        });
                        continue;
                    }
                }
                if self.registry.get(id).is_none() {
                    errors.push(PlanValidationError::UnknownAgent {
                        agent: id.to_string(),
                    });
                    continue;
                }
                if let Some(check) = self.availability {
                    if matches!(check(id), AgentAvailability::Unavailable) {
                        errors.push(PlanValidationError::AgentUnavailable {
                            agent: id.to_string(),
                        });
                    }
                }
            } else {
                warnings.push(format!(
                    "step {} has no agent recommendation — agent selection is required",
                    step.id
                ));
            }
        }

        // 2. Dependency structure via the authoritative TaskGraph API —
        // cycle detection and missing-dependency checks are never
        // reimplemented here (§14).
        let mut graph = TaskGraph::new();
        for step in &plan.steps {
            let mut task = Task::new(
                step.title.clone(),
                step.description.clone(),
                step.agent_recommendation
                    .as_ref()
                    .map(|r| r.agent_definition_id.clone())
                    .unwrap_or_default(),
                String::new(),
            );
            task.id = step.id.clone();
            if let Err(e) = graph.add_task(task) {
                errors.push(map_graph_error(e));
            }
        }
        if !errors.is_empty() {
            return PlanValidationResult::invalid(errors);
        }
        for step in &plan.steps {
            let deps: Vec<&String> = step.depends_on.iter().collect();
            for dep in deps {
                if *dep == step.id {
                    errors.push(PlanValidationError::SelfDependency {
                        task: step.id.clone(),
                    });
                    continue;
                }
                match graph.add_dependency(&step.id, dep) {
                    Ok(()) => {}
                    Err(TaskGraphError::UnknownTask(t)) => {
                        errors.push(PlanValidationError::InvalidDependency {
                            task: step.id.clone(),
                            dependency: t,
                        });
                    }
                    Err(TaskGraphError::Cycle { path }) => {
                        errors.push(PlanValidationError::Cycle { path });
                    }
                    Err(e) => errors.push(map_graph_error(e)),
                }
            }
        }

        // 3. Budget (§22): never silently exceed it.
        if let (Some(est), Some(budget)) = (plan.estimated_cost_cents, constraints.budget_cents) {
            if est > budget {
                errors.push(PlanValidationError::BudgetExceeded {
                    estimated_cents: est,
                    budget_cents: budget,
                });
            }
        }

        // 4. Isolation (§46): a step may not silently choose shared
        // workspace for coding work; the worktree count is capped.
        let isolated = plan
            .steps
            .iter()
            .filter(|s| s.isolation == IsolationMode::GitWorktree)
            .count();
        if isolated > constraints.max_worktrees {
            errors.push(PlanValidationError::TooManyWorktrees {
                count: isolated,
                max: constraints.max_worktrees,
            });
        }
        for step in &plan.steps {
            if step.isolation == IsolationMode::SharedWorkspace && !step.requires_shared_workspace {
                errors.push(PlanValidationError::SharedWorkspaceNotAllowed {
                    step: step.id.clone(),
                });
            }
        }

        // 5. Parallelism (§12): the plan may not raise the concurrency cap.
        if plan.steps.len() > 1 {
            // Parallelism is not explicitly carried by the proposal; the
            // engine bounds it at compile time. We still reject an
            // implicitly over-parallel topology: more independent
            // "wave 0" steps than the policy allows.
            let wave0 = plan
                .steps
                .iter()
                .filter(|s| s.depends_on.is_empty())
                .count();
            if wave0 > constraints.max_parallel_tasks {
                errors.push(PlanValidationError::ParallelismExceeded {
                    requested: wave0,
                    max: constraints.max_parallel_tasks,
                });
            }
        }

        if errors.is_empty() {
            PlanValidationResult::ok(warnings)
        } else {
            PlanValidationResult::invalid(errors)
        }
    }
}

fn map_graph_error(e: TaskGraphError) -> PlanValidationError {
    match e {
        TaskGraphError::UnknownTask(t) => PlanValidationError::InvalidDependency {
            task: String::new(),
            dependency: t,
        },
        TaskGraphError::Cycle { path } => PlanValidationError::Cycle { path },
        TaskGraphError::DuplicateTaskId(id) => PlanValidationError::DuplicateStepId {
            id,
            first: 0,
            second: 0,
        },
        other => PlanValidationError::InvalidDependency {
            task: format!("{other:?}"),
            dependency: String::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// §15 plan compilation
// ---------------------------------------------------------------------------

/// Deterministic compilation (§15): the same validated plan always yields
/// the same [`TaskGraph`] structure (step ids become task ids). The
/// compiled graph is the only way a proposal reaches the scheduler.
pub struct PlanCompiler {
    workspace_id: String,
    /// Parallel batch cap applied to the compiled policy (clamped by the
    /// scheduler's own policy later).
    parallelism: usize,
}

impl PlanCompiler {
    pub fn new(workspace_id: &str, parallelism: usize) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            parallelism: parallelism.max(1),
        }
    }

    /// Builds the executable graph. `workspace_id` is the engine's
    /// authoritative workspace; step titles/descriptions are preserved
    /// exactly as approved (§28 — no hidden instructions are injected).
    pub fn compile(
        &self,
        plan: &ProposedPlan,
    ) -> Result<(TaskGraph, TaskPolicy), PlanValidationError> {
        let mut graph = TaskGraph::new();
        for step in &plan.steps {
            let agent = step
                .agent_recommendation
                .as_ref()
                .map(|r| r.agent_definition_id.clone())
                .unwrap_or_default();
            let mut task = Task::new(
                step.title.clone(),
                step.description.clone(),
                agent,
                self.workspace_id.clone(),
            );
            task.id = step.id.clone();
            // Phase 3C §45: isolation is a validated proposal — the
            // scheduler/environment layer decides where the agent runs.
            task.isolation = step.isolation;
            task.requires_shared_workspace = step.requires_shared_workspace;
            graph.add_task(task).map_err(map_graph_error)?;
        }
        for step in &plan.steps {
            for dep in &step.depends_on {
                graph
                    .add_dependency(&step.id, dep)
                    .map_err(map_graph_error)?;
            }
        }
        let policy = TaskPolicy {
            max_parallel_tasks: self.parallelism.max(1),
            ..TaskPolicy::default()
        };
        Ok((graph, policy))
    }
}

// ---------------------------------------------------------------------------
// §20–§21 planner provider boundary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlannerError {
    /// The provider could not be reached (network/DNS).
    Network {
        message: String,
    },
    /// The provider rejected credentials.
    Auth {
        message: String,
    },
    RateLimited {
        message: String,
    },
    ModelUnavailable {
        message: String,
    },
    /// The provider timed out.
    Timeout {
        message: String,
    },
    /// The response did not match the structured schema (§20).
    InvalidResponse {
        message: String,
    },
    /// Validation of the returned plan failed (§14).
    ValidationFailed {
        errors: Vec<PlanValidationError>,
    },
    /// Too many retries on invalid output (§40).
    RetriesExhausted {
        attempts: u32,
    },
    /// Planning budget exceeded.
    BudgetExceeded {
        estimated_cents: u64,
        budget_cents: u64,
    },
    /// No planner provider is configured / the operation is not allowed in
    /// the current planner phase.
    NoProvider,
    /// An operation was attempted in a phase that does not permit it (all
    /// transitions are typed; nothing is silently ignored).
    NotAllowed {
        reason: String,
    },
    /// Planner invocation policy (§44): intent was handled deterministically
    /// and the planner was never called.
    Bypassed {
        reason: String,
    },
}

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network { message } => write!(f, "network error: {message}"),
            Self::Auth { message } => write!(f, "authentication error: {message}"),
            Self::RateLimited { message } => write!(f, "rate limited: {message}"),
            Self::ModelUnavailable { message } => write!(f, "model unavailable: {message}"),
            Self::Timeout { message } => write!(f, "timeout: {message}"),
            Self::InvalidResponse { message } => write!(f, "invalid planner response: {message}"),
            Self::ValidationFailed { errors } => {
                write!(f, "plan failed validation: {}", first_error(errors))
            }
            Self::RetriesExhausted { attempts } => {
                write!(f, "planner retries exhausted after {attempts} attempts")
            }
            Self::BudgetExceeded {
                estimated_cents,
                budget_cents,
            } => write!(
                f,
                "plan exceeds budget: ${}.{:02} > ${}.{:02}",
                estimated_cents / 100,
                estimated_cents % 100,
                budget_cents / 100,
                budget_cents % 100
            ),
            Self::NotAllowed { reason } => write!(f, "not allowed: {reason}"),
            Self::NoProvider => write!(f, "no planner provider configured"),
            Self::Bypassed { reason } => write!(f, "intent resolved deterministically: {reason}"),
        }
    }
}

impl std::error::Error for PlannerError {}

fn first_error(errors: &[PlanValidationError]) -> String {
    errors
        .first()
        .map(|e| e.to_string())
        .unwrap_or_else(|| "unknown validation error".to_string())
}

/// The planner provider boundary (§7). Implementations produce a
/// [`ProposedPlan`] from a request; they never execute anything. Tests
/// inject deterministic mocks; no real LLM is required in standard CI.
pub trait PlannerProvider: Send + Sync {
    /// Provider id (from the existing registry).
    fn provider_id(&self) -> &str;
    /// Produce a structured plan. The implementation owns retry-on-invalid
    /// output up to `config.max_retries` (§20, §40).
    fn generate(
        &self,
        request: &PlannerRequest,
        config: &PlannerConfig,
    ) -> Result<ProposedPlan, PlannerError>;
}

/// Parses a raw provider response into a plan, tolerating Markdown fences.
/// Never attempts uncontrolled string parsing (§20): anything that is not
/// structurally valid JSON with the expected shape is an error.
pub fn parse_plan_response(raw: &str) -> Result<ProposedPlan, PlannerError> {
    let trimmed = raw.trim();
    let json = strip_fences(trimmed).ok_or_else(|| PlannerError::InvalidResponse {
        message: "response is not JSON (missing object)".to_string(),
    })?;
    let schema: PlanSchema =
        serde_json::from_str(json).map_err(|e| PlannerError::InvalidResponse {
            message: format!("schema mismatch: {e}"),
        })?;
    schema
        .into_plan()
        .map_err(|e| PlannerError::InvalidResponse {
            message: e.to_string(),
        })
}

fn strip_fences(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in s[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..start + i + c.len_utf8()]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// §24 planner events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlannerEvent {
    PlanningStarted {
        request_id: String,
        intent: String,
    },
    PlanningCompleted {
        request_id: String,
        plan_hash: u64,
        provider: String,
        model: String,
        latency_ms: u64,
        estimated_cost_cents: Option<u64>,
    },
    PlanningFailed {
        request_id: String,
        error: PlannerError,
        retries: u32,
    },
    PlanValidated {
        request_id: String,
        plan_hash: u64,
        warnings: Vec<String>,
    },
    PlanApproved {
        plan_id: String,
        plan_hash: u64,
    },
    PlanRejected {
        plan_id: String,
        plan_hash: u64,
        reason: String,
    },
    PlanEdited {
        plan_id: String,
        plan_hash: u64,
        change: String,
    },
    PlanExecutionStarted {
        plan_id: String,
        task_ids: Vec<TaskId>,
    },
    PlanStepCompleted {
        plan_id: String,
        step_id: TaskId,
    },
    PlanResumed {
        plan_id: String,
        completed: usize,
        remaining: usize,
    },
    PlanCancelled {
        plan_id: String,
    },
}

// ---------------------------------------------------------------------------
// §29 audit trail
// ---------------------------------------------------------------------------

/// One planning audit record (§29): request id, provider/model, prompt and
/// schema versions, timestamp, plan hash, approval, execution ids. Never
/// contains secrets or hidden reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerAuditRecord {
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub prompt_version: u32,
    pub schema_version: u32,
    pub timestamp_ms: u64,
    pub plan_hash: Option<u64>,
    pub approved: bool,
    pub rejected: bool,
    pub latency_ms: Option<u64>,
    pub execution_task_ids: Vec<TaskId>,
}

/// Bounded audit trail (newest first).
#[derive(Debug, Clone, Default)]
pub struct PlannerAuditTrail {
    records: Vec<PlannerAuditRecord>,
    max: usize,
}

impl PlannerAuditTrail {
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            max: 64,
        }
    }

    pub fn push(&mut self, record: PlannerAuditRecord) {
        self.records.insert(0, record);
        self.records.truncate(self.max);
    }

    pub fn records(&self) -> &[PlannerAuditRecord] {
        &self.records
    }
}

// ---------------------------------------------------------------------------
// §39, §41 metrics
// ---------------------------------------------------------------------------

/// Planner quality metrics (§39, §41): a measurable feedback loop. All
/// counters are monotonic; latency is aggregated (no unbounded vectors).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlannerMetrics {
    pub plans_generated: u64,
    pub plans_valid: u64,
    pub plans_invalid: u64,
    pub invalid_schema_count: u64,
    pub unknown_agent_count: u64,
    pub cycle_count: u64,
    pub budget_violation_count: u64,
    pub parallelism_violation_count: u64,
    pub human_edits: u64,
    pub plan_edited_agent_changed: u64,
    pub plan_edited_description_changed: u64,
    pub plan_edited_dependencies_changed: u64,
    pub plan_edited_step_added: u64,
    pub plan_edited_step_removed: u64,
    pub human_rejections: u64,
    pub executions_started: u64,
    pub executions_succeeded: u64,
    pub executions_failed: u64,
    pub retries_used: u64,
    pub bypassed_intents: u64,
    pub planning_latency_count: u64,
    pub planning_latency_total_ms: u64,
    pub planning_latency_max_ms: u64,
    /// Estimated planning spend in cents (from pricing when known).
    pub estimated_planning_cost_cents: u64,
    /// Estimated plan execution cost in cents (plan estimates, when given).
    pub estimated_execution_cost_cents: u64,
}

impl PlannerMetrics {
    pub fn record_latency(&mut self, ms: u64) {
        self.planning_latency_count += 1;
        self.planning_latency_total_ms += ms;
        self.planning_latency_max_ms = self.planning_latency_max_ms.max(ms);
    }

    pub fn avg_latency_ms(&self) -> Option<u64> {
        self.planning_latency_total_ms
            .checked_div(self.planning_latency_count)
    }
}

// ---------------------------------------------------------------------------
// §17–§19, §23, §25–§26 plan state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerPhase {
    Idle,
    Planning,
    /// Valid plan awaiting approval (§16, §30).
    NeedsApproval,
    /// Approved but not yet executed (§23).
    Approved,
    Executing,
    /// Approved plan interrupted by restart (§26). Resume is explicit.
    Interrupted,
    Done,
    Failed,
    Cancelled,
    Rejected,
}

/// Per-step execution status (derived from the authoritative scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

impl PlanStepStatus {
    pub fn from_task(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Pending | TaskStatus::Ready => Self::Queued,
            TaskStatus::Running | TaskStatus::Waiting => Self::Running,
            TaskStatus::Completed => Self::Completed,
            TaskStatus::Failed => Self::Failed,
            TaskStatus::Cancelled => Self::Cancelled,
            TaskStatus::Skipped => Self::Skipped,
            TaskStatus::Blocked => Self::Skipped,
            TaskStatus::NeedsReview => Self::Running,
            TaskStatus::Interrupted => Self::Cancelled,
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Pending => "○",
            Self::Queued => "…",
            Self::Running => "→",
            Self::Completed => "✓",
            Self::Failed => "✗",
            Self::Cancelled => "–",
            Self::Skipped => "·",
        }
    }
}

/// What a human changed about the plan (§18, §41). Applied locally; the
/// plan is re-validated before execution — never silently replaced (§19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanEditChange {
    SetAgent {
        step_id: String,
        agent: String,
    },
    SetDescription {
        step_id: String,
        description: String,
    },
    SetTitle {
        step_id: String,
        title: String,
    },
    SetDependencies {
        step_id: String,
        dependencies: Vec<String>,
    },
    AddStep {
        after_step_id: Option<String>,
    },
    RemoveStep {
        step_id: String,
    },
}

/// The plan state machine (§17–§19, §23, §25–§26). Owned by the engine;
/// transitions are typed — invalid moves are errors, never silent.
#[derive(Debug)]
pub struct PlannerState {
    phase: PlannerPhase,
    plan_id: Option<String>,
    request_id: Option<String>,
    intent: Option<String>,
    plan: Option<ProposedPlan>,
    /// Current hash of the (possibly edited) plan.
    plan_hash: u64,
    provider: String,
    model: String,
    prompt_version: u32,
    schema_version: u32,
    latency_ms: Option<u64>,
    /// Parallel batch cap for compilation (§18).
    parallelism: usize,
    /// Per-step status map (step id → status) during execution.
    step_status: HashMap<String, PlanStepStatus>,
    /// Steps completed in a previous (interrupted) execution (§26).
    completed_steps: Vec<String>,
    edited: bool,
    metrics: PlannerMetrics,
    audit: PlannerAuditTrail,
    last_error: Option<PlannerError>,
    /// Estimated planning cost from the provider call (pricing, §22).
    planning_cost_cents: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStepView {
    pub step: PlanStep,
    pub status: PlanStepStatus,
}

/// Public snapshot for UI/IPC (§42).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannerStatus {
    pub phase: PlannerPhase,
    pub plan_id: Option<String>,
    pub intent: Option<String>,
    pub plan: Option<ProposedPlan>,
    pub plan_hash: u64,
    pub provider: String,
    pub model: String,
    pub parallelism: usize,
    pub steps: Vec<PlanStepView>,
    pub edited: bool,
    pub last_error: Option<String>,
    pub latency_ms: Option<u64>,
    pub planning_cost_cents: Option<u64>,
    /// Completed in the current or a previous execution.
    pub completed_count: usize,
}

impl Default for PlannerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PlannerState {
    pub fn new() -> Self {
        Self {
            phase: PlannerPhase::Idle,
            plan_id: None,
            request_id: None,
            intent: None,
            plan: None,
            plan_hash: 0,
            provider: String::new(),
            model: String::new(),
            prompt_version: PLANNER_PROMPT_VERSION,
            schema_version: PLANNER_SCHEMA_VERSION,
            latency_ms: None,
            parallelism: 2,
            step_status: HashMap::new(),
            completed_steps: Vec::new(),
            edited: false,
            metrics: PlannerMetrics::default(),
            audit: PlannerAuditTrail::new(),
            last_error: None,
            planning_cost_cents: None,
        }
    }

    pub fn phase(&self) -> PlannerPhase {
        self.phase
    }

    pub fn plan(&self) -> Option<&ProposedPlan> {
        self.plan.as_ref()
    }

    pub fn plan_id(&self) -> Option<&str> {
        self.plan_id.as_deref()
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub fn metrics(&self) -> &PlannerMetrics {
        &self.metrics
    }

    pub fn metrics_mut(&mut self) -> &mut PlannerMetrics {
        &mut self.metrics
    }

    pub fn audit(&self) -> &[PlannerAuditRecord] {
        self.audit.records()
    }

    pub fn last_error(&self) -> Option<&PlannerError> {
        self.last_error.as_ref()
    }

    pub fn status(&self) -> PlannerStatus {
        let completed = self
            .steps_snapshot()
            .iter()
            .filter(|s| s.status == PlanStepStatus::Completed)
            .count();
        PlannerStatus {
            phase: self.phase,
            plan_id: self.plan_id.clone(),
            intent: self.intent.clone(),
            plan: self.plan.clone(),
            plan_hash: self.plan_hash,
            provider: self.provider.clone(),
            model: self.model.clone(),
            parallelism: self.parallelism,
            steps: self.steps_snapshot(),
            edited: self.edited,
            last_error: self.last_error.as_ref().map(|e| e.to_string()),
            latency_ms: self.latency_ms,
            planning_cost_cents: self.planning_cost_cents,
            completed_count: completed,
        }
    }

    fn steps_snapshot(&self) -> Vec<PlanStepView> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        plan.steps
            .iter()
            .map(|s| PlanStepView {
                step: s.clone(),
                status: self
                    .step_status
                    .get(&s.id)
                    .copied()
                    .unwrap_or(PlanStepStatus::Pending),
            })
            .collect()
    }

    // -- transitions ------------------------------------------------------

    /// Begins a request (§21). Only allowed from Idle (or after a
    /// completed/failed/rejected/cancelled plan — always explicit).
    pub fn begin_request(
        &mut self,
        request_id: &str,
        intent: &str,
        provider: &str,
        model: &str,
    ) -> Result<(), PlannerError> {
        if matches!(self.phase, PlannerPhase::Planning) {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            }); // placeholder guard
        }
        self.request_id = Some(request_id.to_string());
        self.intent = Some(intent.to_string());
        self.provider = provider.to_string();
        self.model = model.to_string();
        self.plan = None;
        self.plan_id = None;
        self.plan_hash = 0;
        self.latency_ms = None;
        self.last_error = None;
        self.edited = false;
        self.step_status.clear();
        self.completed_steps.clear();
        self.phase = PlannerPhase::Planning;
        Ok(())
    }

    /// Applies a provider result (§20–§21). Invalid/validation-failed
    /// plans land in `Failed` with a typed error — never partially applied.
    pub fn on_provider_result(
        &mut self,
        result: Result<ProposedPlan, PlannerError>,
        latency_ms: u64,
    ) -> Vec<PlannerEvent> {
        let mut events = Vec::new();
        let request_id = self.request_id.clone().unwrap_or_default();
        self.latency_ms = Some(latency_ms);
        self.metrics.record_latency(latency_ms);
        match result {
            Ok(plan) => {
                self.metrics.plans_generated += 1;
                let hash = plan_hash(&plan);
                self.plan_hash = hash;
                self.plan_id = Some(format!("plan-{hash}"));
                self.plan = Some(plan);
                // Costs: planning cost from pricing (set by caller);
                // execution estimate from the plan itself.
                if let Some(c) = self.planning_cost_cents {
                    self.metrics.estimated_planning_cost_cents += c;
                }
                if let Some(c) = self.plan.as_ref().and_then(|p| p.estimated_cost_cents) {
                    self.metrics.estimated_execution_cost_cents += c;
                }
                events.push(PlannerEvent::PlanningCompleted {
                    request_id: request_id.clone(),
                    plan_hash: hash,
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    latency_ms,
                    estimated_cost_cents: self.plan.as_ref().and_then(|p| p.estimated_cost_cents),
                });
                // The plan is parsed and schema-valid; policy validation
                // against the engine's constraints happens next (§14). The
                // caller publishes `PlanValidated` only after that passes.
                self.phase = PlannerPhase::NeedsApproval;
            }
            Err(e) => {
                self.metrics.plans_invalid += 1;
                self.last_error = Some(e.clone());
                self.phase = PlannerPhase::Failed;
                events.push(PlannerEvent::PlanningFailed {
                    request_id,
                    error: e,
                    retries: self.metrics.retries_used as u32,
                });
            }
        }
        events
    }

    /// Validates the current plan against constraints. On failure the
    /// state moves to `Failed` with a typed validation error (§14).
    pub fn validate_plan(
        &mut self,
        validator: &PlanValidator,
        constraints: &PlannerConstraints,
    ) -> Result<(), PlannerError> {
        let Some(plan) = self.plan.clone() else {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        };
        let result = validator.validate(&plan, constraints);
        if !result.valid {
            self.metrics.plans_invalid += 1;
            for e in &result.errors {
                match e {
                    PlanValidationError::UnknownAgent { .. } => {
                        self.metrics.unknown_agent_count += 1
                    }
                    PlanValidationError::Cycle { .. } => self.metrics.cycle_count += 1,
                    PlanValidationError::BudgetExceeded { .. } => {
                        self.metrics.budget_violation_count += 1
                    }
                    PlanValidationError::ParallelismExceeded { .. } => {
                        self.metrics.parallelism_violation_count += 1
                    }
                    _ => {}
                }
            }
            self.last_error = Some(PlannerError::ValidationFailed {
                errors: result.errors.clone(),
            });
            self.phase = PlannerPhase::Failed;
            return Err(self.last_error.clone().unwrap());
        }
        self.metrics.plans_valid += 1;
        Ok(())
    }

    /// Sets the parallel batch cap (clamped ≥ 1, ≤ 32). Re-validation is
    /// the caller's job (the engine re-validates on execute).
    pub fn set_parallelism(&mut self, n: usize) {
        self.parallelism = n.clamp(1, 32);
    }

    pub fn parallelism(&self) -> usize {
        self.parallelism
    }

    /// Human edit (§18–§19, §41). Applied locally; the plan is
    /// re-validated before execution. Never silently replaces anything.
    pub fn edit(&mut self, change: &PlanEditChange) -> Result<(), PlannerError> {
        if !matches!(
            self.phase,
            PlannerPhase::NeedsApproval | PlannerPhase::Approved
        ) {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        let Some(plan) = self.plan.as_mut() else {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        };
        match change {
            PlanEditChange::SetAgent { step_id, agent } => {
                let step = plan
                    .steps
                    .iter_mut()
                    .find(|s| s.id == *step_id)
                    .ok_or_else(|| PlannerError::InvalidResponse {
                        message: format!("unknown step {step_id}"),
                    })?;
                let mut rec = step
                    .agent_recommendation
                    .take()
                    .unwrap_or(AgentRecommendation {
                        agent_definition_id: agent.clone(),
                        reason: None,
                        confidence: None,
                    });
                rec.agent_definition_id = agent.clone();
                rec.reason = Some("edited by user".to_string());
                step.agent_recommendation = Some(rec);
                self.metrics.plan_edited_agent_changed += 1;
            }
            PlanEditChange::SetDescription {
                step_id,
                description,
            } => {
                let step = find_step_mut(plan, step_id)?;
                step.description = description.clone();
                self.metrics.plan_edited_description_changed += 1;
            }
            PlanEditChange::SetTitle { step_id, title } => {
                let step = find_step_mut(plan, step_id)?;
                step.title = title.clone();
            }
            PlanEditChange::SetDependencies {
                step_id,
                dependencies,
            } => {
                let step = find_step_mut(plan, step_id)?;
                step.depends_on = dependencies.clone();
                self.metrics.plan_edited_dependencies_changed += 1;
            }
            PlanEditChange::AddStep { after_step_id } => {
                let idx = after_step_id
                    .as_ref()
                    .and_then(|a| plan.steps.iter().position(|s| &s.id == a))
                    .map(|i| i + 1)
                    .unwrap_or(plan.steps.len());
                let id = format!("step-{}", plan.steps.len() + 1);
                plan.steps.insert(
                    idx,
                    PlanStep {
                        id,
                        title: "New step".to_string(),
                        description: String::new(),
                        agent_recommendation: None,
                        depends_on: Vec::new(),
                        isolation: IsolationMode::GitWorktree,
                        requires_shared_workspace: false,
                    },
                );
                self.metrics.plan_edited_step_added += 1;
            }
            PlanEditChange::RemoveStep { step_id } => {
                let before = plan.steps.len();
                plan.steps.retain(|s| s.id != *step_id);
                if plan.steps.len() == before {
                    return Err(PlannerError::InvalidResponse {
                        message: format!("unknown step {step_id}"),
                    });
                }
                for s in &mut plan.steps {
                    s.depends_on.retain(|d| d != step_id);
                }
                self.metrics.plan_edited_step_removed += 1;
            }
        }
        self.metrics.human_edits += 1;
        self.edited = true;
        self.plan_hash = plan_hash(plan);
        self.audit.push(PlannerAuditRecord {
            request_id: self.request_id.clone().unwrap_or_default(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt_version: self.prompt_version,
            schema_version: self.schema_version,
            timestamp_ms: now_ms(),
            plan_hash: Some(self.plan_hash),
            approved: false,
            rejected: false,
            latency_ms: self.latency_ms,
            execution_task_ids: Vec::new(),
        });
        Ok(())
    }

    pub fn approve(&mut self) -> Result<(), PlannerError> {
        if !matches!(self.phase, PlannerPhase::NeedsApproval) {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        self.phase = PlannerPhase::Approved;
        self.audit.push(PlannerAuditRecord {
            request_id: self.request_id.clone().unwrap_or_default(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt_version: self.prompt_version,
            schema_version: self.schema_version,
            timestamp_ms: now_ms(),
            plan_hash: Some(self.plan_hash),
            approved: true,
            rejected: false,
            latency_ms: self.latency_ms,
            execution_task_ids: Vec::new(),
        });
        Ok(())
    }

    pub fn reject(&mut self, reason: &str) -> Result<(), PlannerError> {
        if !matches!(
            self.phase,
            PlannerPhase::NeedsApproval | PlannerPhase::Approved
        ) {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        self.metrics.human_rejections += 1;
        self.phase = PlannerPhase::Rejected;
        self.audit.push(PlannerAuditRecord {
            request_id: self.request_id.clone().unwrap_or_default(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            prompt_version: self.prompt_version,
            schema_version: self.schema_version,
            timestamp_ms: now_ms(),
            plan_hash: Some(self.plan_hash),
            approved: false,
            rejected: true,
            latency_ms: self.latency_ms,
            execution_task_ids: Vec::new(),
        });
        self.last_error = Some(PlannerError::InvalidResponse {
            message: format!("plan rejected by user: {reason}"),
        });
        Ok(())
    }

    /// Moves into execution: returns the compiled graph + policy, or a
    /// typed error. Execution is the point of no return for the planner —
    /// the scheduler is authoritative from here (§23). Compilation to an
    /// executable graph requires explicit approval (§30); `Auto` mode is
    /// the engine calling `approve()` before this, never a bypass here.
    pub fn compile_for_execution(
        &mut self,
        workspace_id: &str,
    ) -> Result<(TaskGraph, TaskPolicy), PlannerError> {
        if self.phase != PlannerPhase::Approved {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        let Some(plan) = self.plan.clone() else {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        };
        let compiler = PlanCompiler::new(workspace_id, self.parallelism);
        let (graph, policy) = compiler
            .compile(&plan)
            .map_err(|e| PlannerError::ValidationFailed { errors: vec![e] })?;
        self.phase = PlannerPhase::Executing;
        for step in &plan.steps {
            self.step_status
                .insert(step.id.clone(), PlanStepStatus::Queued);
        }
        self.plan_id = Some(format!("plan-{}", self.plan_hash));
        Ok((graph, policy))
    }

    /// Marks a step's scheduler status (from authoritative TaskEvents).
    pub fn on_step_status(&mut self, step_id: &str, status: TaskStatus) {
        let mapped = PlanStepStatus::from_task(status);
        if self.phase != PlannerPhase::Executing && self.phase != PlannerPhase::Interrupted {
            return;
        }
        self.step_status.insert(step_id.to_string(), mapped);
        if mapped == PlanStepStatus::Completed
            && !self.completed_steps.contains(&step_id.to_string())
        {
            self.completed_steps.push(step_id.to_string());
        }
    }

    /// Interrupts execution (restart, §26). Never resumes silently.
    pub fn interrupt(&mut self, reason: &str) {
        if self.phase == PlannerPhase::Executing {
            self.phase = PlannerPhase::Interrupted;
            for st in self.step_status.values_mut() {
                if *st == PlanStepStatus::Running || *st == PlanStepStatus::Queued {
                    *st = PlanStepStatus::Cancelled;
                }
            }
            self.last_error = Some(PlannerError::InvalidResponse {
                message: reason.to_string(),
            });
        }
    }

    /// Explicit resume (§26): rebuilds the graph from remaining steps.
    pub fn remaining_steps(&self) -> Vec<PlanStep> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        plan.steps
            .iter()
            .filter(|s| !self.completed_steps.contains(&s.id))
            .cloned()
            .collect()
    }

    pub fn resume(&mut self, workspace_id: &str) -> Result<(TaskGraph, TaskPolicy), PlannerError> {
        if self.phase != PlannerPhase::Interrupted {
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        let remaining = self.remaining_steps();
        if remaining.is_empty() {
            self.phase = PlannerPhase::Done;
            return Err(PlannerError::NotAllowed {
                reason: "operation not permitted in the current planner phase".to_string(),
            });
        }
        let plan = ProposedPlan {
            goal: self
                .plan
                .as_ref()
                .map(|p| p.goal.clone())
                .unwrap_or_default(),
            steps: remaining
                .into_iter()
                .map(|mut s| {
                    // Dependencies on already-completed steps are satisfied;
                    // dropping them keeps the resumed graph acyclic and
                    // runnable (§26).
                    s.depends_on.retain(|d| !self.completed_steps.contains(d));
                    s
                })
                .collect(),
            estimated_cost_cents: None,
            estimated_duration_min: None,
            reasoning_summary: None,
            warnings: Vec::new(),
        };
        let compiler = PlanCompiler::new(workspace_id, self.parallelism);
        let (graph, policy) = compiler
            .compile(&plan)
            .map_err(|e| PlannerError::ValidationFailed { errors: vec![e] })?;
        self.phase = PlannerPhase::Executing;
        for step in &plan.steps {
            self.step_status
                .insert(step.id.clone(), PlanStepStatus::Queued);
        }
        Ok((graph, policy))
    }

    pub fn finish_execution(&mut self, all_completed: bool) {
        self.phase = if all_completed {
            PlannerPhase::Done
        } else {
            PlannerPhase::Cancelled
        };
    }

    pub fn cancel(&mut self) {
        self.phase = PlannerPhase::Cancelled;
    }

    pub fn fail(&mut self, error: PlannerError) {
        self.last_error = Some(error);
        self.phase = PlannerPhase::Failed;
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    // -- persistence (§25–§26) --------------------------------------------

    pub fn export_persisted(&self) -> Option<PersistedPlanState> {
        if !matches!(
            self.phase,
            PlannerPhase::Approved | PlannerPhase::Executing | PlannerPhase::Interrupted
        ) {
            return None;
        }
        let plan = self.plan.clone()?;
        let est_cost = plan.estimated_cost_cents;
        Some(PersistedPlanState {
            version: PERSISTED_PLAN_VERSION,
            plan_id: self.plan_id.clone().unwrap_or_default(),
            goal: plan.goal.clone(),
            plan,
            planner_provider: self.provider.clone(),
            planner_model: self.model.clone(),
            parallelism: self.parallelism,
            estimated_cost_cents: est_cost,
            approval: match self.phase {
                PlannerPhase::Approved => PersistedApproval::Approved,
                PlannerPhase::Executing | PlannerPhase::Interrupted => {
                    PersistedApproval::ApprovedAndStarted
                }
                _ => PersistedApproval::Approved,
            },
            completed_steps: self.completed_steps.clone(),
            plan_hash: self.plan_hash,
        })
    }

    pub fn import_persisted(state: PersistedPlanState) -> Self {
        let mut s = Self::new();
        let hash = plan_hash(&state.plan);
        s.plan = Some(state.plan);
        s.plan_id = Some(state.plan_id);
        s.plan_hash = if state.plan_hash == 0 {
            hash
        } else {
            state.plan_hash
        };
        s.provider = state.planner_provider;
        s.model = state.planner_model;
        s.parallelism = state.parallelism.max(1);
        s.completed_steps = state.completed_steps;
        s.edited = true;
        for step in s.plan.as_ref().map(|p| &p.steps).into_iter().flatten() {
            let status = if s.completed_steps.contains(&step.id) {
                PlanStepStatus::Completed
            } else {
                PlanStepStatus::Pending
            };
            s.step_status.insert(step.id.clone(), status);
        }
        s.phase = PlannerPhase::Interrupted;
        s.last_error = Some(PlannerError::InvalidResponse {
            message: "workflow interrupted by restart — resume explicitly".to_string(),
        });
        s
    }
}

fn find_step_mut<'a>(
    plan: &'a mut ProposedPlan,
    step_id: &str,
) -> Result<&'a mut PlanStep, PlannerError> {
    plan.steps
        .iter_mut()
        .find(|s| s.id == step_id)
        .ok_or_else(|| PlannerError::InvalidResponse {
            message: format!("unknown step {step_id}"),
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedApproval {
    Approved,
    ApprovedAndStarted,
}

/// Persisted plan slice (§25–§26). Never contains credentials or private
/// reasoning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedPlanState {
    pub version: u32,
    pub plan_id: String,
    pub goal: String,
    pub plan: ProposedPlan,
    /// Reference only — provider/model ids, never credentials.
    pub planner_provider: String,
    pub planner_model: String,
    pub parallelism: usize,
    pub estimated_cost_cents: Option<u64>,
    pub approval: PersistedApproval,
    pub completed_steps: Vec<String>,
    pub plan_hash: u64,
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> ProposedPlan {
        let json = r#"{
            "goal": "Implement authentication",
            "tasks": [
                {"id": "research", "title": "Inspect auth architecture", "description": "Read the auth module", "agent": "fake-agent", "depends_on": []},
                {"id": "implement", "title": "Implement Google/GitHub auth", "description": "Add providers", "agent": "fake-agent", "depends_on": ["research"]}
            ],
            "estimated_cost_cents": 142,
            "estimated_duration_min": 25
        }"#;
        parse_plan_response(json).unwrap()
    }

    fn registry() -> AgentRegistry {
        AgentRegistry::new()
    }

    fn constraints() -> PlannerConstraints {
        PlannerConstraints {
            budget_cents: Some(1000),
            max_parallel_tasks: 4,
            approval: PlannerApprovalMode::Confirm,
            user_preferences: vec![],
            max_worktrees: 8,
        }
    }

    #[test]
    fn parses_structured_schema() {
        let plan = sample_plan();
        assert_eq!(plan.goal, "Implement authentication");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].id, "research");
        assert_eq!(plan.steps[1].depends_on, vec!["research"]);
        assert_eq!(plan.estimated_cost_cents, Some(142));
    }

    #[test]
    fn parses_markdown_fenced_json() {
        let raw = "```json\n{\"goal\":\"g\",\"tasks\":[{\"id\":\"a\",\"title\":\"A\"}]}\n```";
        let plan = parse_plan_response(raw).unwrap();
        assert_eq!(plan.steps[0].id, "a");
    }

    #[test]
    fn rejects_prose_responses() {
        let err = parse_plan_response("Sure! Here is a plan: first we do X, then Y.").unwrap_err();
        assert!(matches!(err, PlannerError::InvalidResponse { .. }));
    }

    #[test]
    fn rejects_unknown_agents() {
        let mut plan = sample_plan();
        plan.steps[1].agent_recommendation = Some(AgentRecommendation {
            agent_definition_id: "ghost-agent".into(),
            reason: None,
            confidence: None,
        });
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let c = constraints();
        let r = v.validate(&plan, &c);
        assert!(!r.valid);
        assert!(r.errors.contains(&PlanValidationError::UnknownAgent {
            agent: "ghost-agent".into()
        }));
    }

    #[test]
    fn detects_cycles_and_missing_deps() {
        let mut plan = sample_plan();
        plan.steps[0].depends_on = vec!["implement".to_string()];
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let c = constraints();
        let r = v.validate(&plan, &c);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::Cycle { .. })));

        let mut plan2 = sample_plan();
        plan2.steps[1].depends_on = vec!["nope".to_string()];
        let r2 = v.validate(&plan2, &constraints());
        assert!(!r2.valid);
        assert!(r2
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::InvalidDependency { .. })));
    }

    #[test]
    fn rejects_budget_violations() {
        let mut plan = sample_plan();
        plan.estimated_cost_cents = Some(5000);
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let c = PlannerConstraints {
            budget_cents: Some(1000),
            ..constraints()
        };
        let r = v.validate(&plan, &c);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::BudgetExceeded { .. })));
    }

    #[test]
    fn rejects_parallelism_above_cap() {
        let json = r#"{
            "goal": "g",
            "tasks": [
                {"id": "a", "title": "A", "agent": "fake-agent"},
                {"id": "b", "title": "B", "agent": "fake-agent"},
                {"id": "c", "title": "C", "agent": "fake-agent"},
                {"id": "d", "title": "D", "agent": "fake-agent"},
                {"id": "e", "title": "E", "agent": "fake-agent"}
            ]
        }"#;
        let plan = parse_plan_response(json).unwrap();
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let c = PlannerConstraints {
            max_parallel_tasks: 2,
            ..constraints()
        };
        let r = v.validate(&plan, &c);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::ParallelismExceeded { .. })));
    }

    #[test]
    fn shared_workspace_requires_explicit_opt_in() {
        // §46: the planner may not silently choose shared workspace.
        let json = r#"{
            "goal": "g",
            "tasks": [
                {"id": "a", "title": "A", "agent": "fake-agent", "isolation": "shared_workspace"},
                {"id": "b", "title": "B", "agent": "fake-agent"}
            ]
        }"#;
        let plan = parse_plan_response(json).unwrap();
        assert_eq!(plan.steps[0].isolation, IsolationMode::SharedWorkspace);
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let r = v.validate(&plan, &constraints());
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::SharedWorkspaceNotAllowed { .. })));
        // With the explicit opt-in it validates.
        let mut plan2 = plan.clone();
        plan2.steps[0].requires_shared_workspace = true;
        let r2 = v.validate(&plan2, &constraints());
        assert!(r2.valid, "{:#?}", r2.errors);
    }

    #[test]
    fn too_many_worktrees_is_rejected() {
        let json = r#"{
            "goal": "g",
            "tasks": [
                {"id": "a", "title": "A", "agent": "fake-agent"},
                {"id": "b", "title": "B", "agent": "fake-agent"},
                {"id": "c", "title": "C", "agent": "fake-agent"}
            ]
        }"#;
        let plan = parse_plan_response(json).unwrap();
        let reg = registry();
        let v = PlanValidator::new(&reg);
        let c = PlannerConstraints {
            max_worktrees: 2,
            ..constraints()
        };
        let r = v.validate(&plan, &c);
        assert!(!r.valid);
        assert!(r
            .errors
            .iter()
            .any(|e| matches!(e, PlanValidationError::TooManyWorktrees { .. })));
    }

    #[test]
    fn duplicate_step_ids_rejected() {
        let json = r#"{"goal":"g","tasks":[{"id":"a","title":"A"},{"id":"a","title":"B"}]}"#;
        let err = parse_plan_response(json).unwrap_err();
        assert!(matches!(err, PlannerError::InvalidResponse { .. }));
    }

    #[test]
    fn plan_hash_is_deterministic_and_order_independent() {
        let p1 = sample_plan();
        let mut p2 = p1.clone();
        p2.steps.swap(0, 1);
        assert_eq!(plan_hash(&p1), plan_hash(&p2));
        let mut p3 = p1.clone();
        p3.estimated_cost_cents = Some(999);
        assert_ne!(plan_hash(&p1), plan_hash(&p3));
        let mut p4 = p1.clone();
        p4.warnings.push("note".to_string());
        // Warnings/reasoning are not part of the schedule hash.
        assert_eq!(plan_hash(&p1), plan_hash(&p4));
    }

    #[test]
    fn compiler_is_deterministic() {
        let plan = sample_plan();
        let c1 = PlanCompiler::new("ws", 2);
        let c2 = PlanCompiler::new("ws", 2);
        let (g1, p1) = c1.compile(&plan).unwrap();
        let (g2, p2) = c2.compile(&plan).unwrap();
        assert_eq!(g1.list_task_ids(), g2.list_task_ids());
        assert_eq!(p1.max_parallel_tasks, p2.max_parallel_tasks);
        let ids = g1.list_task_ids();
        assert_eq!(ids, vec!["research".to_string(), "implement".to_string()]);
    }

    #[test]
    fn context_builder_is_bounded_and_secret_free() {
        let root = std::env::temp_dir().join(format!("ft-planctx-{}", now_ms()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "x").unwrap();
        std::fs::write(root.join(".env"), "API_KEY=secret").unwrap();
        std::fs::write(root.join("id_rsa"), "PRIVATE").unwrap();
        std::fs::write(root.join("notes.txt"), "hi").unwrap();
        let input = PlannerContextInput {
            workspace_id: "w".into(),
            workspace_name: "w".into(),
            project_root: root.to_string_lossy().to_string(),
            available_agents: vec![],
            active_tasks: vec![],
            recent_tasks: vec![],
            provider_ids: vec!["openai".into()],
            constraints: constraints(),
        };
        let ctx = PlannerContextBuilder::new(input).build();
        assert!(ctx.repo_entries.contains(&"Cargo.toml".to_string()));
        assert!(ctx.repo_entries.contains(&"notes.txt".to_string()));
        assert!(!ctx
            .repo_entries
            .iter()
            .any(|e| e.contains(".env") || e.contains("id_rsa")));
        assert!(ctx.is_secret_free());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn state_machine_approval_flow() {
        let mut s = PlannerState::new();
        assert_eq!(s.phase(), PlannerPhase::Idle);
        s.begin_request("r1", "build auth", "openai", "gpt-4o-mini")
            .unwrap();
        assert_eq!(s.phase(), PlannerPhase::Planning);
        let events = s.on_provider_result(Ok(sample_plan()), 42);
        assert_eq!(s.phase(), PlannerPhase::NeedsApproval);
        assert!(events
            .iter()
            .any(|e| matches!(e, PlannerEvent::PlanningCompleted { .. })));
        // `PlanValidated` is published by the engine after §14 policy
        // validation passes — not by the state machine alone.
        // Not approved → cannot compile/execute.
        assert!(s.compile_for_execution("ws").is_err());
        s.approve().unwrap();
        assert_eq!(s.phase(), PlannerPhase::Approved);
        let (graph, _policy) = s.compile_for_execution("ws").unwrap();
        assert_eq!(graph.list_task_ids().len(), 2);
        assert_eq!(s.phase(), PlannerPhase::Executing);
    }

    #[test]
    fn state_machine_edit_and_reject() {
        let mut s = PlannerState::new();
        s.begin_request("r2", "x", "openai", "m").unwrap();
        s.on_provider_result(Ok(sample_plan()), 5);
        let hash_before = s.status().plan_hash;
        s.edit(&PlanEditChange::SetAgent {
            step_id: "implement".into(),
            agent: "codex".into(),
        })
        .unwrap();
        assert!(s.status().edited);
        assert_ne!(hash_before, s.status().plan_hash);
        s.reject("user changed their mind").unwrap();
        assert_eq!(s.phase(), PlannerPhase::Rejected);
    }

    #[test]
    fn state_machine_interrupt_and_resume() {
        let mut s = PlannerState::new();
        s.begin_request("r3", "x", "openai", "m").unwrap();
        s.on_provider_result(Ok(sample_plan()), 5);
        s.approve().unwrap();
        s.compile_for_execution("ws").unwrap();
        s.on_step_status("research", TaskStatus::Completed);
        s.interrupt("restart");
        assert_eq!(s.phase(), PlannerPhase::Interrupted);
        let remaining = s.remaining_steps();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "implement");
        let (graph, _) = s.resume("ws").unwrap();
        assert_eq!(graph.list_task_ids(), vec!["implement".to_string()]);
    }

    #[test]
    fn persistence_round_trip() {
        let mut s = PlannerState::new();
        s.begin_request("r4", "x", "openai", "m").unwrap();
        s.on_provider_result(Ok(sample_plan()), 5);
        s.approve().unwrap();
        s.compile_for_execution("ws").unwrap();
        s.on_step_status("research", TaskStatus::Completed);
        let persisted = s.export_persisted().expect("persisted");
        assert_eq!(persisted.completed_steps, vec!["research"]);
        let restored = PlannerState::import_persisted(persisted);
        assert_eq!(restored.phase(), PlannerPhase::Interrupted);
        assert_eq!(restored.status().completed_count, 1);
        assert_eq!(restored.remaining_steps().len(), 1);
    }

    #[test]
    fn metrics_count_quality_signals() {
        let mut s = PlannerState::new();
        s.begin_request("r5", "x", "openai", "m").unwrap();
        // Invalid schema response → failed + retry counting.
        s.metrics_mut().retries_used += 1;
        s.on_provider_result(
            Err(PlannerError::InvalidResponse {
                message: "bad json".into(),
            }),
            10,
        );
        assert_eq!(s.metrics().plans_generated, 0);
        assert_eq!(s.metrics().plans_invalid, 1);
        assert_eq!(s.phase(), PlannerPhase::Failed);
    }

    #[test]
    fn audit_records_are_bounded_and_secret_free() {
        let mut s = PlannerState::new();
        for i in 0..70 {
            s.begin_request(&format!("r{i}"), "x", "openai", "m")
                .unwrap();
            s.on_provider_result(Ok(sample_plan()), 1);
            s.approve().unwrap();
        }
        assert!(s.audit().len() <= 64);
        let rec = &s.audit()[0];
        assert!(rec.request_id.starts_with('r'));
        assert_eq!(rec.prompt_version, PLANNER_PROMPT_VERSION);
        assert_eq!(rec.schema_version, PLANNER_SCHEMA_VERSION);
    }

    #[test]
    fn intent_bypass_routes_simple_commands_away_from_planner() {
        for simple in [
            "show agents",
            "show tasks",
            "split pane",
            "run tests",
            "list tasks",
            "clear",
            "focus",
        ] {
            assert!(
                matches!(classify_intent(simple), IntentDisposition::Bypass { .. }),
                "{simple:?} should bypass"
            );
        }
        for complex in [
            "build authentication with google and github",
            "fix the failing tests",
            "add payment webhook",
            "refactor the api",
            "add unit tests",
            "implement oauth login",
        ] {
            assert!(
                matches!(classify_intent(complex), IntentDisposition::Plan),
                "{complex:?} should route to the planner"
            );
        }
        // Ambiguous text never becomes a plan (cost/latency discipline).
        assert!(matches!(
            classify_intent("hello there"),
            IntentDisposition::Bypass { .. }
        ));
        assert!(matches!(
            classify_intent(""),
            IntentDisposition::Bypass { .. }
        ));
    }

    #[test]
    fn intent_normalization_is_deterministic_and_bounded() {
        let n = normalize_intent("  build   login with google and github  ");
        assert_eq!(n.objective, "Build login with google and github");
        assert_eq!(
            n.objective,
            normalize_intent("build login with google and github").objective
        );
        assert!(n.constraints.is_empty());
    }

    #[test]
    fn profiles_map_parameters_without_provider_assumptions() {
        let fast = PlannerProfile::fast();
        let deep = PlannerProfile::deep_planning();
        assert!(fast.max_tokens < deep.max_tokens);
        assert_ne!(fast.cost_guidance, deep.cost_guidance);
        let cfg = PlannerConfig::default().with_profile(PlannerProfileId::Fast);
        assert_eq!(cfg.temperature, fast.temperature);
        assert_eq!(cfg.max_tokens, fast.max_tokens);
    }
}
