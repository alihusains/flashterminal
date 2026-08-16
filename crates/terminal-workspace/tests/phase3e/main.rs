//! Phase 3E regression suite (3e.md §44–§46).
//!
//! Adaptive orchestration + controlled replanning: trigger detection
//! (failure/review/budget/merge/artifact), replan creation, validation,
//! diff, approval, rejection, editing, task/artifact invalidation, plan
//! version history, loop protection, human escalation, persistence, and
//! malicious-planner rejection. Everything is deterministic: disposable git
//! repos, the `fake-agent` binary (skipped when not built), and a mock
//! planner provider — no LLM required in CI.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use terminal_session::adaptive::{AutonomyPolicy, ReplanLimits, ReplanTrigger};
use terminal_session::collaboration::{ReviewFinding, ReviewReport, ReviewVerdict, Severity};
use terminal_session::orchestration::{Task, TaskStatus};
use terminal_session::planning::{
    parse_plan_response, PlannerConfig, PlannerError, PlannerProvider, PlannerRequest, ProposedPlan,
};
use terminal_workspace::engine::Multiplexer;
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;

// ---------------------------------------------------------------------------
// helpers (mirrors phase3b/3d)
// ---------------------------------------------------------------------------

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

fn engine_in_repo(name: &str) -> (Multiplexer, String) {
    let dir = std::env::temp_dir().join(format!("ft-3e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir.to_string_lossy(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("base.txt"), "base content\n").unwrap();
    git(&dir.to_string_lossy(), &["add", "."]);
    git(
        &dir.to_string_lossy(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("3e-ws", &dir.to_string_lossy()).unwrap();
    (m, dir.to_string_lossy().to_string())
}

fn git(repo: &str, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {repo}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ws_id(m: &Multiplexer) -> String {
    m.workspaces()[0].id.clone()
}

fn create_task(
    m: &mut Multiplexer,
    title: &str,
    scenario: &str,
    args: &[&str],
    deps: &[&str],
) -> String {
    let id = m
        .task_create(&ws_id(m), title, "", "fake-agent", &[], false)
        .unwrap();
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), scenario.to_string())],
    )
    .unwrap();
    m.task_add_arguments(&id, args).unwrap();
    for d in deps {
        m.task_add_dependency(&id, &d.to_string()).unwrap();
    }
    id
}

fn agent_done(t: &Task) -> bool {
    t.status == TaskStatus::NeedsReview || t.status.is_terminal() || t.status == TaskStatus::Blocked
}

fn drain_until_done(m: &mut Multiplexer, ids: &[String], timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        let done = ids
            .iter()
            .all(|id| m.task_get(id).map(agent_done).unwrap_or(false));
        if done {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// ---------------------------------------------------------------------------
// mock planner (deterministic; parses real plan JSON)
// ---------------------------------------------------------------------------

struct MockPlannerProvider {
    responses: Mutex<VecDeque<String>>,
}

impl MockPlannerProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn plan(goal: &str, steps: &[(&str, &str, &str, &[&str])]) -> String {
        Self::plan_with_cost(goal, steps, None)
    }

    fn plan_with_cost(
        goal: &str,
        steps: &[(&str, &str, &str, &[&str])],
        estimated_cost_cents: Option<u64>,
    ) -> String {
        // (id, title, agent, deps)
        let mut tasks = String::new();
        for (i, (id, title, agent, deps)) in steps.iter().enumerate() {
            if i > 0 {
                tasks.push(',');
            }
            let deps: Vec<String> = deps.iter().map(|d| format!("\"{d}\"")).collect();
            tasks.push_str(&format!(
                r#"{{"id":"{id}","title":"{title}","description":"fixture","agent":"{agent}","depends_on":[{}]}}"#,
                deps.join(",")
            ));
        }
        let cost = estimated_cost_cents
            .map(|c| format!(",\"estimated_cost_cents\":{c}"))
            .unwrap_or_default();
        format!(r#"{{"goal":"{goal}","tasks":[{tasks}]{cost}}}"#)
    }
}

impl PlannerProvider for MockPlannerProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn generate(
        &self,
        _request: &PlannerRequest,
        _config: &PlannerConfig,
    ) -> Result<ProposedPlan, PlannerError> {
        let Some(raw) = self.responses.lock().unwrap().pop_front() else {
            return Err(PlannerError::InvalidResponse {
                message: "mock exhausted".to_string(),
            });
        };
        if let Some(kind) = raw.strip_prefix("ERR:") {
            return Err(match kind {
                "network" => PlannerError::Network {
                    message: "mock network failure".to_string(),
                },
                _ => PlannerError::InvalidResponse {
                    message: format!("mock error {kind}"),
                },
            });
        }
        parse_plan_response(&raw)
    }
}

// ---------------------------------------------------------------------------
// §26 adaptive workflow fixture: Research → Implementation → Tests (fails)
// ---------------------------------------------------------------------------

#[test]
fn failing_tests_emit_signal_and_proposed_replan() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("adaptive-flow");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "Investigate and fix",
            &[
                (
                    "investigate",
                    "Investigate failing tests",
                    "fake-agent",
                    &[],
                ),
                ("fix", "Fix the failures", "fake-agent", &["investigate"]),
                ("rerun", "Re-run tests", "fake-agent", &["fix"]),
            ],
        ),
    ])));
    let r = create_task(&mut m, "Research", "completion", &[], &[]);
    let i = create_task(&mut m, "Implementation", "completion", &[], &[r.as_str()]);
    let t = create_task(&mut m, "Tests", "tests-failed", &[], &[i.as_str()]);
    m.task_run();
    // Producers land in NeedsReview (isolated tasks always do) — approve
    // them in dependency order so the gate releases the next task (§9).
    assert!(drain_until_done(&mut m, std::slice::from_ref(&r), 60_000));
    m.resolve_task_review(&r, true).unwrap();
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&i), 60_000));
    m.resolve_task_review(&i, true).unwrap();
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));

    // §7: test failure → Replan candidate (TestsFailed trigger).
    m.drain_frame();
    let signals = m.adaptive_signals();
    assert!(
        signals
            .iter()
            .any(|s| s.trigger == ReplanTrigger::TestsFailed),
        "tests-failed trigger emitted: {signals:?}"
    );

    // §45/§12: the replan proposes Investigate → Fix → Re-run; it awaits
    // approval and is versioned (v1 → v2 with a diff).
    let id = m
        .replan_workflow("investigate failing tests and fix")
        .unwrap();
    assert_eq!(
        m.planner_status().phase,
        terminal_session::planning::PlannerPhase::NeedsApproval
    );
    let proposals = m.replan_list();
    assert!(proposals.iter().any(|p| p.id == id));
    let history = m.workflow_history();
    assert!(!history.is_empty());
    // §14: the new plan differs from the original (added steps).
    let latest = history.last().unwrap();
    if let Some(diff) = &latest.diff_from_previous {
        assert!(!diff.added.is_empty() || !diff.modified.is_empty());
    }
}

// ---------------------------------------------------------------------------
// §27 critical finding fixture
// ---------------------------------------------------------------------------

#[test]
fn critical_review_finding_emits_critical_signal() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("critical");
    let i = create_task(&mut m, "Implementation", "completion", &[], &[]);
    let s = create_task(&mut m, "Security Review", "completion", &[], &[i.as_str()]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&i), 60_000));
    // Approve the producer so Security Review can run (§9).
    m.resolve_task_review(&i, true).unwrap();
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&s), 60_000));

    // Security returns a Critical finding (§27).
    m.record_review_report(
        &s,
        &ReviewReport {
            reviewer_task_id: Some("sec".into()),
            verdict: ReviewVerdict::Fail,
            findings: vec![ReviewFinding::new(Severity::Critical, "auth bypass", None)],
            reason: "critical".into(),
        },
    );
    m.drain_frame();
    let signals = m.adaptive_signals();
    let crit = signals
        .iter()
        .find(|s| s.trigger == ReplanTrigger::CriticalReviewFinding);
    assert!(crit.is_some(), "critical trigger emitted: {signals:?}");
    assert_eq!(
        crit.unwrap().severity,
        terminal_session::adaptive::ReplanSeverity::Critical
    );
}

// ---------------------------------------------------------------------------
// §28 replan rejection fixture
// ---------------------------------------------------------------------------

#[test]
fn rejected_replan_leaves_workflow_intact() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("reject");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("bad plan", &[("x", "Something wrong", "fake-agent", &[])]),
    ])));
    let a = create_task(&mut m, "A", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));

    let id = m.replan_workflow("replan").unwrap();
    // §28: user rejects the bad replan.
    m.replan_reject(&id, "not needed").unwrap();
    assert!(m.replan_list().is_empty(), "rejected proposal removed");
    // Original workflow intact: no new tasks, completed task still there.
    assert_eq!(m.task_get(&a).unwrap().status, TaskStatus::NeedsReview);
    assert!(m.workflow_history().iter().all(|v| !v.approved));
}

// ---------------------------------------------------------------------------
// §29 replan edit fixture
// ---------------------------------------------------------------------------

#[test]
fn replan_edit_requires_revalidation() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("edit");
    // Parallelism default is 2 — the 3-task fixture needs 3 slots.
    let mut policy = m.task_policy();
    policy.max_parallel_tasks = 3;
    m.set_task_policy(policy);
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "proposal",
            &[
                ("a", "Task A", "fake-agent", &[]),
                ("b", "Task B", "fake-agent", &[]),
                ("c", "Task C", "fake-agent", &[]),
            ],
        ),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));

    let id = m.replan_workflow("replan").unwrap();
    // §29: user edits — remove B, change C's agent to a bogus one.
    let changes = vec![
        terminal_session::planning::PlanEditChange::RemoveStep {
            step_id: "b".to_string(),
        },
        terminal_session::planning::PlanEditChange::SetAgent {
            step_id: "c".to_string(),
            agent: "nonexistent-agent".to_string(),
        },
    ];
    // The edit itself applies (locally); revalidation must fail because the
    // agent is unavailable.
    let edit_result = m.replan_edit(&id, &changes);
    assert!(
        edit_result.is_err(),
        "edited plan with unavailable agent must fail validation (§16)"
    );
}

// ---------------------------------------------------------------------------
// §19–§20 invalidation
// ---------------------------------------------------------------------------

#[test]
fn task_invalidation_requires_approval_and_preserves_artifact() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("invalidate");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &["--write-file", "keep.txt", "--set-content", "kept"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    let art = m
        .artifact_list()
        .into_iter()
        .find(|r| r.artifact.path.as_deref() == Some("keep.txt"))
        .expect("A's artifact");

    // Un-approved invalidation records but does not revert the task.
    m.invalidate_task(
        &a,
        "assumption changed",
        vec!["evidence: new data".into()],
        false,
    )
    .unwrap();
    assert_eq!(m.task_get(&a).unwrap().status, TaskStatus::NeedsReview);
    assert!(!m.task_invalidations()[0].approved);

    // Approved invalidation reverts the task with the reason (§19).
    m.invalidate_task(
        &a,
        "assumption changed",
        vec!["evidence: new data".into()],
        true,
    )
    .unwrap();
    let status = m.task_get(&a).unwrap().status;
    assert!(
        status == TaskStatus::Failed || status == TaskStatus::Cancelled,
        "approved invalidation reverts the task: {status:?}"
    );
    assert!(m
        .task_get(&a)
        .unwrap()
        .error
        .as_ref()
        .map(|e| e.message.contains("invalidated"))
        .unwrap_or(false));

    // §20: artifact invalidation preserves the old record for lineage.
    m.invalidate_artifact(&art.artifact.id, "superseded", vec!["new design".into()])
        .unwrap();
    assert!(
        m.artifact_get(&art.artifact.id).is_some(),
        "artifact preserved"
    );
    assert!(!m.artifact_invalidations().is_empty());
}

// ---------------------------------------------------------------------------
// §30–§32 history, loop protection, human escalation
// ---------------------------------------------------------------------------

#[test]
fn replan_limit_blocks_and_escalates() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("loop");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p1", &[("r1", "Retry", "fake-agent", &[])]),
        MockPlannerProvider::plan("p2", &[("r2", "Retry", "fake-agent", &[])]),
    ])));
    m.set_adaptive_policy(
        ReplanLimits {
            max_replans: 1,
            replan_cooldown_seconds: 1,
        },
        AutonomyPolicy::Manual,
    );
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));

    // First replan goes through.
    assert!(m.replan_workflow("replan 1").is_ok());
    // Second replan: limit reached → error + human escalation.
    let err = m.replan_workflow("replan 2").unwrap_err();
    assert!(
        err.to_string().contains("replan limit"),
        "loop protection: {err}"
    );
    assert!(m.replan_limits().1, "limit_reached flag set");
    assert!(
        !m.workflow_interventions().is_empty(),
        "human escalation recorded"
    );
}

// ---------------------------------------------------------------------------
// §23 budget risk trigger
// ---------------------------------------------------------------------------

#[test]
fn budget_risk_emits_signal() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("budget");
    // Small budget so spent ≥ budget triggers immediately. The agent has
    // no pricing estimate, so spent stays 0 — drive the trigger through
    // the deterministic evaluator by forcing spent ≥ budget via a failed
    // task whose estimated cost is counted (see `budget_observation`).
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(0); // any spend ≥ 0 triggers
    m.set_task_policy(policy);
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    m.drain_frame();
    let signals = m.adaptive_signals();
    // The failed task itself always emits TaskFailure; the budget rule
    // fires when spent ≥ budget (budget 0 → any spent).
    assert!(
        signals
            .iter()
            .any(|s| s.trigger == ReplanTrigger::BudgetRisk),
        "budget risk trigger emitted: {signals:?}"
    );
}

// ---------------------------------------------------------------------------
// §42 persistence
// ---------------------------------------------------------------------------

#[test]
fn adaptive_state_survives_restart() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("persist");
    m.signal_replan("test_cause", "test detail");
    m.invalidate_task(&"fake-id".to_string(), "reason", vec![], false)
        .unwrap_err(); // unknown task → err, fine
    let a = create_task(&mut m, "A", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    m.record_review_report(
        &a,
        &ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Warning,
            findings: vec![ReviewFinding::new(Severity::High, "check", None)],
            reason: "deterministic".into(),
        },
    );
    m.drain_frame();

    let state = m.snapshot_state();
    let persisted = state.adaptive.clone().expect("adaptive state persisted");
    assert!(persisted
        .signals
        .iter()
        .any(|s| s.reason.contains("test_cause")));

    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);
    assert!(
        m2.adaptive_signals()
            .iter()
            .any(|s| s.reason.contains("test_cause")),
        "signals survive restart"
    );
    assert!(m2.task_review_consensus(&a).is_some());
}

// ---------------------------------------------------------------------------
// §46 malicious planner fixtures (security)
// ---------------------------------------------------------------------------

#[test]
fn planner_cannot_raise_its_own_limits_or_bypass_approval() {
    let (mut m, _repo) = engine_in_repo("security");
    // The autonomy policy is deterministic config — the planner provider
    // cannot change it (there is no such API), and Automatic is disabled.
    m.set_adaptive_policy(ReplanLimits::default(), AutonomyPolicy::Manual);
    assert_eq!(m.autonomy_policy(), AutonomyPolicy::Manual);
    // The planner cannot increase max_agents/budget — the scheduler policy
    // is set by the engine only; a malicious provider output exceeding the
    // budget is rejected by the validator.
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(1);
    m.set_task_policy(policy);
    let bad = MockPlannerProvider::plan_with_cost(
        "expensive",
        &[("x", "Spend everything", "fake-agent", &[])],
        Some(10_000), // $100 > $0.01 budget
    );
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![bad])));
    // The plan pipeline rejects the overspend before approval (§21):
    // `plan_request` validates the provider output against the engine's
    // constraints. A malicious planner can neither bypass the budget nor
    // raise the limit itself.
    let err = m.plan_request("build an expensive workflow").unwrap_err();
    assert!(
        err.to_string().contains("budget") || err.to_string().contains("cost"),
        "overspend plan rejected: {err}"
    );
}

// ---------------------------------------------------------------------------
// §44 merge conflict trigger
// ---------------------------------------------------------------------------

#[test]
fn merge_conflict_generates_replan_signal() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("merge-conflict");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &["--write-file", "base.txt", "--set-content", "A change"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    m.resolve_task_review(&a, true).unwrap();
    // Now modify main to conflict with A's change.
    std::fs::write(
        std::path::Path::new(&repo).join("base.txt"),
        "main change\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "main change",
        ],
    );
    let wt = m
        .worktree_list()
        .iter()
        .find(|r| r.task_id.as_deref() == Some(a.as_str()))
        .expect("A's worktree")
        .id
        .clone();
    let outcome = m.worktree_merge(&wt, "main").unwrap();
    assert!(
        matches!(
            outcome,
            terminal_session::worktrees::MergeOutcome::Conflict(_)
        ),
        "conflicting merge surfaces as Conflict"
    );
    m.drain_frame();
    let signals = m.adaptive_signals();
    assert!(
        signals
            .iter()
            .any(|s| s.trigger == ReplanTrigger::MergeConflict),
        "merge conflict trigger emitted: {signals:?}"
    );
}

// ---------------------------------------------------------------------------
// §31 no infinite replan loops — repeated failing replans escalate
// ---------------------------------------------------------------------------

#[test]
fn repeated_failure_without_replan_escalates_once() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("escalate");
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    // Many frames: the evaluator keeps seeing the failure, but dedup +
    // cooldown coalesce it — never 100 identical signals.
    for _ in 0..20 {
        m.drain_frame();
    }
    let count = m
        .adaptive_signals()
        .iter()
        .filter(|s| s.trigger == ReplanTrigger::TaskFailure)
        .count();
    assert!(count <= 2, "coalesced signals, got {count}");
}
