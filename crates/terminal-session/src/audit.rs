//! First-class audit trail (phases/4.md §17–§19).
//!
//! Every meaningful event — plan created, plan approved, policy evaluated,
//! action allowed/denied, approval requested/granted/rejected, agent
//! started/stopped, artifact created/modified, replan created/approved/
//! rejected, workflow completed/failed, stop/pause — is recorded with:
//!
//! ```text
//! timestamp   workflow   agent   task   action   result   source
//! ```
//!
//! The trail is **bounded in RAM** (newest kept, §39), **persisted** with
//! the workspace state, and **never contains credentials** (§40) — the
//! writer re-runs the redactor defensively. [`AuditTrail::explain`] renders
//! a human-readable "Why did FlashTerminal do this?" for the UX (§18),
//! without leaking internal details unless expanded.

use serde::{Deserialize, Serialize};

use crate::planning::now_ms;
use crate::policy::RiskLevel;
use crate::redact::Redactor;

// ---------------------------------------------------------------------------
// §17 event model
// ---------------------------------------------------------------------------

/// What happened (§17/§18). Kept deliberately small and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    // plans
    PlanCreated,
    PlanValidated,
    PlanApproved,
    PlanRejected,
    PlanExecuted,
    PlanSuperseded,
    // policy
    PolicyEvaluated,
    ActionAllowed,
    ActionDenied,
    ActionRequiredApproval,
    // approvals
    ApprovalRequested,
    ApprovalGranted,
    ApprovalRejected,
    ApprovalExpired,
    ApprovalInvalidated,
    ApprovalReplayBlocked,
    // agents
    AgentStarted,
    AgentStopped,
    AgentCrashed,
    AgentResumed,
    // artifacts
    ArtifactCreated,
    ArtifactModified,
    ArtifactInvalidated,
    // replans
    ReplanCreated,
    ReplanApproved,
    ReplanRejected,
    ReplanInvalidated,
    ReplanLimitReached,
    // workflow
    WorkflowStarted,
    WorkflowPaused,
    WorkflowResumed,
    WorkflowStopped,
    WorkflowCompleted,
    WorkflowFailed,
    // safety
    HumanEscalationRaised,
    BudgetExceeded,
    BudgetIncreased,
    NetworkDenied,
    SecretDenied,
    FilesystemDenied,
    PauseAll,
    StopAll,
    WorkflowReverted,
}

impl AuditEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PlanCreated => "plan created",
            Self::PlanValidated => "plan validated",
            Self::PlanApproved => "plan approved",
            Self::PlanRejected => "plan rejected",
            Self::PlanExecuted => "plan executed",
            Self::PlanSuperseded => "plan superseded",
            Self::PolicyEvaluated => "policy evaluated",
            Self::ActionAllowed => "action allowed",
            Self::ActionDenied => "action denied",
            Self::ActionRequiredApproval => "action required approval",
            Self::ApprovalRequested => "approval requested",
            Self::ApprovalGranted => "approval granted",
            Self::ApprovalRejected => "approval rejected",
            Self::ApprovalExpired => "approval expired",
            Self::ApprovalInvalidated => "approval invalidated",
            Self::ApprovalReplayBlocked => "approval replay blocked",
            Self::AgentStarted => "agent started",
            Self::AgentStopped => "agent stopped",
            Self::AgentCrashed => "agent crashed",
            Self::AgentResumed => "agent resumed",
            Self::ArtifactCreated => "artifact created",
            Self::ArtifactModified => "artifact modified",
            Self::ArtifactInvalidated => "artifact invalidated",
            Self::ReplanCreated => "replan created",
            Self::ReplanApproved => "replan approved",
            Self::ReplanRejected => "replan rejected",
            Self::ReplanInvalidated => "replan invalidated",
            Self::ReplanLimitReached => "replan limit reached",
            Self::WorkflowStarted => "workflow started",
            Self::WorkflowPaused => "workflow paused",
            Self::WorkflowResumed => "workflow resumed",
            Self::WorkflowStopped => "workflow stopped",
            Self::WorkflowCompleted => "workflow completed",
            Self::WorkflowFailed => "workflow failed",
            Self::HumanEscalationRaised => "human escalation raised",
            Self::BudgetExceeded => "budget exceeded",
            Self::BudgetIncreased => "budget increased",
            Self::NetworkDenied => "network denied",
            Self::SecretDenied => "secret denied",
            Self::FilesystemDenied => "filesystem denied",
            Self::PauseAll => "pause all",
            Self::StopAll => "stop all",
            Self::WorkflowReverted => "workflow reverted",
        }
    }

    /// Whether this event is human-initiated (not agent-initiated).
    pub fn is_human_initiated(self) -> bool {
        matches!(
            self,
            Self::PlanApproved
                | Self::PlanRejected
                | Self::ApprovalGranted
                | Self::ApprovalRejected
                | Self::ReplanApproved
                | Self::ReplanRejected
                | Self::WorkflowPaused
                | Self::WorkflowResumed
                | Self::WorkflowStopped
                | Self::PauseAll
                | Self::StopAll
                | Self::BudgetIncreased
                | Self::WorkflowReverted
        )
    }
}

/// Result of an audited action (free-form, redacted at write time).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditResult {
    Success,
    Failure(String),
    Pending,
    Denied(String),
}

/// One audit record (§17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Stable id: `audit:<uuid>`.
    pub id: String,
    pub kind: AuditEventKind,
    /// ms epoch.
    pub timestamp_ms: u64,
    pub workflow_id: String,
    pub agent_id: Option<String>,
    pub task_id: Option<String>,
    /// Human-facing action description (redacted).
    pub action: String,
    pub risk: Option<RiskLevel>,
    pub result: AuditResult,
    /// Who/what initiated this (user handle, engine, planner, agent).
    pub source: String,
    /// Optional structured detail (redacted; never chain-of-thought).
    pub detail: String,
}

impl AuditEvent {
    pub fn new(
        kind: AuditEventKind,
        workflow_id: impl Into<String>,
        action: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            id: format!("audit:{}", uuid::Uuid::new_v4()),
            kind,
            timestamp_ms: now_ms(),
            workflow_id: workflow_id.into(),
            agent_id: None,
            task_id: None,
            action: action.into(),
            risk: None,
            result: AuditResult::Success,
            source: source.into(),
            detail: String::new(),
        }
    }

    pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
        self.agent_id = Some(agent.into());
        self
    }

    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task_id = Some(task.into());
        self
    }

    pub fn with_risk(mut self, risk: RiskLevel) -> Self {
        self.risk = Some(risk);
        self
    }

    pub fn with_result(mut self, result: AuditResult) -> Self {
        self.result = result;
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// Defense-in-depth redaction: any value that looks like a secret is
    /// masked before this event enters the trail (§40).
    pub fn redact(&mut self) -> &mut Self {
        self.action = Redactor::redact(&self.action);
        self.detail = Redactor::redact(&self.detail);
        match &mut self.result {
            AuditResult::Failure(m) | AuditResult::Denied(m) => {
                let cloned = m.clone();
                *m = Redactor::redact(&cloned);
            }
            _ => {}
        }
        self
    }
}

// ---------------------------------------------------------------------------
// §17 store
// ---------------------------------------------------------------------------

/// The audit trail (§17). Bounded in RAM; optionally disk-backed tail
/// (see [`AuditTrail::with_disk_backing`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditTrail {
    /// Newest first.
    events: Vec<AuditEvent>,
    /// Serialized events kept on disk beyond the in-memory cap.
    disk_backing: Option<String>,
    // Non-serialized: parsed back on load.
    #[serde(skip)]
    disk_events: Vec<AuditEvent>,
    /// In-memory cap (events kept in RAM).
    pub max_memory_events: usize,
    pub version: u32,
}

impl Default for AuditTrail {
    fn default() -> Self {
        Self::new()
    }
}

impl AuditTrail {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            disk_backing: None,
            disk_events: Vec::new(),
            max_memory_events: 512,
            version: 1,
        }
    }

    /// Older audit history may be disk-backed where appropriate (§39). The
    /// engine can point this at `state_dir/audit.jsonl`; the trail appends
    /// serialized events and keeps a bounded tail in RAM.
    pub fn with_disk_backing(mut self, path: impl Into<String>) -> Self {
        self.disk_backing = Some(path.into());
        self
    }

    pub fn disk_backing(&self) -> Option<&str> {
        self.disk_backing.as_deref()
    }

    /// Records one event. Returns its id. Applies defensive redaction,
    /// bounds RAM, and (when disk-backed) appends to the log line file.
    pub fn record(&mut self, mut event: AuditEvent) -> String {
        event.redact();
        let id = event.id.clone();
        self.events.insert(0, event);
        if self.events.len() > self.max_memory_events {
            self.events.truncate(self.max_memory_events);
        }
        if let Some(path) = self.disk_backing.clone() {
            if let Some(first) = self.events.first() {
                if let Ok(json) = serde_json::to_string(first) {
                    // Best-effort append — never fatal for audit.
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map(|mut f| {
                            let _ = std::io::Write::write_all(&mut f, json.as_bytes());
                            let _ = std::io::Write::write_all(&mut f, b"\n");
                        });
                }
            }
        }
        id
    }

    /// Records a simply-built event chain and returns the id.
    pub fn record_kind(
        &mut self,
        kind: AuditEventKind,
        workflow_id: impl Into<String>,
        action: impl Into<String>,
        source: impl Into<String>,
    ) -> String {
        self.record(AuditEvent::new(kind, workflow_id, action, source))
    }

    pub fn all(&self) -> &[AuditEvent] {
        &self.events
    }

    pub fn recent(&self, n: usize) -> Vec<&AuditEvent> {
        self.events.iter().take(n).collect()
    }

    pub fn by_workflow(&self, workflow_id: &str) -> Vec<&AuditEvent> {
        self.events
            .iter()
            .filter(|e| e.workflow_id == workflow_id)
            .collect()
    }

    pub fn of_kind(&self, kind: AuditEventKind) -> Vec<&AuditEvent> {
        self.events.iter().filter(|e| e.kind == kind).collect()
    }

    pub fn get(&self, id: &str) -> Option<&AuditEvent> {
        self.events.iter().find(|e| e.id == id)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Persisted slice: the in-memory ring is what survives restarts in
    /// the session state (disk events are best-effort and re-read from the
    /// jsonl file by the engine only when configured).
    pub fn persisted(&self) -> Vec<AuditEvent> {
        self.events.clone()
    }

    /// Rebuilds a trail from a persisted slice (§25). Bounds and fresh
    /// ordering are reapplied — a corrupted/hostile payload cannot grow RAM
    /// or spoof internal invariants (redaction stays defensive on write).
    pub fn from_events(events: Vec<AuditEvent>) -> Self {
        let mut trail = Self::new();
        for event in events.into_iter().rev() {
            trail.record(event);
        }
        trail
    }

    // ------------------------------------------------------------------
    // §18 explanation rendering
    // ------------------------------------------------------------------

    /// Renders one event as a readable "Why did FlashTerminal do this?"
    /// explanation (§18). Returns `None` for unknown id.
    pub fn explain(&self, id: &str) -> Option<String> {
        let e = self.get(id)?;
        Some(self.render(e))
    }

    /// Renders a readable explanation for the *most recent* event of a
    /// given kind (used by the UX to answer "why did X happen").
    pub fn explain_latest(&self, kind: AuditEventKind) -> Option<String> {
        self.of_kind(kind).first().map(|e| self.render(e))
    }

    fn render(&self, e: &AuditEvent) -> String {
        let who = e.agent_id.as_deref().unwrap_or("FlashTerminal");
        let mut out = String::new();
        out.push_str(&format!("{} {}.\n", who, e.kind.as_str()));
        if !e.action.is_empty() {
            out.push_str(&format!("\nAction:\n{}\n", e.action));
        }
        if let Some(risk) = e.risk {
            out.push_str(&format!("\nRisk:\n{}\n", risk.label()));
        }
        if !e.detail.is_empty() {
            out.push_str(&format!("\nReason:\n{}\n", e.detail));
        }
        if let Some(task) = &e.task_id {
            out.push_str(&format!("\nTask:\n{task}\n"));
        }
        match &e.result {
            AuditResult::Success => out.push_str("\nResult:\nSuccess\n"),
            AuditResult::Pending => out.push_str("\nResult:\nPending\n"),
            AuditResult::Failure(m) | AuditResult::Denied(m) => {
                out.push_str(&format!("\nResult:\n{m}\n"))
            }
        }
        out.push_str(&format!("\nSource:\n{}\n", e.source));
        out
    }
}

// ---------------------------------------------------------------------------
// §18 example UX copy
// ---------------------------------------------------------------------------

/// Example messages the UI surfaces verbatim (§18) — shared so the tests
/// and the desktop stay in sync.
pub mod copy {
    pub fn action_allowed(agent: &str, action: &str, task: &str, policy: &str) -> String {
        format!(
            "{agent} {action}.\n\nReason:\nTask \"{task}\"\n\nPolicy:\n{policy}\n\nApproval:\nNot required\n\nResult:\nSuccess"
        )
    }

    pub fn action_requires_approval(agent: &str, action: &str, risk: &str, policy: &str) -> String {
        format!(
            "{agent} requested:\n\n{action}\n\nRisk:\n{risk}\n\nPolicy:\n{policy}\n\nDecision:\nAwaiting your approval"
        )
    }

    pub fn action_approved_by(
        agent: &str,
        action: &str,
        risk: &str,
        policy: &str,
        user: &str,
    ) -> String {
        format!(
            "{agent} requested:\n\n{action}\n\nRisk:\n{risk}\n\nPolicy:\n{policy}\n\nDecision:\nApproved by {user}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Action, AutonomyLevel, PolicyContext, PolicyEngine};

    #[test]
    fn trail_records_and_bounds_memory() {
        let mut t = AuditTrail::new();
        for i in 0..2000 {
            t.record_kind(
                AuditEventKind::PolicyEvaluated,
                "wf",
                format!("action {i}"),
                "engine",
            );
        }
        assert!(t.len() <= 512, "audit memory must stay bounded");
        assert_eq!(t.of_kind(AuditEventKind::PolicyEvaluated).len(), 512);
    }

    #[test]
    fn latest_first_ordering() {
        let mut t = AuditTrail::new();
        let first = t.record_kind(AuditEventKind::AgentStarted, "wf", "agent start", "engine");
        let second = t.record_kind(AuditEventKind::AgentStopped, "wf", "agent stop", "engine");
        assert_eq!(t.all()[0].id, second);
        assert_eq!(t.all()[1].id, first);
        assert!(t.explain(&first).is_some());
        assert!(t.get("audit:nope").is_none());
    }

    #[test]
    fn explain_renders_readable_text() {
        let mut t = AuditTrail::new();
        let id = t.record_kind(AuditEventKind::ActionAllowed, "wf", "npm install", "engine");
        let text = t.explain(&id).unwrap();
        assert!(text.contains("npm install"));
        assert!(text.contains("Action"));
    }

    #[test]
    fn explanation_does_not_expose_secrets() {
        let mut e = AuditEvent::new(AuditEventKind::ActionDenied, "wf", "cat .env", "engine")
            .with_detail("blocked: value sk-ant-api03-11111111111111111111111111111111 seen")
            .with_result(AuditResult::Denied(
                "sk-ant-api03-22222222222222222222222222222222 leaked?".into(),
            ));
        e.redact();
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("sk-ant-"), "redactor must mask the token");
        assert!(!json.contains("11111111111111111111111111111111"));
        assert!(json.contains("REDACTED"));
    }

    #[test]
    fn human_events_marked() {
        assert!(AuditEventKind::ApprovalGranted.is_human_initiated());
        assert!(!AuditEventKind::AgentStarted.is_human_initiated());
    }

    #[test]
    fn copy_examples_match_phase4_docs() {
        let allowed = copy::action_allowed(
            "Claude",
            "modified auth.ts",
            "Implement OAuth",
            "WorktreeOnly",
        );
        assert!(allowed.contains("Implement OAuth"));
        assert!(allowed.contains("WorktreeOnly"));
        assert!(allowed.contains("Not required"));
        let approved = copy::action_approved_by(
            "Claude",
            "npm install",
            "Medium",
            "Network requires approval",
            "Ali",
        );
        assert!(approved.contains("Approved by Ali"));
    }

    #[test]
    fn policy_decisions_flow_into_audit() {
        let e = PolicyEngine::default();
        let mut t = AuditTrail::new();
        let ev = e.evaluate(
            &crate::policy::Action::Process(crate::policy::CommandSpec::from_shell(
                "rm -rf /tmp/x",
            )),
            &PolicyContext::new("wf"),
        );
        let id = t.record_kind(
            match ev.decision {
                crate::policy::PolicyDecision::Deny => AuditEventKind::ActionDenied,
                crate::policy::PolicyDecision::RequireApproval => {
                    AuditEventKind::ActionRequiredApproval
                }
                crate::policy::PolicyDecision::Allow => AuditEventKind::ActionAllowed,
            },
            "wf",
            ev.action.clone(),
            "policy",
        );
        assert!(t.explain(&id).unwrap().contains(&ev.action));
        let _ = (
            AutonomyLevel::default(),
            Action::WorkspaceControl {
                operation: "stop".into(),
            },
        );
    }
}
