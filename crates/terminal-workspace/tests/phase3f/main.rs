//! Phase 3F regression suite (phases/3f.md §15–§37).
//!
//! Trust & safety (§15–§22), crash & recovery (§23–§27), auditability
//! (§28–§30), human control UX (§31–§34) and security review (§35–§37).
//! Everything is deterministic: disposable git repos, the `fake-agent`
//! binary (skipped when not built), and mock planner providers — no LLM
//! required in CI.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use terminal_session::adaptive::{AutonomyPolicy, ReplanLimits, ReplanTrigger};
use terminal_session::orchestration::{Task, TaskStatus};
use terminal_session::planning::{
    parse_plan_response, PlannerConfig, PlannerError, PlannerPhase, PlannerProvider,
    PlannerRequest, ProposedPlan,
};
use terminal_session::redact::Redactor;
use terminal_workspace::engine::{AttentionAgent, AttentionReplan, AttentionTask, Multiplexer};
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;

// ---------------------------------------------------------------------------
// helpers (mirrors phase3b/3d/3e)
// ---------------------------------------------------------------------------

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

fn engine_in_repo(name: &str) -> (Multiplexer, String) {
    let dir = std::env::temp_dir().join(format!("ft-3f-{name}-{}", std::process::id()));
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
    m.create_workspace("3f-ws", &dir.to_string_lossy()).unwrap();
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

/// Deadline-poll until `cond` holds (assertion on timeout). Macro so the
/// condition can borrow `m` between drains.
macro_rules! poll_until {
    ($m:expr, $cond:expr, $what:expr) => {{
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let _ = $m.drain_frame();
            if $cond {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for: {}",
                $what
            );
            std::thread::sleep(Duration::from_millis(30));
        }
    }};
}

fn running_count(m: &Multiplexer) -> usize {
    m.task_list()
        .iter()
        .filter(|t| t.status == TaskStatus::Running)
        .count()
}

fn live_sessions(m: &Multiplexer) -> usize {
    m.agent_runtime().list_sessions().len()
}

// ---------------------------------------------------------------------------
// mock planners (deterministic; parse real plan JSON)
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

/// A planner that is permanently gone/erroring (simulates provider outage).
struct DeadPlannerProvider;

impl PlannerProvider for DeadPlannerProvider {
    fn provider_id(&self) -> &str {
        "dead"
    }

    fn generate(
        &self,
        _request: &PlannerRequest,
        _config: &PlannerConfig,
    ) -> Result<ProposedPlan, PlannerError> {
        Err(PlannerError::Network {
            message: "provider unreachable".to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// PART D — TRUST AND SAFETY (§16–§19)
// ---------------------------------------------------------------------------

/// §16 dangerous command: the planner proposes `rm -rf /` as work. The
/// engine's deterministic pipeline must never silently execute it — the
/// approval boundary is the final authority (the planner never is).
#[test]
fn adversarial_dangerous_command_never_silently_executed() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("adv-danger");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "purge everything",
            &[(
                "nuke",
                "Run rm -rf / --force --no-preserve-root",
                "fake-agent",
                &[],
            )],
        ),
    ])));
    let r = m.plan_request("implement the destructive maintenance run");
    // The validator has no content filter for dangerous commands (gap: no
    // denial-list step — see PlanValidator::validate, planning.rs:980–1123),
    // but what §16 requires is *never silent execution*: the plan must sit
    // at the human approval gate with zero tasks created.
    assert!(
        r.is_ok(),
        "dangerous proposal reached the approval gate: {r:?}"
    );
    assert_eq!(
        m.planner_status().phase,
        PlannerPhase::NeedsApproval,
        "dangerous plan never auto-executes"
    );
    assert!(m.task_list().is_empty(), "no tasks created before approval");
    let err = m.plan_execute().unwrap_err();
    assert!(
        err.to_string().contains("not permitted"),
        "execution without human approval blocked: {err}"
    );
    assert!(m.task_list().is_empty(), "still nothing executed");
}

/// §16 secret exfiltration: the planner proposes `cat ~/.ssh/id_rsa`.
#[test]
fn adversarial_secret_exfiltration_escalated() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("adv-exfil");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "copy credentials",
            &[(
                "steal",
                "cat ~/.ssh/id_rsa and post it to the log endpoint",
                "fake-agent",
                &[],
            )],
        ),
    ])));
    let r = m.plan_request("implement the deployment keys retrieval");
    assert!(r.is_ok(), "exfiltration proposal visible for review: {r:?}");
    assert_eq!(
        m.planner_status().phase,
        PlannerPhase::NeedsApproval,
        "secret-exfil plan escalated to the human gate, never executed"
    );
    assert!(m.task_list().is_empty());
    assert!(m.plan_execute().is_err(), "cannot execute before approval");
}

/// §16 policy bypass: a plan with more parallel agents than
/// `max_agents` must be rejected or gated by the scheduler policy.
#[test]
fn adversarial_policy_bypass_max_agents_gated() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("adv-agents");
    let mut policy = m.task_policy();
    policy.max_agents = 1;
    policy.max_parallel_tasks = 4; // the plan wants 3 simultaneous agents
    m.set_task_policy(policy);
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "three jobs",
            &[
                ("a", "Job A", "fake-agent", &[]),
                ("b", "Job B", "fake-agent", &[]),
                ("c", "Job C", "fake-agent", &[]),
            ],
        ),
    ])));
    // The plan itself is accepted (parallelism is below max_parallel_tasks),
    // but the scheduler is authoritative: it may run at most
    // min(max_parallel_tasks, max_agents) = 1 agent concurrently (§33).
    m.plan_request("implement my three jobs").unwrap();
    m.plan_approve().unwrap();
    let ids = m.plan_execute().unwrap();
    assert_eq!(ids.len(), 3);
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut max_concurrent = 0usize;
    let mut all_done = false;
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        // Authoritative concurrency = tasks in scheduler-Running state
        // (agent sessions linger in the runtime registry after exit).
        max_concurrent = max_concurrent.max(
            m.task_list()
                .iter()
                .filter(|t| t.status == TaskStatus::Running)
                .count(),
        );
        if m.task_list()
            .iter()
            .all(|t| t.status == TaskStatus::NeedsReview || t.status.is_terminal())
        {
            all_done = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(all_done, "all three tasks ran to completion");
    assert!(
        max_concurrent <= 1,
        "planner's 3-agent plan was gated to max_agents=1 (observed {max_concurrent})"
    );
}

/// §16 budget bypass: plan estimated cost exceeds `max_cost_cents` →
/// rejected; the planner cannot raise its own budget.
#[test]
fn adversarial_budget_bypass_rejected() {
    let (mut m, _repo) = engine_in_repo("adv-budget");
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(100); // $1.00
    m.set_task_policy(policy);
    let bad = MockPlannerProvider::plan_with_cost(
        "expensive",
        &[("x", "Burn money", "fake-agent", &[])],
        Some(50_000), // $500 > $1
    );
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![bad])));
    let err = m.plan_request("build an expensive workflow").unwrap_err();
    assert!(
        err.to_string().contains("budget"),
        "overspend plan rejected: {err}"
    );
    assert!(
        m.task_list().is_empty(),
        "no tasks escaped the budget rejection"
    );
    assert_eq!(
        m.task_policy().max_cost_cents,
        Some(100),
        "the planner cannot raise its own budget — policy is engine-owned"
    );
    assert!(
        m.planner_metrics().budget_violation_count >= 1,
        "the rejection is recorded in planner metrics"
    );
}

/// §16 invalid dependency: a task depending on a nonexistent task id fails
/// plan validation (or escalates), never schedules.
#[test]
fn adversarial_invalid_dependency_rejected() {
    let (mut m, _repo) = engine_in_repo("adv-dep");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan(
            "broken",
            &[
                ("a", "Real work", "fake-agent", &[]),
                ("b", "Depends on nothing", "fake-agent", &["ghost-task"]),
            ],
        ),
    ])));
    let err = m.plan_request("implement the workflow").unwrap_err();
    assert!(
        err.to_string().contains("task"),
        "invalid dependency surfaces a validation error: {err}"
    );
    assert!(m.planner_status().phase == PlannerPhase::Failed);
    assert!(m.task_list().is_empty(), "no phantom task scheduled");
}

/// §17 replan security: an approved v1 constraint (budget) cannot silently
/// change inside an unapproved v2 proposal — the v2 is rejected because the
/// engine's own policy is the ceiling, not the planner's claim.
#[test]
fn replan_security_v2_cannot_raise_budget_silently() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("replan-sec");
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(500); // approved v1 budget: $5.00
    m.set_task_policy(policy);
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("v1", &[("x", "Fix it", "fake-agent", &[])]),
        MockPlannerProvider::plan_with_cost(
            "v2",
            &[("x", "Fix it harder", "fake-agent", &[])],
            Some(50_000), // tries to become $500 without approval
        ),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let v1 = m.replan_workflow("replan 1").unwrap();
    m.replan_approve(&v1).unwrap();
    let before = m.workflow_history().len();
    let err = m
        .replan_workflow("replan 2: spend the whole budget")
        .unwrap_err();
    assert!(
        err.to_string().contains("budget") || err.to_string().contains("cost"),
        "silent budget raise rejected: {err}"
    );
    assert_eq!(
        m.workflow_history().len(),
        before,
        "no new plan version was recorded for the rejected v2"
    );
    assert_eq!(
        m.task_policy().max_cost_cents,
        Some(500),
        "approved $5 constraint unchanged — policy only changes with explicit approval"
    );
}

/// §18 replan loop protection: repeated failures + replans eventually hit
/// `ReplanLimits.max_replans` — execution stops, human escalation fires,
/// and the loop never runs forever.
#[test]
fn replan_loop_protection_stops_at_max() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("loop-stop");
    m.set_adaptive_policy(
        ReplanLimits {
            max_replans: 2,
            replan_cooldown_seconds: 1,
        },
        AutonomyPolicy::Manual,
    );
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p1", &[("r1", "Retry", "fake-agent", &[])]),
        MockPlannerProvider::plan("p2", &[("r2", "Retry", "fake-agent", &[])]),
        MockPlannerProvider::plan("p3", &[("r3", "Retry", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));

    // Loop the replan cycle: two succeed, the third must stop at the limit.
    let mut attempts = 0;
    let mut stopped_with_limit = None;
    for _ in 0..10 {
        match m.replan_workflow(&format!("replan attempt {attempts}")) {
            Ok(_) => attempts += 1,
            Err(e) => {
                stopped_with_limit = Some(e);
                break;
            }
        }
    }
    assert_eq!(attempts, 2, "exactly max_replans=2 replans allowed");
    let err = stopped_with_limit.expect("loop terminated by the limit");
    assert!(
        err.to_string().contains("replan limit"),
        "limit error surfaced: {err}"
    );
    assert!(m.replan_limits().1, "limit_reached flag set");
    assert!(
        !m.workflow_interventions().is_empty(),
        "human escalation recorded when automation stops"
    );
}

/// §19 budget enforcement: an expensive task is gated by `max_cost_cents`
/// — execution stops (task blocks with a budget error) instead of running.
#[test]
fn budget_enforcement_blocks_exhausted_start() {
    let (mut m, _repo) = engine_in_repo("budget-stop");
    // Zero budget: any work exceeds the cap, nothing may start.
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(0);
    m.set_task_policy(policy);
    let t = create_task(&mut m, "Expensive", "completion", &[], &[]);
    m.task_run();
    poll_until!(
        m,
        m.task_get(&t)
            .map(|task| task.status == TaskStatus::Blocked)
            .unwrap_or(false),
        "task blocked by the cost budget"
    );
    let task = m.task_get(&t).unwrap();
    let msg = task
        .error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_default();
    assert!(
        msg.contains("budget"),
        "blocking reason names the budget: {msg}"
    );
    assert_eq!(running_count(&m), 0, "nothing started past the budget");
    assert_eq!(live_sessions(&m), 0, "no agent process was spawned");
}

// ---------------------------------------------------------------------------
// PART E — CRASH AND RECOVERY (§23–§27)
// ---------------------------------------------------------------------------

/// §23 kill the agent: SIGKILL the fake-agent mid-workflow → failure is
/// detected, the workflow stays coherent, artifacts are preserved, and a
/// replan signal is created where appropriate.
#[test]
fn crash_agent_killed_midworkflow_recovers() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("kill-agent");
    let t = create_task(
        &mut m,
        "Killable",
        "modify",
        &[
            "--write-file",
            "keep3f.txt",
            "--set-content",
            "kept artifact",
            "--duration",
            "600",
            "--echo",
            &format!("ft3f-kill-{}", std::process::id()),
        ],
        &[],
    );
    let mate = create_task(&mut m, "Mate", "completion", &[], &[t.as_str()]);
    m.task_run();
    // Wait until the agent wrote + committed its file inside the worktree.
    let marker = format!("ft3f-kill-{}", std::process::id());
    poll_until!(
        m,
        m.worktree_list()
            .iter()
            .filter(|r| r.task_id.as_deref() == Some(t.as_str()))
            .any(|r| Path::new(&r.path).join("keep3f.txt").exists()),
        "agent wrote its artifact before we kill it"
    );
    assert!(
        m.task_get(&t)
            .map(|task| task.status == TaskStatus::Running)
            .unwrap_or(false),
        "killable task is mid-flight"
    );
    // SIGKILL the agent's process group (matched by the unique marker arg).
    let kill = std::process::Command::new("pkill")
        .args(["-9", "-f", &marker])
        .status()
        .expect("pkill runs");
    assert!(kill.success(), "pkill found and killed the agent process");
    // Failure must be detected by the engine — never a silent hang.
    poll_until!(
        m,
        m.task_get(&t)
            .map(|task| task.status == TaskStatus::Failed)
            .unwrap_or(false),
        "killed agent surfaced as task failure"
    );
    assert!(
        m.task_get(&t).unwrap().error.is_some(),
        "visible failure error recorded"
    );
    // Workflow remains coherent: the dependent task was never corrupted.
    assert_eq!(
        m.task_get(&mate).unwrap().status,
        TaskStatus::Blocked,
        "dependent task cleanly blocked by the failed producer"
    );
    // Artifacts preserved: the worktree record + its file survive.
    let rec = m
        .worktree_list()
        .into_iter()
        .find(|r| r.task_id.as_deref() == Some(t.as_str()))
        .expect("worktree record preserved");
    assert!(
        Path::new(&rec.path).join("keep3f.txt").exists(),
        "written artifact preserved after the kill"
    );
    assert!(
        !Path::new(&repo).join("keep3f.txt").is_file(),
        "artifact stayed inside the worktree, not the repo root"
    );
    // Replan signal created where appropriate.
    m.drain_frame();
    assert!(
        m.adaptive_signals()
            .iter()
            .any(|s| s.trigger == ReplanTrigger::TaskFailure),
        "kill produced a TaskFailure replan signal"
    );
}

/// §24 kill the planner: provider gone/erroring mid-flow → workflow not
/// corrupted; engine state stays consistent and later requests fail safely.
#[test]
fn crash_planner_loss_workflow_not_corrupted() {
    let (mut m, _repo) = engine_in_repo("kill-planner");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p1", &[("x", "Work", "fake-agent", &[])]),
    ])));
    m.plan_request("design the work plan").unwrap();
    assert_eq!(m.planner_status().phase, PlannerPhase::NeedsApproval);

    // The provider dies mid-flow: subsequent requests surface the failure.
    m.set_planner_provider(Box::new(DeadPlannerProvider));
    let err = m
        .replan_workflow("replan after the provider died")
        .unwrap_err();
    assert!(
        err.to_string().contains("network") || err.to_string().contains("provider"),
        "provider failure visible: {err}"
    );
    assert_eq!(m.planner_status().phase, PlannerPhase::Failed);
    assert!(
        m.planner_last_error()
            .map(|e| e.contains("network"))
            .unwrap_or(false),
        "planner_last_error exposes the outage"
    );
    // No corruption: nothing was half-created.
    assert!(m.task_list().is_empty());
    assert!(m.workflow_history().is_empty());
    // Recovery is explicit and safe: a working provider succeeds again.
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p2", &[("y", "Work again", "fake-agent", &[])]),
    ])));
    assert!(
        m.replan_workflow("replan with a healthy provider").is_ok(),
        "engine fully usable after the planner outage"
    );
}

/// §25 kill the application: simulate a restart by snapshotting state and
/// re-seeding a fresh engine; plan versions, artifacts and pending human
/// decisions must survive re-creation.
#[test]
fn crash_app_restart_preserves_plan_and_decisions() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("restart");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("restart-plan", &[("s1", "Step one", "fake-agent", &[])]),
    ])));
    let t = create_task(
        &mut m,
        "Artifactor",
        "modify",
        &["--write-file", "keep3f.txt", "--set-content", "survives"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    m.signal_replan("restart-cause", "restart detail");
    let id = m.replan_workflow("replan pending across restart").unwrap();
    assert!(!m.workflow_history().is_empty());
    assert!(m.replan_get(&id).is_some());

    // The application dies here. Restart: fresh engine, same state.
    let state = m.snapshot_state();
    drop(m);
    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);

    // Plan versions survived (v1 with its diff metadata).
    let history = m2.workflow_history();
    assert_eq!(history.len(), 1, "plan version restored");
    assert!(history[0].superseded_by.is_none());
    // Artifacts preserved (metadata survives; payloads re-read on demand).
    assert!(
        m2.artifact_list()
            .iter()
            .any(|r| r.artifact.path.as_deref() == Some("keep3f.txt")),
        "artifact metadata restored"
    );
    // Pending decision survives: the open replan proposal is still there.
    assert!(
        m2.replan_get(&id).is_some(),
        "pending replan proposal restored"
    );
    assert!(
        m2.workflow_history().iter().all(|v| !v.approved),
        "nothing auto-approved across the restart"
    );
    // The task graph (with its NeedsReview result) was restored too.
    assert!(
        m2.task_get(&t)
            .map(|task| task.status == TaskStatus::NeedsReview)
            .unwrap_or(false),
        "task state restored from the crash"
    );
}

/// §26 provider failure: auth-failure / network / invalid-response are all
/// visible, never silently corrupt, and recovery is explicit.
#[test]
fn provider_failure_visible_and_recoverable() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("provider-fail");
    // (a) planner provider: network outage → typed, visible failure.
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        "ERR:network".to_string()
    ])));
    let err = m.plan_request("create the plan under outage").unwrap_err();
    assert!(
        err.to_string().contains("network"),
        "network failure surfaced: {err}"
    );
    assert_eq!(m.planner_status().phase, PlannerPhase::Failed);
    assert!(m.workflow_history().is_empty(), "no half-written plan");
    assert_eq!(m.task_list().len(), 0, "scheduler untouched");

    // (b) agent provider: auth failure at runtime → task fails visibly.
    let bad = create_task(&mut m, "Auth", "auth-failure", &[], &[]);
    let ok = create_task(&mut m, "Healthy", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, &[bad.clone(), ok.clone()], 60_000));
    assert_eq!(
        m.task_get(&bad).unwrap().status,
        TaskStatus::Failed,
        "auth failure detected"
    );
    assert!(m.task_get(&bad).unwrap().error.is_some(), "error visible");
    assert!(
        matches!(
            m.task_get(&ok).unwrap().status,
            TaskStatus::NeedsReview | TaskStatus::Completed
        ),
        "the healthy task was not corrupted by the other provider's failure"
    );
    // (c) invalid response → planner rejects it, budget/policy unchanged.
    let before = m.task_policy();
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        r#"{"goal":"g","tasks":[{"id":"x","title":]}"#.to_string(),
    ])));
    let err = m.plan_request("write the garbage plan").unwrap_err();
    assert!(
        err.to_string().contains("invalid") || err.to_string().contains("schema"),
        "invalid response rejected loudly: {err}"
    );
    assert_eq!(m.task_policy(), before, "policy budgets stay correct");
}

/// §27 sleep/wake: simulate machine sleep by suspending the scheduler
/// (PAUSE ALL), verifying live state stays consistent, then resume.
#[test]
fn sleep_wake_pause_resume_state_consistent() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("sleep-wake");
    let long = create_task(&mut m, "Long", "long-running", &["--duration", "600"], &[]);
    m.task_run();
    poll_until!(
        m,
        m.task_get(&long)
            .map(|t| t.status == TaskStatus::Running)
            .unwrap_or(false),
        "long-running agent active"
    );
    // "Sleep": suspend the scheduler mid-flight.
    m.set_workflow_paused(true);
    let _ = m.drain_frame();
    std::thread::sleep(Duration::from_millis(400));
    // The running agent keeps its state (honest limitation: running PTYs
    // are not force-stopped by PAUSE) — and new work stays gated.
    assert_eq!(
        m.task_get(&long).unwrap().status,
        TaskStatus::Running,
        "running state preserved across suspend"
    );
    assert!(m.workflow_summary().paused, "summary reflects the pause");
    // "Wake": resume; the engine remains coherent and schedules again.
    m.set_workflow_paused(false);
    let fresh = create_task(&mut m, "Fresh", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        std::slice::from_ref(&fresh),
        30_000
    ));
    assert!(!m.workflow_summary().paused);
    assert!(
        m.task_get(&fresh)
            .map(|t| t.status == TaskStatus::NeedsReview)
            .unwrap_or(false),
        "scheduling restored after wake"
    );
    m.stop_all(); // cleanup the still-running long agent
}

// ---------------------------------------------------------------------------
// PART F — AUDITABILITY (§28–§30)
// ---------------------------------------------------------------------------

/// §28 workflow audit trail: after a run with replans, history has ≥2
/// versions, v2 carries a diff vs v1, and human interventions exist.
#[test]
fn audit_trail_versions_diffs_interventions() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("audit-trail");
    m.set_adaptive_policy(
        ReplanLimits {
            max_replans: 2,
            replan_cooldown_seconds: 1,
        },
        AutonomyPolicy::Manual,
    );
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p1", &[("a", "Try A", "fake-agent", &[])]),
        MockPlannerProvider::plan("p2", &[("b", "Try B", "fake-agent", &[])]),
        MockPlannerProvider::plan("p3", &[("c", "Try C", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let r1 = m.replan_workflow("replan 1").unwrap();
    m.replan_approve(&r1).unwrap();
    let r2 = m.replan_workflow("replan 2").unwrap();
    m.replan_approve(&r2).unwrap();
    // Third replan: the limit stops it and escalates to a human record.
    assert!(m.replan_workflow("replan 3").is_err());

    let history = m.workflow_history();
    assert!(
        history.len() >= 2,
        "≥2 plan versions recorded: {}",
        history.len()
    );
    assert_eq!(history[0].superseded_by, Some(2), "v1 → v2 link");
    let v2 = history.last().unwrap();
    assert!(
        v2.diff_from_previous
            .as_ref()
            .map(|d| !d.added.is_empty())
            .unwrap_or(false),
        "v2 diff lists added steps"
    );
    assert!(
        !m.workflow_interventions().is_empty(),
        "replan-limit escalation recorded as an intervention"
    );
    assert_eq!(
        m.replan_metrics().replan_approval_count,
        2,
        "two approvals recorded"
    );
}

/// §29 plan versioning: v1 → replan proposal → v2, each inspectable, with
/// a diff showing added/removed/modified tasks.
#[test]
fn plan_versioning_inspectable_with_diffs() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("versioning");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("v1", &[("x", "Original", "fake-agent", &[])]),
        MockPlannerProvider::plan(
            "v2",
            &[
                ("x", "Updated approach", "fake-agent", &[]),
                ("y", "Brand new step", "fake-agent", &[]),
            ],
        ),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let p1 = m.replan_workflow("replan 1").unwrap();
    let p2 = m.replan_workflow("replan 2").unwrap();

    let plan1 = m.replan_get(&p1).expect("v1 proposal inspectable");
    let plan2 = m.replan_get(&p2).expect("v2 proposal inspectable");
    assert_eq!(plan1.plan.steps.len(), 1);
    assert_eq!(plan1.plan.steps[0].id, "x");
    assert_eq!(plan2.plan.steps.len(), 2);
    assert_eq!(plan2.plan.steps[0].id, "x");

    let history = m.workflow_history();
    assert_eq!(history.len(), 2);
    let diff = history[1].diff_from_previous.as_ref().expect("v2 diff");
    assert!(
        diff.added.iter().any(|s| s == "y"),
        "added task visible: {diff:?}"
    );
    assert!(
        diff.modified.iter().any(|s| s == "x"),
        "modified task visible: {diff:?}"
    );
    assert!(diff.removed.is_empty(), "nothing removed: {diff:?}");
    // Titles inspectable on each immutable version.
    assert!(history[0].plan.steps.iter().any(|s| s.title == "Original"));
    assert!(history[1]
        .plan
        .steps
        .iter()
        .any(|s| s.title == "Updated approach"));
}

/// §30 explain why: the replan proposal carries a reason that maps back to
/// the triggered signal (test failure evidence → the proposal a human reviews).
#[test]
fn replan_explain_why_signal_maps_to_proposal() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("explain-why");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("fix", &[("f", "Fix the failing tests", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "Tests", "tests-failed", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    m.drain_frame();
    // The signal carries the evidence a human needs (which task, what failed).
    let signals = m.adaptive_signals();
    let sig = signals
        .iter()
        .find(|s| s.trigger == ReplanTrigger::TestsFailed)
        .expect("TestsFailed signal emitted");
    assert!(
        !sig.reason.is_empty() && sig.task_id.as_deref() == Some(t.as_str()),
        "signal carries evidence: {sig:?}"
    );
    // The replan proposal carries a reason the approval UI can show, and the
    // signal is consumed (no stale queued triggers after the replan).
    let id = m.replan_workflow("fix the 14 failing tests").unwrap();
    let proposal = m.replan_get(&id).expect("proposal pending approval");
    assert!(
        proposal.reason.contains("fix the 14 failing tests"),
        "proposal reason maps to the request: {}",
        proposal.reason
    );
    assert!(
        m.adaptive_signals().is_empty(),
        "the addressed signal was consumed by the replan"
    );
    // The proposal itself is inspectable back-to-the-signal: its step set is
    // the remediation plan the human approves.
    assert!(
        proposal
            .plan
            .steps
            .iter()
            .any(|s| s.title.contains("Fix the failing tests")),
        "proposal steps offer the remediation"
    );
}

// ---------------------------------------------------------------------------
// PART G — HUMAN CONTROL UX (§31–§34)
// ---------------------------------------------------------------------------

fn attention_breakdown(items: &terminal_workspace::engine::AttentionItems) -> String {
    format!(
        "agents={} review={} replans={} total={}",
        items.agents.len(),
        items.review_tasks.len(),
        items.replans.len(),
        items.total
    )
}

/// §31 approval center: `attention_items()` aggregates agents awaiting
/// permission + review tasks + open replans; total == sum of the three.
#[test]
fn approval_center_aggregates_attention_items() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("attention");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("improve", &[("u", "Upgrade", "fake-agent", &[])]),
    ])));
    // (1) agent awaiting permission: the `approval` scenario parks at the
    // permission prompt until a human answers.
    let _ask = create_task(&mut m, "Ask permission", "approval", &[], &[]);
    // (2) review task: an isolated producer lands in NeedsReview.
    let done = create_task(&mut m, "Produced", "completion", &[], &[]);
    poll_until!(
        m,
        m.attention_items()
            .agents
            .iter()
            .any(|x: &AttentionAgent| x.state == "NeedsApproval"),
        "agent parked at the permission prompt"
    );
    // (3) open replan proposal awaiting a decision.
    let _id = m.replan_workflow("improve things").unwrap();
    // The completion task lands in review one scheduling frame after its
    // agent parks — wait for it explicitly instead of racing the poll.
    poll_until!(
        m,
        m.attention_items()
            .review_tasks
            .iter()
            .any(|r: &AttentionTask| r.task_id == done),
        "producer lands in review"
    );

    let items = m.attention_items();
    assert!(
        !items.agents.is_empty(),
        "agents listed: {}",
        attention_breakdown(&items)
    );
    assert!(
        items
            .agents
            .iter()
            .any(|a: &AttentionAgent| a.state == "NeedsApproval" && a.attention.is_some()),
        "permission reason surfaced"
    );
    assert!(
        items
            .review_tasks
            .iter()
            .any(|r: &AttentionTask| r.task_id == done),
        "review task listed"
    );
    assert!(
        !items.replans.is_empty(),
        "open replan listed: {}",
        attention_breakdown(&items)
    );
    assert!(
        items
            .replans
            .iter()
            .any(|r: &AttentionReplan| !r.reason.is_empty() && r.workflow_id == ws_id(&m)),
        "replan carries workflow + reason"
    );
    assert_eq!(
        items.total,
        items.agents.len() + items.review_tasks.len() + items.replans.len(),
        "total is the sum of the three vectors"
    );
    assert!(
        m.stop_all().agents_stopped >= 1,
        "cleanup stopped the parked agent"
    );
}

/// §32 emergency controls: STOP ALL stops agents + tasks, preserves
/// human-pending decisions, and the approval center still lists them.
#[test]
fn emergency_stop_all_preserves_human_decisions() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("stop-all");
    // Two concurrent slots: the long runner + reviewable producer must both
    // run, or the 600s agent would hold the only `max_agents` slot forever.
    let mut policy = m.task_policy();
    policy.max_agents = 4;
    policy.max_parallel_tasks = 4;
    m.set_task_policy(policy);
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("replan", &[("z", "Z", "fake-agent", &[])]),
    ])));
    let _long = create_task(
        &mut m,
        "Long runner",
        "long-running",
        &["--duration", "600"],
        &[],
    );
    let _ask = create_task(&mut m, "Ask", "approval", &[], &[]);
    m.task_run();
    let _id = m.replan_workflow("improve after stopping").unwrap();
    // review-pending task lands before the emergency control
    let done = create_task(&mut m, "Produced", "completion", &[], &[]);
    m.task_run();
    poll_until!(
        m,
        {
            let a = m.attention_items();
            a.agents
                .iter()
                .any(|x: &AttentionAgent| x.state == "NeedsApproval")
                && a.review_tasks
                    .iter()
                    .any(|r: &AttentionTask| r.task_id == done)
        },
        "live agent + review task + replan all pending"
    );

    let report = m.stop_all();
    assert!(report.agents_stopped >= 1, "agents stopped: {report:?}");
    assert!(report.tasks_stopped >= 1, "tasks stopped: {report:?}");
    assert!(
        report.preserved_decisions > 0,
        "human-pending decisions preserved: {report:?}"
    );
    // Nothing silently auto-resumed; review + replan decisions stay listed.
    let items = m.attention_items();
    assert!(
        !items.review_tasks.is_empty()
            && !items.replans.is_empty()
            && items
                .review_tasks
                .iter()
                .any(|r: &AttentionTask| r.task_id == done),
        "preserved decisions still need the human: {}",
        attention_breakdown(&items)
    );
    assert_eq!(m.task_get(&done).unwrap().status, TaskStatus::NeedsReview);
    assert!(!m.replan_list().is_empty(), "replan proposal still open");
}

/// §33 PAUSE ALL: gates new work while preserving running state; the
/// summary reflects the pause; resuming restores scheduling.
#[test]
fn pause_all_gates_new_work_resume_restores() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("pause-all");
    let long = create_task(&mut m, "Long", "long-running", &["--duration", "600"], &[]);
    m.task_run();
    poll_until!(
        m,
        m.task_get(&long)
            .map(|t| t.status == TaskStatus::Running)
            .unwrap_or(false),
        "agent running before pause"
    );
    m.set_workflow_paused(true);
    // New work is gated: the queued task never gets an agent process.
    let queued = create_task(&mut m, "Queued", "completion", &[], &[]);
    m.task_run();
    let _ = m.drain_frame();
    std::thread::sleep(Duration::from_millis(400));
    let _ = m.drain_frame();
    assert!(
        !m.task_get(&queued)
            .map(|t| t.status == TaskStatus::Running)
            .unwrap_or(false),
        "queued task did not start while paused"
    );
    assert_eq!(live_sessions(&m), 1, "only the pre-pause agent is alive");
    assert_eq!(
        m.task_get(&long).unwrap().status,
        TaskStatus::Running,
        "running state preserved across pause"
    );
    assert!(
        m.workflow_summary().paused,
        "workflow_summary().paused flips"
    );
    // Resume restores scheduling for new work.
    m.set_workflow_paused(false);
    let fresh = create_task(&mut m, "Fresh", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        std::slice::from_ref(&fresh),
        30_000
    ));
    assert!(
        m.task_get(&fresh)
            .map(|t| t.status == TaskStatus::NeedsReview)
            .unwrap_or(false),
        "scheduling restored after resume"
    );
    m.stop_all(); // cleanup the long-running agent
}

/// §34 workflow summary: counts are consistent with `task_list()` statuses
/// and the estimated cost matches what completed results actually carry.
#[test]
fn workflow_summary_mixed_run_consistent() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("summary");
    let a = create_task(&mut m, "Good A", "completion", &[], &[]);
    let bad = create_task(&mut m, "Bad", "failure", &[], &[]);
    let c = create_task(&mut m, "Good C", "completion", &[], &[]);
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        &[a.clone(), bad.clone(), c.clone()],
        60_000
    ));
    // Resolve the two reviews so Completed is reached (mixed run).
    m.resolve_task_review(&a, true).unwrap();
    m.resolve_task_review(&c, true).unwrap();
    m.drain_frame();

    let summary = m.workflow_summary();
    let statuses = m.task_list();
    let completed = statuses
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();
    let failed = statuses
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
        .count();
    let reviewing = statuses
        .iter()
        .filter(|t| t.status == TaskStatus::NeedsReview)
        .count();
    let cost_from_results: u64 = statuses
        .iter()
        .filter_map(|t| t.result.as_ref().and_then(|r| r.estimated_cost_cents))
        .sum();

    assert_eq!(summary.workflows, 1);
    assert_eq!(summary.running, 0);
    assert_eq!(
        summary.completed_today, completed,
        "completed today matches"
    );
    assert_eq!(summary.failed, failed, "failed count matches");
    assert_eq!(summary.waiting, 0);
    assert_eq!(
        summary.needs_approval,
        reviewing + m.replan_list().len(),
        "needs-approval = review tasks + open replans"
    );
    assert_eq!(
        summary.estimated_cost_cents, cost_from_results,
        "estimated cost equals the sum of what result records carry"
    );
    assert!(!summary.paused);
}

// ---------------------------------------------------------------------------
// PART H — SECURITY REVIEW (§35–§37) + APPROVAL INTEGRITY (§22)
// ---------------------------------------------------------------------------

/// §35 secret audit: with a registered secret (the runtime registers
/// resolved credentials exactly this way — agent.rs credential resolution),
/// no secret material may appear in persisted state, task results,
/// workflow history or artifact payloads.
#[test]
fn secret_audit_persisted_state_clean() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("secret-audit");
    const SECRET: &str = "SENTINEL_PRIVATE_KEY_3f7c91";
    Redactor::register_secret(SECRET);
    // (1) an agent writes artifact payload content containing the secret;
    // (2) an agent prints the secret into its output stream.
    let w = create_task(
        &mut m,
        "Writer",
        "modify",
        &["--write-file", "creds.txt", "--set-content", SECRET],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&w), 60_000));
    let t = create_task(&mut m, "Talker", "completion", &["--echo", SECRET], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));

    // Artifact payloads must be redacted at registration (§38).
    for rec in m.artifact_list() {
        if let Some(payload) = m.artifact_payload(&rec.artifact.id) {
            let text = String::from_utf8_lossy(&payload);
            assert!(
                !text.contains(SECRET),
                "artifact payload leaked the secret: {}",
                rec.artifact.path.as_deref().unwrap_or("?")
            );
        }
    }
    // No serialized engine state may carry the sentinel. Live task records
    // keep their original arguments in memory (retry fidelity) — the
    // persisted export (`snapshot_state`, which wraps the versioned task
    // export) redacts them (§52/§35), like agent launch configs do.
    let mut serialized = String::new();
    serialized.push_str(&serde_json::to_string(&m.workflow_history()).unwrap());
    serialized.push_str(&serde_json::to_string(&m.worktree_list()).unwrap());
    serialized.push_str(&serde_json::to_string(&m.artifact_list()).unwrap());
    serialized.push_str(&serde_json::to_string(&m.snapshot_state()).unwrap());
    assert!(
        !serialized.contains(SECRET),
        "persisted state contains secret material ({SECRET})"
    );
    Redactor::unregister_secret(SECRET);
}

/// §36 path traversal: `../` and absolute escape paths are refused at the
/// worktree boundary (clear error, nothing written outside); every task's
/// execution environment is confined inside the repo worktree.
#[test]
fn path_traversal_rejected_at_worktree_boundary() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("traversal");
    let up = create_task(
        &mut m,
        "Dot-dot",
        "modify",
        &["--write-file", "../escape3f.txt", "--set-content", "x"],
        &[],
    );
    let abs_target = format!("/tmp/ft3f-abs-{}-3f.txt", std::process::id());
    let absolute = create_task(
        &mut m,
        "Absolute",
        "modify",
        &["--write-file", &abs_target, "--set-content", "x"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        &[up.clone(), absolute.clone()],
        60_000
    ));

    // Both escape attempts fail cleanly (refused) — and write nothing outside.
    for id in [&up, &absolute] {
        let task = m.task_get(id).expect("task exists");
        assert_eq!(task.status, TaskStatus::Failed, "escape attempt rejected");
        assert!(task.error.is_some(), "clear error surfaced");
    }
    assert!(
        !Path::new(&repo).join("escape3f.txt").exists(),
        "no file escaped to the repo root via ../"
    );
    assert!(
        !Path::new(&abs_target).exists(),
        "absolute path outside the worktree was not written"
    );
    // The repository stayed pristine.
    assert_eq!(
        git(&repo, &["status", "--porcelain"]),
        "",
        "repo tree unchanged by escape attempts"
    );
    // Every task's execution environment is confined to the repo/worktree.
    for id in [&up, &absolute] {
        let env = m.task_environment_preview(id).expect("env preview");
        let cwd = &env.working_directory;
        assert!(
            Path::new(cwd).starts_with(&repo),
            "agent cwd {cwd} confined to the repository"
        );
        if let Some(wt) = &env.worktree_id {
            assert_ne!(cwd, &repo, "worktree cwd is never the repo root");
            assert!(m.worktree_get(wt).is_some(), "worktree registered");
        }
    }
    // Note (gap): a *write-through-symlink* vector (an agent arg that is a
    // symlink inside the worktree pointing outside) is not blocked at the
    // engine or fake-agent layer — only `..`/absolute constructs are
    // refused (fake-agent/src/main.rs modify/consume guards). No API accepts
    // a symlinked path today, so there is nothing to reject yet.
}

/// §37 command injection #1: shell metacharacters in launched arguments are
/// passed as literal single arguments — never interpreted by a shell.
#[test]
fn command_injection_arguments_literal_not_shell() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("inject-args");
    let pid = std::process::id();
    let injected_a = format!("/tmp/ft3f-inj-{pid}-a");
    let injected_b = format!("/tmp/ft3f-inj-{pid}-b");
    let args = [
        "--echo".to_string(),
        format!("safe && touch {injected_a} && echo pwned"),
        "--echo".to_string(),
        format!("; rm -rf {injected_b}; echo $HOME"),
    ];
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let t = create_task(&mut m, "Injected", "completion", &arg_refs, &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 30_000));
    let task = m.task_get(&t).unwrap();
    assert!(
        task.status == TaskStatus::NeedsReview || task.status.is_terminal(),
        "clean handling — no launch error: {:?}",
        task.error
    );
    assert!(task.error.is_none(), "no failure from shell interpretation");
    assert!(
        !Path::new(&injected_a).exists(),
        "`&&` fragment was not interpreted as shell syntax"
    );
    assert!(
        !Path::new(&injected_b).exists(),
        "`;` fragment was not interpreted as shell syntax"
    );
    assert!(
        Path::new(&repo).join("base.txt").exists(),
        "nothing in the workspace was deleted"
    );
}

/// §37 command injection #2: metacharacters in planner task titles are
/// preserved as literal text and never alter scheduling or planner output.
#[test]
fn command_injection_titles_preserved_literally() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("inject-titles");
    let pid = std::process::id();
    let marker = format!("/tmp/ft3f-title-{pid}");
    let evil = format!("Cleanup; rm -rf {marker}; && touch {marker}");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("cleanup", &[("s", &evil, "fake-agent", &[])]),
    ])));
    m.plan_request("implement the cleanup plan").unwrap();
    // The literal text is preserved in the planner output for human review.
    let plan = m.planner_status().plan.expect("plan present");
    assert_eq!(plan.steps[0].title, evil, "literal text preserved");
    // Execution validates and schedules with the literal title intact.
    m.plan_approve().unwrap();
    let ids = m.plan_execute().unwrap();
    assert_eq!(ids.len(), 1);
    assert!(drain_until_done(&mut m, &ids, 30_000));
    assert!(
        m.task_list().iter().any(|t| t.title == evil),
        "task carries the literal title — no shell expansion"
    );
    assert!(!Path::new(&marker).exists(), "no injected file created");
    assert!(Path::new(&repo).join("base.txt").exists());
}

/// §22 approval integrity #1: approving a stale/wrong replan id fails
/// safely — Err, with zero state change.
#[test]
fn approval_integrity_stale_replan_id_fails_safe() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("approve-stale");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p", &[("a", "A", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let real = m.replan_workflow("replan").unwrap();
    m.replan_approve(&real).unwrap();
    let before_history = m.workflow_history().len();
    let before_tasks = m.task_list().len();

    let err = m.replan_approve("replan:stale-0000").unwrap_err();
    assert!(
        err.to_string().contains("unknown replan"),
        "stale id rejected loudly: {err}"
    );
    assert_eq!(
        m.workflow_history().len(),
        before_history,
        "history unchanged"
    );
    assert_eq!(m.task_list().len(), before_tasks, "tasks unchanged");
    assert_eq!(
        m.replan_metrics().replan_approval_count,
        1,
        "only the real approval was counted"
    );
}

/// §22 approval integrity #2: a duplicate approval of the same id is
/// rejected — never double-applied (history holds exactly one version).
#[test]
fn approval_integrity_duplicate_approval_idempotent() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("approve-dup");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p", &[("a", "A", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let id = m.replan_workflow("replan").unwrap();
    m.replan_approve(&id).unwrap();
    let err = m.replan_approve(&id).unwrap_err();
    assert!(
        err.to_string().contains("not permitted"),
        "second approval rejected: {err}"
    );
    let history = m.workflow_history();
    assert_eq!(history.len(), 1, "exactly one plan version — never two v2s");
    assert!(history[0].approved, "the single version is approved");
    assert_eq!(
        m.replan_metrics().replan_approval_count,
        1,
        "approval applied exactly once"
    );
}

/// §22 approval integrity #3: IPC-level replay — re-approving an already
/// rejected replan id returns Err and the original plan stays intact.
#[test]
fn approval_integrity_replayed_request_after_reject_errs() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("approve-replay");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p", &[("a", "A", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let id = m.replan_workflow("replan").unwrap();
    m.replan_reject(&id, "not needed").unwrap();
    // A replayed (duplicated) approval of the now-rejected id must fail.
    let err = m.replan_approve(&id).unwrap_err();
    assert!(
        err.to_string().contains("unknown replan"),
        "replay rejected: {err}"
    );
    // Original workflow intact: the task failed state and the v1 record.
    assert_eq!(m.task_get(&t).unwrap().status, TaskStatus::Failed);
    let history = m.workflow_history();
    assert_eq!(history.len(), 1, "v1 history intact");
    assert!(!history[0].approved);
    assert_eq!(
        m.replan_metrics().replan_approval_count,
        0,
        "no approval leaked"
    );
}

/// §22 approval integrity #4: rejecting a replan leaves v1 intact, removes
/// the proposal, and records the rejection in the metrics.
#[test]
fn approval_integrity_reject_records_metrics_and_preserves_v1() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("approve-reject");
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("p", &[("a", "A", "fake-agent", &[])]),
    ])));
    let t = create_task(&mut m, "T", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&t), 60_000));
    let id = m.replan_workflow("replan").unwrap();
    m.replan_reject(&id, "the plan does not help").unwrap();

    assert!(m.replan_list().is_empty(), "rejected proposal removed");
    let history = m.workflow_history();
    assert_eq!(history.len(), 1, "original v1 preserved");
    assert!(!history[0].approved);
    assert!(
        history[0].diff_from_previous.is_none(),
        "v1 is the root of the audit trail"
    );
    assert_eq!(
        m.replan_metrics().replan_rejection_count,
        1,
        "rejection recorded in metrics"
    );
    assert!(
        m.task_get(&t).unwrap().status == TaskStatus::Failed,
        "original execution state untouched by the rejection"
    );
}
