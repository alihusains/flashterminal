//! Adaptive orchestration (3e.md): controlled replanning.
//!
//! The planner may *propose* a revised plan; it must never mutate or
//! execute the workflow directly (§2). This module owns the deterministic
//! half of that boundary:
//!
//! - `ReplanTrigger` / `ReplanSeverity` / `ReplanSignal` (§3–§5) — a formal,
//!   persisted signal model. Severity never auto-executes anything; it only
//!   drives UI/approval behavior (§5).
//! - `WorkflowEvaluator` (§6–§7) — deterministic rules that turn observed
//!   state (task results, reviews, artifacts, merge conflicts, budget) into
//!   replan signals. No LLM is asked to detect failures.
//! - Signal deduplication (§8) + cooldown (§9) — 100 identical test
//!   failures never become 100 replan requests.
//! - `ReplanContextBuilder` (§10) — a bounded context for the planner.
//! - `ProposedReplan` / `PlanDiff` / `PlanVersion` (§12–§14) — the planner's
//!   output stays a *proposal*; history is immutable (v1 → v2 → v3).
//! - `TaskInvalidation` / `ArtifactInvalidation` (§19–§20) — explicit,
//!   evidence-backed; old artifacts are preserved for lineage.
//! - `ReplanLimits` / loop protection (§31–§32) — a workflow-level cap and
//!   human escalation when automation cannot safely continue (§33).
//! - `AutonomyPolicy` (§34–§37) — Manual/Assisted/Automatic; **Automatic is
//!   disabled in Phase 3E**.
//! - Metrics (§24–§25) — replan + planner-quality metrics, tracked
//!   separately.

use crate::collaboration::{ReviewFinding, Severity};
use crate::orchestration::{TaskId, TaskStatus};
use crate::planning::{now_ms, PlanStep, ProposedPlan};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// §3 replan triggers
// ---------------------------------------------------------------------------

/// Formal replan trigger taxonomy (3e.md §3). Kept deliberately small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanTrigger {
    TaskFailure,
    CriticalReviewFinding,
    RepeatedRetryFailure,
    ArtifactMissing,
    DependencyInvalidated,
    BudgetRisk,
    EnvironmentFailure,
    TestsFailed,
    MergeConflict,
    ManualUserRequest,
}

impl ReplanTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TaskFailure => "task_failure",
            Self::CriticalReviewFinding => "critical_review_finding",
            Self::RepeatedRetryFailure => "repeated_retry_failure",
            Self::ArtifactMissing => "artifact_missing",
            Self::DependencyInvalidated => "dependency_invalidated",
            Self::BudgetRisk => "budget_risk",
            Self::EnvironmentFailure => "environment_failure",
            Self::TestsFailed => "tests_failed",
            Self::MergeConflict => "merge_conflict",
            Self::ManualUserRequest => "manual_user_request",
        }
    }
}

/// Replan severity (3e.md §5). Does not trigger execution — it controls
/// UI/approval behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanSeverity {
    Info,
    Warning,
    Critical,
}

impl ReplanSeverity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

// ---------------------------------------------------------------------------
// §4 replan signal
// ---------------------------------------------------------------------------

/// A formal replanning signal (3e.md §4). Persisted; deduplicated; never
/// auto-executes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanSignal {
    pub id: String,
    pub workflow_id: String,
    pub task_id: Option<TaskId>,
    pub trigger: ReplanTrigger,
    pub severity: ReplanSeverity,
    pub reason: String,
    pub evidence_artifacts: Vec<String>,
    pub created_at: u64,
}

impl ReplanSignal {
    pub fn new(
        workflow_id: impl Into<String>,
        task_id: Option<TaskId>,
        trigger: ReplanTrigger,
        severity: ReplanSeverity,
        reason: impl Into<String>,
        evidence_artifacts: Vec<String>,
    ) -> Self {
        Self {
            id: format!("replan-signal:{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.into(),
            task_id,
            trigger,
            severity,
            reason: reason.into(),
            evidence_artifacts,
            created_at: now_ms(),
        }
    }

    /// §8: coalescing key — workflow, task, trigger, evidence fingerprint.
    /// Equivalent signals (e.g. repeated runs of the same failing task)
    /// share a key and are coalesced instead of spamming the bus.
    pub fn dedupe_key(&self) -> String {
        let task = self.task_id.clone().unwrap_or_default();
        let mut evidence = self.evidence_artifacts.clone();
        evidence.sort();
        let fp = evidence.join(",");
        format!(
            "{}|{}|{}|{}",
            self.workflow_id,
            task,
            self.trigger.as_str(),
            fp
        )
    }
}

// ---------------------------------------------------------------------------
// §8–§9 dedup + cooldown
// ---------------------------------------------------------------------------

/// Coalescing/cooldown state for one workflow (§8–§9). `max_replans`
/// bounds how many replan *proposals* a workflow may go through (§31);
/// `cooldown_seconds` prevents rapid re-signaling of the same failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanLimits {
    pub max_replans: u32,
    pub replan_cooldown_seconds: u64,
}

impl Default for ReplanLimits {
    fn default() -> Self {
        Self {
            max_replans: 5,
            replan_cooldown_seconds: 30,
        }
    }
}

/// Dedup bookkeeping: key → last emission time (cooldown) + the full set of
/// keys ever emitted (so re-signaling after a cooldown still requires new
/// evidence to be worth it — handled by the evaluator).
#[derive(Debug, Clone, Default)]
pub struct SignalRegistry {
    /// dedupe_key → last emitted at (ms).
    last_emitted: std::collections::HashMap<String, u64>,
}

impl SignalRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if `signal` is allowed through (no equivalent signal emitted
    /// within the cooldown window).
    pub fn admit(&mut self, signal: &ReplanSignal, cooldown_secs: u64) -> bool {
        let key = signal.dedupe_key();
        let now = now_ms();
        if let Some(last) = self.last_emitted.get(&key) {
            let elapsed_ms = now.saturating_sub(*last);
            if elapsed_ms < cooldown_secs.saturating_mul(1000) {
                return false;
            }
        }
        self.last_emitted.insert(key, now);
        true
    }
}

// ---------------------------------------------------------------------------
// §6–§7 deterministic workflow evaluation
// ---------------------------------------------------------------------------

/// A minimal, deterministic view of workflow state the evaluator consumes.
/// Built by the engine from authoritative state — never from agent claims.
#[derive(Debug, Clone, Default)]
pub struct WorkflowSnapshot {
    pub workflow_id: String,
    /// Tasks with their current status + failure/retry evidence.
    pub tasks: Vec<TaskObservation>,
    /// Review findings observed so far (consensus per task is derived).
    pub findings: Vec<ReviewFinding>,
    /// Artifact references that are declared but missing (readiness gates).
    pub missing_artifacts: Vec<(TaskId, String)>,
    /// Merge conflicts surfaced by the worktree layer (§22).
    pub merge_conflicts: Vec<MergeConflictEvidence>,
    /// Budget state (§23) — cents.
    pub budget: Option<BudgetObservation>,
    /// Failing-test evidence from agent work records (§7).
    pub test_failures: Vec<(TaskId, u32)>,
    /// Environment failures (spawn errors, adapter failures).
    pub environment_failures: Vec<(TaskId, String)>,
    /// Whether any task exhausted its retry budget.
    pub retries_exhausted: Vec<(TaskId, u32)>,
}

#[derive(Debug, Clone)]
pub struct TaskObservation {
    pub task_id: TaskId,
    pub status: TaskStatus,
    pub failed: bool,
    pub retries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflictEvidence {
    pub task_id: TaskId,
    pub files: Vec<String>,
    pub branches: Vec<String>,
    pub base: Option<String>,
    pub ours: String,
    pub theirs: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetObservation {
    pub spent_cents: u64,
    pub budget_cents: Option<u64>,
    pub estimated_remaining_cents: Option<u64>,
}

/// The deterministic evaluator (§6–§7). Pure: `evaluate` returns signals
/// from a snapshot; it has no side effects and no LLM dependency.
#[derive(Debug, Clone, Copy, Default)]
pub struct WorkflowEvaluator;

impl WorkflowEvaluator {
    /// Deterministic rules (§7):
    /// - test failure → Replan candidate (Warning)
    /// - critical review finding → Replan candidate (Critical)
    /// - required artifact missing → Replan (Warning)
    /// - merge conflict → Replan candidate (Warning)
    /// - budget exceeded → Replan / Stop (Critical)
    /// - task repeatedly failed → Replan candidate (Warning)
    pub fn evaluate(&self, snapshot: &WorkflowSnapshot) -> Vec<ReplanSignal> {
        let mut out = Vec::new();
        for (task_id, count) in &snapshot.test_failures {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                Some(task_id.clone()),
                ReplanTrigger::TestsFailed,
                ReplanSeverity::Warning,
                format!("{count} test(s) failed in task {task_id}"),
                Vec::new(),
            ));
        }
        for (task_id, reason) in &snapshot.environment_failures {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                Some(task_id.clone()),
                ReplanTrigger::EnvironmentFailure,
                ReplanSeverity::Warning,
                format!("task {task_id} hit an environment failure: {reason}"),
                Vec::new(),
            ));
        }
        for (task_id, retries) in &snapshot.retries_exhausted {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                Some(task_id.clone()),
                ReplanTrigger::RepeatedRetryFailure,
                ReplanSeverity::Warning,
                format!("task {task_id} exhausted its retry budget ({retries} retries)"),
                Vec::new(),
            ));
        }
        for (task_id, art) in &snapshot.missing_artifacts {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                Some(task_id.clone()),
                ReplanTrigger::ArtifactMissing,
                ReplanSeverity::Warning,
                format!("task {task_id} requires missing artifact {art}"),
                vec![art.clone()],
            ));
        }
        for mc in &snapshot.merge_conflicts {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                Some(mc.task_id.clone()),
                ReplanTrigger::MergeConflict,
                ReplanSeverity::Warning,
                format!(
                    "merge conflict in task {} on {} files: {}",
                    mc.task_id,
                    mc.files.len(),
                    mc.files.join(", ")
                ),
                mc.files.clone(),
            ));
        }
        // Critical review findings (§27): a Critical finding must stop or
        // enter NeedsAttention according to policy — surfaced as a
        // Critical replan signal.
        let has_critical = snapshot
            .findings
            .iter()
            .any(|f| f.severity == Severity::Critical);
        if has_critical {
            out.push(ReplanSignal::new(
                &snapshot.workflow_id,
                None,
                ReplanTrigger::CriticalReviewFinding,
                ReplanSeverity::Critical,
                "a security/critical review finding requires remediation",
                snapshot
                    .findings
                    .iter()
                    .filter(|f| f.severity == Severity::Critical)
                    .map(|f| f.id.clone())
                    .collect(),
            ));
        }
        // Budget (§23): budget exceeded → Replan/Stop (Critical); risk of
        // exceeding (projected > remaining) → Warning.
        if let Some(b) = &snapshot.budget {
            if let Some(budget) = b.budget_cents {
                let projected = b.spent_cents + b.estimated_remaining_cents.unwrap_or(0);
                if b.spent_cents >= budget {
                    out.push(ReplanSignal::new(
                        &snapshot.workflow_id,
                        None,
                        ReplanTrigger::BudgetRisk,
                        ReplanSeverity::Critical,
                        format!(
                            "budget exceeded: spent ${}.{:02} ≥ budget ${}.{:02}",
                            b.spent_cents / 100,
                            b.spent_cents % 100,
                            budget / 100,
                            budget % 100
                        ),
                        Vec::new(),
                    ));
                } else if projected > budget {
                    out.push(ReplanSignal::new(
                        &snapshot.workflow_id,
                        None,
                        ReplanTrigger::BudgetRisk,
                        ReplanSeverity::Warning,
                        format!(
                            "budget risk: projected ${}.{:02} > budget ${}.{:02}",
                            projected / 100,
                            projected % 100,
                            budget / 100,
                            budget % 100
                        ),
                        Vec::new(),
                    ));
                }
            }
        }
        // Plain task failures → Replan candidate (unless already covered by
        // a more specific trigger).
        for t in &snapshot.tasks {
            if t.failed {
                let covered = out.iter().any(|s| {
                    s.task_id.as_deref() == Some(t.task_id.as_str())
                        && matches!(
                            s.trigger,
                            ReplanTrigger::TestsFailed
                                | ReplanTrigger::RepeatedRetryFailure
                                | ReplanTrigger::EnvironmentFailure
                        )
                });
                if !covered {
                    out.push(ReplanSignal::new(
                        &snapshot.workflow_id,
                        Some(t.task_id.clone()),
                        ReplanTrigger::TaskFailure,
                        ReplanSeverity::Warning,
                        format!("task {} failed", t.task_id),
                        Vec::new(),
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// §10 replan context
// ---------------------------------------------------------------------------

/// Bounded context handed to the planner on a replan (§10). Never dumps
/// unlimited logs — only structured summaries and bounded lists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplanContext {
    pub workflow_id: String,
    /// Original plan goal.
    pub goal: String,
    pub completed: Vec<String>,
    pub running: Vec<String>,
    pub failed: Vec<String>,
    pub remaining: Vec<String>,
    /// Artifact ids available (metadata only — never payloads).
    pub artifacts: Vec<String>,
    /// Review findings (id + severity + summary, bounded).
    pub findings: Vec<String>,
    /// Test report summaries (bounded).
    pub test_reports: Vec<String>,
    /// Worktree state summary (branch, state) — bounded.
    pub worktree_state: Vec<String>,
    pub budget: Option<BudgetObservation>,
    /// User constraints (bounded, free-form policy notes).
    pub user_constraints: Vec<String>,
}

impl ReplanContext {
    pub fn to_request_fragment(&self) -> String {
        format!(
            "replan(workflow={}, goal={:?}, completed={}, failed={}, remaining={}, \
             artifacts={}, budget={:?})",
            self.workflow_id,
            self.goal,
            self.completed.len(),
            self.failed.len(),
            self.remaining.len(),
            self.artifacts.len(),
            self.budget.as_ref().map(|b| b.spent_cents),
        )
    }
}

/// Builder for a bounded replan context (§10). The engine fills it from
/// authoritative state; bounds keep planner context fixed-size.
#[derive(Debug, Clone)]
pub struct ReplanContextBuilder {
    max_tasks: usize,
    max_findings: usize,
}

impl Default for ReplanContextBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplanContextBuilder {
    pub fn new() -> Self {
        Self {
            max_tasks: 64,
            max_findings: 32,
        }
    }

    pub fn with_task_bound(mut self, n: usize) -> Self {
        self.max_tasks = n;
        self
    }

    pub fn build(&self, wf: &WorkflowSnapshot) -> ReplanContext {
        let mut completed = Vec::new();
        let mut running = Vec::new();
        let mut failed = Vec::new();
        let mut remaining = Vec::new();
        for t in wf.tasks.iter().take(self.max_tasks) {
            let label = format!("{} ({:?})", t.task_id, t.status);
            match t.status {
                TaskStatus::Completed => completed.push(label),
                TaskStatus::Running
                | TaskStatus::Ready
                | TaskStatus::Waiting
                | TaskStatus::NeedsReview => running.push(label),
                TaskStatus::Failed | TaskStatus::Cancelled | TaskStatus::Skipped => {
                    failed.push(label)
                }
                TaskStatus::Pending | TaskStatus::Blocked | TaskStatus::Interrupted => {
                    remaining.push(label)
                }
            }
        }
        let mut findings: Vec<String> = wf
            .findings
            .iter()
            .take(self.max_findings)
            .map(|f| format!("{} [{}] {}", f.id, f.severity.label(), f.finding))
            .collect();
        findings.sort();
        ReplanContext {
            workflow_id: wf.workflow_id.clone(),
            goal: String::new(),
            completed,
            running,
            failed,
            remaining,
            artifacts: Vec::new(),
            findings,
            test_reports: Vec::new(),
            worktree_state: Vec::new(),
            budget: wf.budget.clone(),
            user_constraints: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// §12 proposed replan
// ---------------------------------------------------------------------------

/// The planner's replan output (§12). A *proposal* — the engine validates,
/// diffs and gates it behind human approval. Never an arbitrary replacement
/// TaskGraph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedReplan {
    pub id: String,
    pub workflow_id: String,
    pub reason: String,
    /// Human-readable summary of the changes.
    pub changes: Vec<String>,
    pub new_tasks: Vec<PlanStep>,
    pub removed_tasks: Vec<TaskId>,
    /// (step id, what changed) — descriptive, for the review UI.
    pub modified_tasks: Vec<(String, String)>,
    /// Extra dependencies the replan introduces: (task id, depends_on).
    pub dependencies: Vec<(String, Vec<String>)>,
    /// (step id, agent) recommendations.
    pub agent_recommendations: Vec<(String, String)>,
    pub estimated_cost_cents: Option<u64>,
    pub warnings: Vec<String>,
    /// Trigger(s) that produced this replan.
    pub triggers: Vec<ReplanTrigger>,
    /// The full proposed plan (steps + goal) from the planner.
    pub plan: ProposedPlan,
}

impl ProposedReplan {
    /// Wraps a planner `ProposedPlan` into a formal replan proposal.
    pub fn from_plan(
        workflow_id: impl Into<String>,
        reason: impl Into<String>,
        plan: ProposedPlan,
        triggers: Vec<ReplanTrigger>,
    ) -> Self {
        let changes: Vec<String> = plan
            .steps
            .iter()
            .map(|s| format!("{}: {}", s.id, s.title))
            .collect();
        Self {
            id: format!("replan:{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.into(),
            reason: reason.into(),
            changes,
            new_tasks: plan.steps.clone(),
            removed_tasks: Vec::new(),
            modified_tasks: Vec::new(),
            dependencies: plan
                .steps
                .iter()
                .filter(|s| !s.depends_on.is_empty())
                .map(|s| (s.id.clone(), s.depends_on.clone()))
                .collect(),
            agent_recommendations: plan
                .steps
                .iter()
                .filter_map(|s| {
                    s.agent_recommendation
                        .as_ref()
                        .map(|r| (s.id.clone(), r.agent_definition_id.clone()))
                })
                .collect(),
            estimated_cost_cents: plan.estimated_cost_cents,
            warnings: plan.warnings.clone(),
            triggers,
            plan,
        }
    }
}

// ---------------------------------------------------------------------------
// §14 plan diff
// ---------------------------------------------------------------------------

/// Structural diff between two plans (§14): added/removed/modified tasks,
/// changed agents, dependencies, budget. Used for the review UI and the
/// audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PlanDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
    pub changed_agents: Vec<(String, String, String)>, // step, old, new
    pub changed_dependencies: Vec<(String, Vec<String>, Vec<String>)>,
    pub changed_budget: Option<(Option<u64>, Option<u64>)>,
}

impl PlanDiff {
    pub fn between(prev: &ProposedPlan, next: &ProposedPlan) -> Self {
        let prev_ids: std::collections::HashSet<String> =
            prev.steps.iter().map(|s| s.id.clone()).collect();
        let next_ids: std::collections::HashSet<String> =
            next.steps.iter().map(|s| s.id.clone()).collect();
        let prev_by_id: std::collections::HashMap<&str, &PlanStep> =
            prev.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        let next_by_id: std::collections::HashMap<&str, &PlanStep> =
            next.steps.iter().map(|s| (s.id.as_str(), s)).collect();

        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();
        let mut changed_agents = Vec::new();
        let mut changed_dependencies = Vec::new();

        for id in &next_ids {
            if !prev_ids.contains(id) {
                added.push(id.clone());
            }
        }
        for id in &prev_ids {
            if !next_ids.contains(id) {
                removed.push(id.clone());
            }
        }
        for id in &next_ids {
            if let (Some(p), Some(n)) = (prev_by_id.get(id.as_str()), next_by_id.get(id.as_str())) {
                if p != n {
                    modified.push(id.clone());
                    let pa = p
                        .agent_recommendation
                        .as_ref()
                        .map(|r| r.agent_definition_id.clone())
                        .unwrap_or_default();
                    let na = n
                        .agent_recommendation
                        .as_ref()
                        .map(|r| r.agent_definition_id.clone())
                        .unwrap_or_default();
                    if pa != na {
                        changed_agents.push((id.clone(), pa, na));
                    }
                    if p.depends_on != n.depends_on {
                        changed_dependencies.push((
                            id.clone(),
                            p.depends_on.clone(),
                            n.depends_on.clone(),
                        ));
                    }
                }
            }
        }
        let changed_budget = if prev.estimated_cost_cents != next.estimated_cost_cents {
            Some((prev.estimated_cost_cents, next.estimated_cost_cents))
        } else {
            None
        };
        Self {
            added,
            removed,
            modified,
            changed_agents,
            changed_dependencies,
            changed_budget,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.modified.is_empty()
            && self.changed_agents.is_empty()
            && self.changed_dependencies.is_empty()
            && self.changed_budget.is_none()
    }
}

// ---------------------------------------------------------------------------
// §13 immutable plan versions
// ---------------------------------------------------------------------------

/// One immutable plan version (§13). Versions are never mutated — a new
/// plan creates a new version; `superseded_by` links v1 → v2 → v3.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanVersion {
    pub version: u32,
    pub plan_id: String,
    pub goal: String,
    pub plan: ProposedPlan,
    /// v1 → superseded by v2, etc. None = current.
    pub superseded_by: Option<u32>,
    pub created_at: u64,
    /// Diff from the previous version (None for v1).
    pub diff_from_previous: Option<PlanDiff>,
    /// Whether this version was user-approved.
    pub approved: bool,
    /// Approval timestamp (None until approved).
    pub approved_at: Option<u64>,
}

impl PlanVersion {
    pub fn new(
        version: u32,
        plan: ProposedPlan,
        previous: Option<&PlanVersion>,
        approved: bool,
    ) -> Self {
        let diff = previous.map(|p| PlanDiff::between(&p.plan, &plan));
        Self {
            version,
            plan_id: format!("plan-v{version}"),
            goal: plan.goal.clone(),
            plan,
            superseded_by: None,
            created_at: now_ms(),
            diff_from_previous: diff,
            approved,
            approved_at: if approved { Some(now_ms()) } else { None },
        }
    }
}

// ---------------------------------------------------------------------------
// §19–§20 invalidation
// ---------------------------------------------------------------------------

/// Explicit invalidation of a completed task (§19). Requires reason +
/// evidence, and normally human approval — the planner can never silently
/// invalidate completed work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInvalidation {
    pub task_id: TaskId,
    pub reason: String,
    pub evidence: Vec<String>,
    pub approved: bool,
    pub approved_at: Option<u64>,
    pub created_at: u64,
}

impl TaskInvalidation {
    pub fn new(task_id: TaskId, reason: impl Into<String>, evidence: Vec<String>) -> Self {
        Self {
            task_id,
            reason: reason.into(),
            evidence,
            approved: false,
            approved_at: None,
            created_at: now_ms(),
        }
    }
}

/// Explicit invalidation of an artifact (§20). The old artifact is
/// **preserved** for lineage — never deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactInvalidation {
    pub artifact_id: String,
    pub reason: String,
    pub evidence: Vec<String>,
    pub created_at: u64,
}

impl ArtifactInvalidation {
    pub fn new(
        artifact_id: impl Into<String>,
        reason: impl Into<String>,
        evidence: Vec<String>,
    ) -> Self {
        Self {
            artifact_id: artifact_id.into(),
            reason: reason.into(),
            evidence,
            created_at: now_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// §33 human escalation
// ---------------------------------------------------------------------------

/// A human-escalation record (§33): what happened, what was attempted,
/// what evidence exists, what options are available. Never hides
/// uncertainty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanEscalation {
    pub id: String,
    pub workflow_id: String,
    pub what_happened: String,
    pub attempted: Vec<String>,
    pub evidence: Vec<String>,
    pub options: Vec<String>,
    pub created_at: u64,
}

impl HumanEscalation {
    pub fn new(
        workflow_id: impl Into<String>,
        what_happened: impl Into<String>,
        attempted: Vec<String>,
        evidence: Vec<String>,
        options: Vec<String>,
    ) -> Self {
        Self {
            id: format!("escalation:{}", uuid::Uuid::new_v4()),
            workflow_id: workflow_id.into(),
            what_happened: what_happened.into(),
            attempted,
            evidence,
            options,
            created_at: now_ms(),
        }
    }
}

// ---------------------------------------------------------------------------
// §34–§37 autonomy policy
// ---------------------------------------------------------------------------

/// Autonomy policy levels (§34). **Automatic is a future capability and is
/// disabled in Phase 3E** — the abstraction exists so the policy is
/// deterministic configuration, never planner-controlled (§38).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyPolicy {
    /// Every replan requires human approval (§35).
    #[default]
    Manual,
    /// The system prepares a replan and highlights it; the user approves
    /// (§36).
    Assisted,
    /// Future capability — NOT enabled in Phase 3E (§37).
    Automatic,
}

// ---------------------------------------------------------------------------
// §42 persistence
// ---------------------------------------------------------------------------

/// Persisted slice of the adaptive state (3e.md §42). Audit history (plan
/// versions, diffs, approvals/rejections, invalidations, escalations) is
/// durable; the dedup registry is runtime-only (rebuilt on restart). Never
/// contains credentials or private reasoning (§43).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PersistedAdaptiveState {
    pub signals: Vec<ReplanSignal>,
    pub plan_versions: Vec<PlanVersion>,
    pub proposals: Vec<ProposedReplan>,
    pub task_invalidations: Vec<TaskInvalidation>,
    pub artifact_invalidations: Vec<ArtifactInvalidation>,
    pub escalations: Vec<HumanEscalation>,
    pub autonomy: AutonomyPolicy,
    pub limits: ReplanLimits,
    pub metrics: ReplanMetrics,
    pub quality: PlannerQualityMetrics,
    pub limit_reached: bool,
}

// ---------------------------------------------------------------------------
// §24–§25 metrics
// ---------------------------------------------------------------------------

/// Replan metrics (§24) — the product metrics for adaptive orchestration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplanMetrics {
    pub replan_count: u32,
    pub replan_trigger_count: u32,
    pub replan_approval_count: u32,
    pub replan_rejection_count: u32,
    pub replan_edit_count: u32,
    /// ms from first signal to approved replan (sum; count = replan_count).
    pub time_to_replan_ms: u64,
    /// Additional estimated cost of approved replans (cents).
    pub additional_cost_cents: u64,
    /// Workflows that recovered (completed after a replan).
    pub workflow_recovery_count: u32,
    /// Per-trigger counts.
    pub trigger_counts: std::collections::BTreeMap<String, u32>,
}

impl ReplanMetrics {
    pub fn approval_rate(&self) -> f64 {
        if self.replan_count == 0 {
            0.0
        } else {
            self.replan_approval_count as f64 / self.replan_count as f64
        }
    }

    pub fn rejection_rate(&self) -> f64 {
        if self.replan_count == 0 {
            0.0
        } else {
            self.replan_rejection_count as f64 / self.replan_count as f64
        }
    }
}

/// Planner-quality metrics (§25) — tracked separately from execution
/// metrics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerQualityMetrics {
    pub valid_replan_rate: u32,
    pub invalid_replan_rate: u32,
    pub human_edit_rate: u32,
    pub human_rejection_rate: u32,
    pub successful_replan_rate: u32,
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::TaskStatus;

    fn signal(
        wf: &str,
        task: Option<&str>,
        trigger: ReplanTrigger,
        sev: ReplanSeverity,
    ) -> ReplanSignal {
        ReplanSignal::new(
            wf,
            task.map(|t| t.to_string()),
            trigger,
            sev,
            "reason",
            Vec::new(),
        )
    }

    #[test]
    fn dedupe_key_coalesces_equivalent_signals() {
        let a = signal(
            "wf",
            Some("t1"),
            ReplanTrigger::TaskFailure,
            ReplanSeverity::Warning,
        );
        let b = signal(
            "wf",
            Some("t1"),
            ReplanTrigger::TaskFailure,
            ReplanSeverity::Warning,
        );
        assert_eq!(a.dedupe_key(), b.dedupe_key());
        let c = signal(
            "wf",
            Some("t2"),
            ReplanTrigger::TaskFailure,
            ReplanSeverity::Warning,
        );
        assert_ne!(a.dedupe_key(), c.dedupe_key());
    }

    #[test]
    fn cooldown_admits_then_rejects() {
        let mut reg = SignalRegistry::new();
        let s = signal(
            "wf",
            Some("t1"),
            ReplanTrigger::TaskFailure,
            ReplanSeverity::Warning,
        );
        assert!(reg.admit(&s, 30));
        // Same key within cooldown → rejected.
        assert!(!reg.admit(&s, 30));
    }

    #[test]
    fn evaluator_emits_expected_triggers() {
        let snap = WorkflowSnapshot {
            workflow_id: "wf".into(),
            tasks: vec![TaskObservation {
                task_id: "t-fail".into(),
                status: TaskStatus::Failed,
                failed: true,
                retries: 0,
            }],
            findings: vec![ReviewFinding::new(Severity::Critical, "vuln", None)],
            missing_artifacts: vec![("t2".to_string(), "artifact:x".to_string())],
            merge_conflicts: vec![MergeConflictEvidence {
                task_id: "t3".into(),
                files: vec!["a.rs".into()],
                branches: vec!["main".into(), "flash/task/t3".into()],
                base: Some("abc".into()),
                ours: "main".into(),
                theirs: "flash/task/t3".into(),
            }],
            budget: Some(BudgetObservation {
                spent_cents: 600,
                budget_cents: Some(500),
                estimated_remaining_cents: Some(100),
            }),
            test_failures: vec![("t4".to_string(), 14)],
            environment_failures: Vec::new(),
            retries_exhausted: vec![("t5".to_string(), 3)],
        };
        let ev = WorkflowEvaluator;
        let signals = ev.evaluate(&snap);
        let triggers: Vec<ReplanTrigger> = signals.iter().map(|s| s.trigger).collect();
        assert!(triggers.contains(&ReplanTrigger::TaskFailure));
        assert!(triggers.contains(&ReplanTrigger::CriticalReviewFinding));
        assert!(triggers.contains(&ReplanTrigger::ArtifactMissing));
        assert!(triggers.contains(&ReplanTrigger::MergeConflict));
        assert!(triggers.contains(&ReplanTrigger::BudgetRisk));
        assert!(triggers.contains(&ReplanTrigger::TestsFailed));
        assert!(triggers.contains(&ReplanTrigger::RepeatedRetryFailure));
        // Critical finding + budget exceeded → Critical severity.
        let crit = signals
            .iter()
            .find(|s| s.trigger == ReplanTrigger::CriticalReviewFinding)
            .unwrap();
        assert_eq!(crit.severity, ReplanSeverity::Critical);
        let budget = signals
            .iter()
            .find(|s| s.trigger == ReplanTrigger::BudgetRisk)
            .unwrap();
        assert_eq!(budget.severity, ReplanSeverity::Critical);
    }

    #[test]
    fn task_failure_not_duplicated_by_specific_trigger() {
        let snap = WorkflowSnapshot {
            workflow_id: "wf".into(),
            tasks: vec![TaskObservation {
                task_id: "t4".into(),
                status: TaskStatus::Failed,
                failed: true,
                retries: 0,
            }],
            test_failures: vec![("t4".to_string(), 3)],
            ..Default::default()
        };
        let signals = WorkflowEvaluator.evaluate(&snap);
        let t4: Vec<_> = signals
            .iter()
            .filter(|s| s.task_id.as_deref() == Some("t4"))
            .collect();
        assert_eq!(
            t4.len(),
            1,
            "TestsFailed covers t4, no duplicate TaskFailure"
        );
        assert_eq!(t4[0].trigger, ReplanTrigger::TestsFailed);
    }

    #[test]
    fn plan_diff_detects_added_removed_modified_and_agents() {
        let mk = |goal: &str, steps: Vec<(&str, &str, &str, &[&str])>| -> ProposedPlan {
            let steps = steps
                .into_iter()
                .map(|(id, title, agent, deps)| PlanStep {
                    id: id.to_string(),
                    title: title.to_string(),
                    description: String::new(),
                    agent_recommendation: Some(crate::planning::AgentRecommendation {
                        agent_definition_id: agent.to_string(),
                        reason: None,
                        confidence: Some(1.0),
                    }),
                    depends_on: deps.iter().map(|d| d.to_string()).collect(),
                    isolation: crate::worktrees::IsolationMode::GitWorktree,
                    requires_shared_workspace: false,
                })
                .collect();
            ProposedPlan {
                goal: goal.to_string(),
                steps,
                estimated_cost_cents: Some(100),
                estimated_duration_min: None,
                reasoning_summary: None,
                warnings: Vec::new(),
            }
        };
        let v1 = mk(
            "Implement",
            vec![
                ("research", "Research", "fake-agent", &[]),
                ("impl", "Implement", "fake-agent", &["research"]),
                ("tests", "Tests", "fake-agent", &["impl"]),
            ],
        );
        let v2 = mk(
            "Implement",
            vec![
                ("investigate", "Investigate failures", "fake-agent", &[]),
                ("research", "Research", "fake-agent", &["investigate"]),
                ("impl", "Implement", "fake-agent", &["research"]),
            ],
        );
        let diff = PlanDiff::between(&v1, &v2);
        assert!(diff.added.contains(&"investigate".to_string()));
        assert!(diff.removed.contains(&"tests".to_string()));
        // research changed (new dependency) → modified.
        assert!(diff.modified.contains(&"research".to_string()));
        assert!(!diff.is_empty());
    }

    #[test]
    fn plan_version_links_superseded() {
        let plan1 = ProposedPlan {
            goal: "g".into(),
            steps: Vec::new(),
            estimated_cost_cents: None,
            estimated_duration_min: None,
            reasoning_summary: None,
            warnings: Vec::new(),
        };
        let v1 = PlanVersion::new(1, plan1.clone(), None, true);
        let mut v2 = PlanVersion::new(2, plan1.clone(), Some(&v1), false);
        // Mark v1 superseded by v2.
        let mut v1_mut = v1;
        v1_mut.superseded_by = Some(2);
        v2.superseded_by = None;
        assert_eq!(v1_mut.superseded_by, Some(2));
        assert!(v2.diff_from_previous.is_some());
    }

    #[test]
    fn replan_limits_default_sane() {
        let l = ReplanLimits::default();
        assert_eq!(l.max_replans, 5);
        assert_eq!(l.replan_cooldown_seconds, 30);
        assert_eq!(AutonomyPolicy::default(), AutonomyPolicy::Manual);
    }
}
