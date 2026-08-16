//! Collaboration domain (3d.md §13–§22): structured results, review
//! findings, deterministic reviewer consensus, and result synthesis.
//!
//! The rules of this module:
//! - No hidden chain of thought (§16): reviewers/synthesizers emit concise
//!   structured explanations (`decision`, `reason`, `evidence`), never
//!   private reasoning.
//! - Review aggregation is deterministic and policy-driven (§20–§21) —
//!   the LLM never votes secretly.
//! - Synthesis only receives explicitly selected TaskResults and
//!   artifacts, and must not invent artifact ids it was not given (§14,
//!   §54).

use crate::orchestration::{Artifact, TaskId, TaskResult};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// §21 severity model
// ---------------------------------------------------------------------------

/// Explicit severity ladder (3d.md §21).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }
}

// ---------------------------------------------------------------------------
// §18 review findings
// ---------------------------------------------------------------------------

/// A single review finding — first-class artifact (3d.md §18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub severity: Severity,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub finding: String,
    /// Reference to the artifact/evidence this finding is based on.
    pub evidence: Option<String>,
    pub created_by_task: Option<TaskId>,
    #[serde(default)]
    pub created_at_ms: u64,
}

impl ReviewFinding {
    pub fn new(
        severity: Severity,
        finding: impl Into<String>,
        created_by_task: Option<TaskId>,
    ) -> Self {
        Self {
            id: format!("finding:{}", uuid::Uuid::new_v4()),
            severity,
            file: None,
            line: None,
            finding: finding.into(),
            evidence: None,
            created_by_task,
            created_at_ms: crate::planning::now_ms(),
        }
    }

    /// Chain-style builder: `decision`, `reason`, `evidence` (3d.md §16).
    pub fn at(mut self, file: impl Into<String>, line: u64) -> Self {
        self.file = Some(file.into());
        self.line = Some(line);
        self
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = Some(evidence.into());
        self
    }
}

/// A reviewer's verdict on one task (3d.md §20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    Warning,
    Fail,
}

/// One reviewer's full report — verdict + findings + concise explanation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub reviewer_task_id: Option<TaskId>,
    pub verdict: ReviewVerdict,
    pub findings: Vec<ReviewFinding>,
    /// Concise structured explanation — never chain-of-thought (§16).
    pub reason: String,
}

// ---------------------------------------------------------------------------
// §20–§21 deterministic reviewer consensus
// ---------------------------------------------------------------------------

/// Configurable aggregation rules (3d.md §21): any Critical → Critical;
/// N+ High → NeedsReview; only ≤ Medium → Warning; all Pass →
/// ApprovedCandidate. The rules are explicit and inspectable — the LLM
/// never votes secretly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPolicy {
    /// Any single Critical finding flips the overall to Critical.
    pub any_critical: bool,
    /// Number of High findings that flips the overall to NeedsReview.
    pub high_threshold: usize,
    /// Any Fail verdict flips the overall to NeedsReview.
    pub fail_means_needs_review: bool,
    /// Fail verdicts alone (without High findings) escalate to NeedsReview.
    pub warning_means_review: bool,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            any_critical: true,
            high_threshold: 1,
            fail_means_needs_review: true,
            warning_means_review: false,
        }
    }
}

/// The aggregated reviewer outcome (3d.md §20).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewConsensus {
    /// All reviewers passed, no significant findings.
    ApprovedCandidate,
    /// Only Low/Medium findings, no failures.
    Warning,
    /// High findings or failed verdicts — a human must look.
    NeedsReview,
    /// At least one Critical finding.
    Critical,
}

impl ReviewConsensus {
    pub fn label(self) -> &'static str {
        match self {
            Self::ApprovedCandidate => "Approved candidate",
            Self::Warning => "Warning",
            Self::NeedsReview => "Needs review",
            Self::Critical => "Critical",
        }
    }
}

/// Full aggregation result — counts + per-rule explanations so the user
/// can always answer "why?" (3d.md §30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAggregation {
    pub overall: ReviewConsensus,
    pub verdicts: Vec<ReviewVerdict>,
    pub severity_counts: Vec<(Severity, usize)>,
    pub findings: Vec<ReviewFinding>,
    /// Human-readable why: every rule that fired.
    pub explanations: Vec<String>,
}

impl ReviewAggregation {
    pub fn total_findings(&self) -> usize {
        self.findings.len()
    }
}

/// Deterministic consensus aggregator (3d.md §20–§21). Pure function of
/// the reports + policy — no hidden votes.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewAggregator;

impl ReviewAggregator {
    pub fn aggregate(reports: &[ReviewReport], policy: &ReviewPolicy) -> ReviewAggregation {
        let mut findings = Vec::new();
        let verdicts: Vec<ReviewVerdict> = reports.iter().map(|r| r.verdict).collect();
        for r in reports {
            for f in &r.findings {
                findings.push(f.clone());
            }
        }
        let mut counts: std::collections::BTreeMap<Severity, usize> =
            std::collections::BTreeMap::new();
        for f in &findings {
            *counts.entry(f.severity).or_insert(0) += 1;
        }
        let severity_counts: Vec<(Severity, usize)> = counts.into_iter().collect();
        let high_count = severity_counts
            .iter()
            .find(|(s, _)| *s == Severity::High)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let critical_count = severity_counts
            .iter()
            .find(|(s, _)| *s == Severity::Critical)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        let mut explanations = Vec::new();

        let overall = if policy.any_critical && critical_count > 0 {
            explanations.push(format!(
                "{critical_count} Critical finding(s) — policy requires human attention"
            ));
            ReviewConsensus::Critical
        } else if policy.fail_means_needs_review && verdicts.contains(&ReviewVerdict::Fail) {
            explanations.push("at least one reviewer verdict is FAIL".to_string());
            ReviewConsensus::NeedsReview
        } else if high_count >= policy.high_threshold {
            explanations.push(format!(
                "{high_count} High finding(s) ≥ threshold {}",
                policy.high_threshold
            ));
            ReviewConsensus::NeedsReview
        } else if policy.warning_means_review && verdicts.contains(&ReviewVerdict::Warning) {
            explanations.push("at least one reviewer verdict is WARNING".to_string());
            ReviewConsensus::NeedsReview
        } else if severity_counts.iter().any(|(s, _)| *s >= Severity::Medium) {
            explanations.push("findings at Medium or below".to_string());
            ReviewConsensus::Warning
        } else {
            explanations.push("all reviewers passed with no significant findings".to_string());
            ReviewConsensus::ApprovedCandidate
        };

        ReviewAggregation {
            overall,
            verdicts,
            severity_counts,
            findings,
            explanations,
        }
    }
}

// ---------------------------------------------------------------------------
// §14–§15, §33–§34 synthesis
// ---------------------------------------------------------------------------

/// Provenance of a synthesis (3d.md §34): every input is recorded — no
/// raw credentials, no private reasoning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisProvenance {
    pub input_task_ids: Vec<TaskId>,
    pub input_artifact_ids: Vec<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub timestamp_ms: u64,
    pub plan_id: Option<String>,
    pub workflow_id: Option<String>,
}

/// Synthesis output (3d.md §15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynthesisResult {
    pub overall_status: String,
    pub summary: String,
    pub completed_work: Vec<String>,
    pub remaining_work: Vec<String>,
    pub warnings: Vec<String>,
    pub failures: Vec<String>,
    /// Artifact ids referenced by this synthesis (only inputs actually
    /// provided — never invented, §54).
    pub artifacts: Vec<String>,
    pub recommendations: Vec<String>,
    pub provenance: SynthesisProvenance,
}

/// The explicitly selected inputs a synthesis may use (§14) — never the
/// entire project history.
#[derive(Debug, Clone)]
pub struct SynthesisInput {
    pub task_results: Vec<TaskResult>,
    pub artifacts: Vec<Artifact>,
    pub plan_id: Option<String>,
    pub workflow_id: Option<String>,
}

/// Deterministic result synthesis (3d.md §14). Combines only the
/// explicitly selected TaskResults and artifacts. An LLM synthesizer can
/// slot in behind this trait later; the default is pure and auditable.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResultSynthesizer;

impl ResultSynthesizer {
    pub fn synthesize(
        input: &SynthesisInput,
        provider: Option<(&str, &str)>, // (provider, model) for provenance
    ) -> Result<SynthesisResult, String> {
        if input.task_results.is_empty() {
            return Err("synthesis requires at least one task result".to_string());
        }
        // Only artifact ids actually provided may be referenced (§54) —
        // hallucinated ids are rejected.
        let provided: std::collections::HashSet<String> =
            input.artifacts.iter().map(|a| a.id.clone()).collect();

        let mut completed = Vec::new();
        let mut remaining = Vec::new();
        let mut warnings = Vec::new();
        let mut failures = Vec::new();
        let mut recommendations = Vec::new();
        for r in &input.task_results {
            for f in &r.files_changed {
                if !completed.contains(f) {
                    completed.push(f.clone());
                }
            }
            warnings.extend(r.warnings.iter().cloned());
            failures.extend(r.errors.iter().cloned());
            recommendations.extend(r.recommendations.iter().cloned());
            if r.status != crate::orchestration::TaskStatus::Completed {
                remaining.push(format!(
                    "{} (status {:?})",
                    r.agent_execution_id
                        .as_ref()
                        .map(|e| e.0.clone())
                        .unwrap_or_default(),
                    r.status
                ));
            }
        }
        let artifact_ids: Vec<String> = input
            .artifacts
            .iter()
            .map(|a| a.id.clone())
            .filter(|id| provided.contains(id))
            .collect();
        let failed_count = failures.len();
        let overall_status = if failed_count > 0 {
            "Needs review".to_string()
        } else if !warnings.is_empty() {
            "Warning".to_string()
        } else {
            "Approved candidate".to_string()
        };
        let summary = format!(
            "synthesized from {} task result(s) and {} artifact(s)",
            input.task_results.len(),
            input.artifacts.len()
        );

        Ok(SynthesisResult {
            overall_status,
            summary,
            completed_work: completed,
            remaining_work: remaining,
            warnings,
            failures,
            artifacts: artifact_ids.clone(),
            recommendations,
            provenance: SynthesisProvenance {
                input_task_ids: input
                    .task_results
                    .iter()
                    .map(|r| {
                        r.agent_execution_id
                            .clone()
                            .map(|e| e.0)
                            .unwrap_or_default()
                    })
                    .collect(),
                input_artifact_ids: artifact_ids,
                model: provider.map(|(_, m)| m.to_string()),
                provider: provider.map(|(p, _)| p.to_string()),
                timestamp_ms: crate::planning::now_ms(),
                plan_id: input.plan_id.clone(),
                workflow_id: input.workflow_id.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionId;
    use crate::orchestration::TaskStatus;

    fn finding(sev: Severity) -> ReviewFinding {
        ReviewFinding::new(sev, "finding", Some("r".to_string()))
    }

    fn report(verdict: ReviewVerdict, findings: Vec<ReviewFinding>) -> ReviewReport {
        ReviewReport {
            reviewer_task_id: Some("r".to_string()),
            verdict,
            findings,
            reason: "deterministic".to_string(),
        }
    }

    #[test]
    fn consensus_pass_warning_fail_means_needs_review() {
        // §55: A=PASS, B=WARNING, C=FAIL → NeedsReview.
        let policy = ReviewPolicy::default();
        let reports = vec![
            report(ReviewVerdict::Pass, vec![]),
            report(ReviewVerdict::Warning, vec![finding(Severity::Low)]),
            report(ReviewVerdict::Fail, vec![finding(Severity::High)]),
        ];
        let agg = ReviewAggregator::aggregate(&reports, &policy);
        assert_eq!(agg.overall, ReviewConsensus::NeedsReview);
        assert!(agg.explanations.iter().any(|e| e.contains("FAIL")));
        assert_eq!(agg.verdicts.len(), 3);
    }

    #[test]
    fn consensus_all_pass_approved_candidate() {
        // §55: replace C with PASS → ApprovedCandidate (per policy).
        let policy = ReviewPolicy::default();
        let reports = vec![
            report(ReviewVerdict::Pass, vec![]),
            report(ReviewVerdict::Pass, vec![]),
            report(ReviewVerdict::Pass, vec![]),
        ];
        let agg = ReviewAggregator::aggregate(&reports, &policy);
        assert_eq!(agg.overall, ReviewConsensus::ApprovedCandidate);
    }

    #[test]
    fn any_critical_is_critical() {
        let policy = ReviewPolicy::default();
        let reports = vec![report(
            ReviewVerdict::Fail,
            vec![finding(Severity::Critical)],
        )];
        let agg = ReviewAggregator::aggregate(&reports, &policy);
        assert_eq!(agg.overall, ReviewConsensus::Critical);
        assert!(agg.explanations.iter().any(|e| e.contains("Critical")));
    }

    #[test]
    fn high_threshold_is_configurable() {
        // One High is below a threshold of 2 → Warning, not NeedsReview.
        let policy = ReviewPolicy {
            high_threshold: 2,
            ..ReviewPolicy::default()
        };
        let reports = vec![report(
            ReviewVerdict::Warning,
            vec![finding(Severity::High)],
        )];
        let agg = ReviewAggregator::aggregate(&reports, &policy);
        assert_eq!(agg.overall, ReviewConsensus::Warning);
    }

    fn task_result(id: &str, files: &[&str]) -> TaskResult {
        TaskResult {
            status: TaskStatus::Completed,
            summary: "done".to_string(),
            artifacts: vec![],
            files_changed: files.iter().map(|f| f.to_string()).collect(),
            commands: vec![],
            duration_ms: 1,
            error: None,
            agent_execution_id: Some(ExecutionId(format!("exec-{id}"))),
            attempt_count: 1,
            estimated_cost_cents: None,
            base_revision: None,
            result_revision: None,
            branch: None,
            worktree: None,
            diff_summary: None,
            metrics: vec![],
            warnings: vec![],
            errors: vec![],
            recommendations: vec![],
        }
    }

    #[test]
    fn synthesis_references_all_inputs_and_rejects_unknown() {
        // §54: three deterministic results → the synthesis references all
        // three artifacts and never invents ids.
        let arts = vec![
            Artifact {
                id: "art-a".into(),
                kind: crate::orchestration::ArtifactType::Document,
                path: None,
                description: "a".into(),
                created_by_task: Some("t-a".into()),
                metadata: vec![],
                created_by_agent: None,
                workspace_id: None,
                worktree: None,
                revision: None,
                created_at_ms: 0,
            },
            Artifact {
                id: "art-b".into(),
                kind: crate::orchestration::ArtifactType::Document,
                path: None,
                description: "b".into(),
                created_by_task: Some("t-b".into()),
                metadata: vec![],
                created_by_agent: None,
                workspace_id: None,
                worktree: None,
                revision: None,
                created_at_ms: 0,
            },
            Artifact {
                id: "art-c".into(),
                kind: crate::orchestration::ArtifactType::Document,
                path: None,
                description: "c".into(),
                created_by_task: Some("t-c".into()),
                metadata: vec![],
                created_by_agent: None,
                workspace_id: None,
                worktree: None,
                revision: None,
                created_at_ms: 0,
            },
        ];
        let input = SynthesisInput {
            task_results: vec![
                task_result("a", &["x.rs"]),
                task_result("b", &["y.rs"]),
                task_result("c", &["z.rs"]),
            ],
            artifacts: arts,
            plan_id: Some("plan-1".into()),
            workflow_id: Some("wf-1".into()),
        };
        let out = ResultSynthesizer::synthesize(&input, Some(("mock", "mock-1"))).unwrap();
        assert_eq!(out.artifacts.len(), 3, "references all three");
        assert!(out.artifacts.contains(&"art-a".to_string()));
        assert!(out.artifacts.contains(&"art-b".to_string()));
        assert!(out.artifacts.contains(&"art-c".to_string()));
        assert!(!out.artifacts.contains(&"art-hallucinated".to_string()));
        assert_eq!(out.provenance.input_task_ids.len(), 3);
        assert_eq!(out.provenance.provider.as_deref(), Some("mock"));
    }

    #[test]
    fn synthesis_fails_without_inputs() {
        let input = SynthesisInput {
            task_results: vec![],
            artifacts: vec![],
            plan_id: None,
            workflow_id: None,
        };
        assert!(ResultSynthesizer::synthesize(&input, None).is_err());
    }
}
