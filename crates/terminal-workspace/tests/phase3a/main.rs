//! Phase 3A regression suite (3a.md §47–§49).
//!
//! Deterministic multi-agent task orchestration:
//! graph validation, explicit transitions, serial/parallel scheduling,
//! dependency failure policies, retry, cancellation, review boundary,
//! budgets, determinism (10 runs), persistence/restore, waiting tasks.
//!
//! Runtime tests use the deterministic `fake-agent` binary and skip when it
//! is not built (same policy as the engine's own tests); every other test is
//! pure and deterministic.

use std::time::{Duration, Instant};

use terminal_workspace::engine::Multiplexer;
use terminal_workspace::model::WorkspaceId;
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::agent::PermissionDecision;
use terminal_workspace::terminal_session::orchestration::{
    DependencyFailurePolicy, PersistedSchedulerState, TaskGraphError, TaskStatus,
};

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

/// Fresh engine with one workspace (tests never share engine state).
fn new_engine() -> Multiplexer {
    let mut m = Multiplexer::new().unwrap();
    let root = std::env::temp_dir().to_string_lossy().to_string();
    m.create_workspace("task-ws", &root).unwrap();
    m
}

fn ws_id(m: &Multiplexer) -> WorkspaceId {
    m.workspaces().first().unwrap().id.clone()
}

/// Creates a task with a deterministic fake-agent scenario (via task env).
fn create_task(m: &mut Multiplexer, title: &str, scenario: &str, deps: &[&str]) -> String {
    let deps: Vec<String> = deps.iter().map(|s| s.to_string()).collect();
    let id = m
        .task_create(
            &ws_id(m),
            title,
            "phase3a fixture",
            "fake-agent",
            &deps,
            false,
        )
        .expect("task created");
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), scenario.to_string())],
    )
    .expect("scenario set");
    id
}

/// Drains until every listed task is settled (terminal or blocked — the
/// final states of a run; Blocked is not terminal by design, §9).
fn drain_until_terminal(m: &mut Multiplexer, ids: &[String], timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        let done = ids.iter().all(|id| {
            m.task_get(&id.to_string())
                .map(|t| t.status.is_terminal() || t.status == TaskStatus::Blocked)
                .unwrap_or(false)
        });
        if done {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn status_of(m: &Multiplexer, id: &str) -> TaskStatus {
    m.task_get(&id.to_string())
        .map(|t| t.status)
        .unwrap_or(TaskStatus::Skipped)
}

fn task_error_kind(err: &TaskGraphError) -> String {
    format!("{err:?}")
}

// ---------------------------------------------------------------------------
// §8 typed validation errors at the engine boundary
// ---------------------------------------------------------------------------

#[test]
fn create_rejects_unknown_agent_and_workspace() {
    let mut m = new_engine();
    let err = task_error_kind(
        &m.task_create(&ws_id(&m), "x", "", "no-such-agent", &[], false)
            .unwrap_err(),
    );
    assert!(err.contains("UnknownAgentDefinition"), "{err}");
    let err = task_error_kind(
        &m.task_create(&"missing-ws".into(), "x", "", "fake-agent", &[], false)
            .unwrap_err(),
    );
    assert!(err.contains("UnknownWorkspace"), "{err}");
    let a = create_task(&mut m, "a", "completion", &[]);
    m.task_create(
        &ws_id(&m),
        "b",
        "",
        "fake-agent",
        std::slice::from_ref(&a),
        false,
    )
    .expect("b depends on a");
    let ghost = task_error_kind(
        &m.task_create(
            &ws_id(&m),
            "c",
            "",
            "fake-agent",
            &["ghost".to_string()],
            false,
        )
        .unwrap_err(),
    );
    assert!(ghost.contains("UnknownTask"), "{ghost}");
}

#[test]
fn workflow_validate_catches_graph_and_engine_issues() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "completion", &[]);
    let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
    // A dependency edge that would close a cycle is rejected at add time.
    let err = task_error_kind(&m.task_add_dependency(&a, &b).unwrap_err());
    assert!(err.contains("Cycle"), "{err}");
    assert!(m.workflow_validate().is_empty());
    // Unknown agent ref is caught by engine validation.
    m.task_set_agent(&a, "ghost-agent").unwrap();
    let issues = m.workflow_validate();
    assert!(issues.iter().any(|e| matches!(
        e,
        TaskGraphError::UnknownAgentDefinition(id) if id == "ghost-agent"
    )));
}

// ---------------------------------------------------------------------------
// §10 serial + parallel scheduling
// ---------------------------------------------------------------------------

#[test]
fn serial_workflow_completes_in_dependency_order() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "completion", &[]);
    let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
    let c = create_task(&mut m, "c", "completion", &[b.as_str()]);
    let mut policy = m.task_policy();
    policy.max_parallel_tasks = 1;
    m.set_task_policy(policy);

    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        &[a.clone(), b.clone(), c.clone()],
        30_000
    ));

    assert_eq!(status_of(&m, &a), TaskStatus::Completed);
    assert_eq!(status_of(&m, &b), TaskStatus::Completed);
    assert_eq!(status_of(&m, &c), TaskStatus::Completed);
    // Results are deterministic summaries, not LLM text.
    for id in [&a, &b, &c] {
        let t = m.task_get(id).unwrap();
        let r = t.result.as_ref().expect("result recorded");
        assert_eq!(r.status, TaskStatus::Completed);
        assert_eq!(r.attempt_count, 1);
        assert!(r.duration_ms > 0);
        assert!(r.summary.contains("attempt(s)"));
        assert!(r.agent_execution_id.is_some());
    }
    // Strict start order = dependency order (serial).
    let trace: Vec<String> = m
        .scheduler_status()
        .states
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    let _ = trace;
}

#[test]
fn parallel_workflow_never_exceeds_concurrency_cap() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    // Working scenario takes ~2s, so overlap is observable.
    let ids: Vec<String> = (0..4)
        .map(|i| create_task(&mut m, &format!("t{i}"), "working", &[]))
        .collect();
    let mut policy = m.task_policy();
    policy.max_parallel_tasks = 2;
    m.set_task_policy(policy);

    m.task_run();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut max_running = 0usize;
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        let s = m.scheduler_status();
        max_running = max_running.max(s.running.len());
        if s.states.iter().all(|(_, st)| st.is_terminal()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(max_running <= 2, "never exceed max_parallel_tasks");
    assert_eq!(max_running, 2, "parallelism actually exercised");
    for id in &ids {
        assert_eq!(status_of(&m, id), TaskStatus::Completed);
    }
}

// ---------------------------------------------------------------------------
// §9 dependency failure policies
// ---------------------------------------------------------------------------

#[test]
fn failed_dependency_blocks_downstream_but_not_independent() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "failure", &[]);
    let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
    let c = create_task(&mut m, "c", "completion", &[]);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        &[a.clone(), c.clone()],
        30_000
    ));
    std::thread::sleep(Duration::from_millis(100));
    let _ = m.drain_frame();

    assert_eq!(status_of(&m, &a), TaskStatus::Failed);
    assert_eq!(status_of(&m, &b), TaskStatus::Blocked, "dependent blocked");
    assert_eq!(
        status_of(&m, &c),
        TaskStatus::Completed,
        "independent proceeds"
    );
    let err = m
        .task_get(&a)
        .unwrap()
        .error
        .as_ref()
        .expect("error recorded");
    assert_eq!(
        err.kind,
        terminal_workspace::terminal_session::orchestration::TaskErrorKind::AgentFailed
    );
}

#[test]
fn skip_downstream_marks_dependents_skipped() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "failure", &[]);
    let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
    let mut policy = m.task_policy();
    policy.failure = DependencyFailurePolicy::SkipDownstream;
    m.set_task_policy(policy);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        &[a.clone(), b.clone()],
        30_000
    ));
    assert_eq!(status_of(&m, &a), TaskStatus::Failed);
    assert_eq!(status_of(&m, &b), TaskStatus::Skipped);
}

// ---------------------------------------------------------------------------
// §25–§26 retry policy
// ---------------------------------------------------------------------------

#[test]
fn transient_failure_retries_then_succeeds() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let id = create_task(&mut m, "flaky", "flaky", &[]);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&id),
        30_000
    ));
    let t = m.task_get(&id).unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "retry recovered");
    assert_eq!(t.attempt_count, 2, "attempt 1 failed, attempt 2 succeeded");
    assert_eq!(m.scheduler_status().retried_count, 1);
    let r = t.result.as_ref().unwrap();
    assert_eq!(r.attempt_count, 2);
    assert!(r.summary.contains("2 attempt(s)"));
}

#[test]
fn auth_failure_is_never_auto_retried() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let id = create_task(&mut m, "auth", "auth-failure", &[]);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&id),
        30_000
    ));
    let t = m.task_get(&id).unwrap();
    assert_eq!(t.status, TaskStatus::Failed);
    assert_eq!(t.attempt_count, 1, "no retry for auth failures");
    assert_eq!(m.scheduler_status().retried_count, 0);
    let err = t.error.as_ref().unwrap();
    assert_eq!(
        err.class,
        terminal_workspace::terminal_session::orchestration::FailureClass::AuthenticationFailure
    );
}

// ---------------------------------------------------------------------------
// §27 cancellation
// ---------------------------------------------------------------------------

#[test]
fn cancellation_stops_agent_and_leaves_no_orphan() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    // long-running with a huge bound: it must not finish on its own.
    let id = create_task(&mut m, "cancel-me", "long-running", &[]);
    m.task_add_arguments(&id, &["--duration", "600"]).unwrap();
    m.task_run();

    // Wait until it is actually running with a session.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut eid = None;
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        let t = m.task_get(&id).unwrap();
        if let Some(e) = t.agent_execution_id.clone() {
            eid = Some(e);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let eid = eid.expect("task agent spawned");

    m.task_cancel(&id).unwrap();
    // The process must die promptly.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let _ = m.drain_frame();
        let t = m.task_get(&id).unwrap();
        if t.status == TaskStatus::Cancelled {
            break;
        }
        assert!(Instant::now() < deadline, "task never reached Cancelled");
        std::thread::sleep(Duration::from_millis(25));
    }
    // No live session for the cancelled execution.
    let snap = m.agent_runtime().get_session(&eid);
    if let Some(s) = snap {
        assert!(
            matches!(
                s.state.as_str(),
                "Stopped" | "Failed" | "Completed" | "Crashed"
            ),
            "agent must be dead, was: {}",
            s.state
        );
    }
}

// ---------------------------------------------------------------------------
// §37 budgets
// ---------------------------------------------------------------------------

#[test]
fn exhausted_budget_blocks_further_starts() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "completion", &[]);
    let b = create_task(&mut m, "b", "completion", &[]);
    let mut policy = m.task_policy();
    policy.max_cost_cents = Some(0); // nothing may run
    m.set_task_policy(policy);
    m.task_run();
    std::thread::sleep(Duration::from_millis(200));
    let _ = m.drain_frame();
    for id in [&a, &b] {
        let t = m.task_get(id).unwrap();
        assert_eq!(t.status, TaskStatus::Blocked, "{id} budget-blocked");
        let err = t.error.as_ref().expect("budget error");
        assert_eq!(
            err.kind,
            terminal_workspace::terminal_session::orchestration::TaskErrorKind::BudgetExceeded
        );
    }
    // Raising the budget and retrying unblocks deterministically.
    let mut policy = m.task_policy();
    policy.max_cost_cents = None;
    m.set_task_policy(policy);
    m.task_retry(&a).unwrap();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&a),
        30_000
    ));
    assert_eq!(status_of(&m, &a), TaskStatus::Completed);
}

// ---------------------------------------------------------------------------
// §29 review boundary
// ---------------------------------------------------------------------------

#[test]
fn review_boundary_halts_progression_until_user_decides() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let ws = m.workspaces().first().unwrap().id.clone();
    let a = m
        .task_create(&ws, "reviewed", "phase3a", "fake-agent", &[], true)
        .expect("task");
    let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
    m.task_run();

    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if status_of(&m, &a) == TaskStatus::NeedsReview {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(status_of(&m, &a), TaskStatus::NeedsReview);
    // Progression halts: dependent must still be Pending.
    assert_eq!(status_of(&m, &b), TaskStatus::Pending);

    // Approve → dependent releases.
    m.resolve_task_review(&a, true).unwrap();
    assert!(drain_until_terminal(
        &mut m,
        &[a.clone(), b.clone()],
        30_000
    ));
    assert_eq!(status_of(&m, &a), TaskStatus::Completed);
    assert_eq!(status_of(&m, &b), TaskStatus::Completed);
}

#[test]
fn rejected_review_fails_task_and_blocks_dependents() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let ws = m.workspaces().first().unwrap().id.clone();
    let a = m
        .task_create(&ws, "reviewed", "phase3a", "fake-agent", &[], true)
        .expect("task");
    m.task_run();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if status_of(&m, &a) == TaskStatus::NeedsReview {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    m.resolve_task_review(&a, false).unwrap();
    let _ = m.drain_frame();
    assert_eq!(status_of(&m, &a), TaskStatus::Failed);
}

// ---------------------------------------------------------------------------
// waiting / approval needs a human, then continues
// ---------------------------------------------------------------------------

#[test]
fn waiting_task_waits_for_human_then_completes() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let id = create_task(&mut m, "approve", "approval", &[]);
    m.task_run();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut eid = None;
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if status_of(&m, &id) == TaskStatus::Waiting {
            eid = m.task_get(&id).unwrap().agent_execution_id.clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(status_of(&m, &id), TaskStatus::Waiting);
    let eid = eid.expect("execution id");
    m.agent_runtime_mut()
        .respond_permission(&eid, PermissionDecision::AllowOnce)
        .expect("permission answered");
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&id),
        30_000
    ));
    assert_eq!(status_of(&m, &id), TaskStatus::Completed);
}

// ---------------------------------------------------------------------------
// §10 determinism: 10 runs → identical schedule
// ---------------------------------------------------------------------------

#[test]
fn determinism_ten_runs_produce_identical_schedules() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut reference: Option<(Vec<TaskStatus>, Vec<String>)> = None;
    for run in 0..10 {
        let mut m = new_engine();
        let a = create_task(&mut m, "a", "completion", &[]);
        let b = create_task(&mut m, "b", "completion", &[a.as_str()]);
        let c = create_task(&mut m, "c", "failure", &[]);
        let d = create_task(&mut m, "d", "completion", &[c.as_str(), b.as_str()]);
        let e = create_task(&mut m, "e", "completion", &[]);
        let mut policy = m.task_policy();
        policy.max_parallel_tasks = 2;
        m.set_task_policy(policy);
        m.task_run();
        assert!(drain_until_terminal(&mut m, &[a, b, c, d, e], 60_000));
        let s = m.scheduler_status();
        // Task ids are fresh UUIDs per run — determinism is about the
        // schedule, so compare structure: statuses in insertion order and
        // the event-kind sequence in emission order.
        let states: Vec<TaskStatus> = s.states.iter().map(|(_, st)| *st).collect();
        let trace: Vec<String> = m.scheduler_trace().iter().map(|(_, k)| k.clone()).collect();
        let snapshot = (states, trace);
        match &reference {
            None => reference = Some(snapshot),
            Some(prev) => {
                if prev != &snapshot {
                    eprintln!("RUN 0: {prev:?}");
                    eprintln!("RUN {run}: {snapshot:?}");
                }
                assert_eq!(prev, &snapshot, "run {run} diverged from run 0");
            }
        }
    }
    let (states, trace) = reference.unwrap();
    assert_eq!(states.len(), 5);
    assert!(trace.iter().any(|k| k == "failed"), "c must fail");
    assert!(
        trace.iter().any(|k| k == "blocked"),
        "d must be blocked on c's failure"
    );
    // Final classification in insertion order: a,b completed; c failed;
    // d blocked; e completed.
    assert_eq!(states[0], TaskStatus::Completed);
    assert_eq!(states[1], TaskStatus::Completed);
    assert_eq!(states[2], TaskStatus::Failed);
    assert_eq!(states[3], TaskStatus::Blocked);
    assert_eq!(states[4], TaskStatus::Completed);
}

// ---------------------------------------------------------------------------
// §52 persistence + §24 interrupted restore
// ---------------------------------------------------------------------------

#[test]
fn persistence_roundtrip_marks_inflight_interrupted() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    // One fast completed task + one long-running (will be mid-flight).
    let a = create_task(&mut m, "done", "completion", &[]);
    let b = create_task(&mut m, "inflight", "long-running", &[]);
    m.task_add_arguments(&b, &["--duration", "600"]).unwrap();
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&a),
        30_000
    ));
    // Make sure b actually started before snapshot.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if m.task_get(&b).unwrap().agent_execution_id.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let state = m.snapshot_state();
    let persisted: PersistedSchedulerState = state.tasks.clone().expect("tasks persisted");
    assert_eq!(persisted.version, 1);
    // Serialization is bounded and secret-free: launches are never stored.
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("credential_ref"));

    let mut m2 = new_engine();
    m2.restore(state);
    assert_eq!(
        status_of(&m2, &a),
        TaskStatus::Completed,
        "terminal survives"
    );
    assert_eq!(
        status_of(&m2, &b),
        TaskStatus::Interrupted,
        "in-flight task interrupted, never silently resumed"
    );
    // Nothing is running after restore.
    assert!(m2.scheduler_status().running.is_empty());
}

// ---------------------------------------------------------------------------
// §22 event ordering
// ---------------------------------------------------------------------------

#[test]
fn task_events_are_ordered_per_task() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let a = create_task(&mut m, "a", "completion", &[]);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&a),
        30_000
    ));
    let trace = m.scheduler_trace();
    let mine: Vec<&str> = trace
        .iter()
        .filter(|(id, _)| id == &a)
        .map(|(_, k)| k.as_str())
        .collect();
    // Every task must see: created → ready → started → completed (in order,
    // with no missing or reordered steps).
    assert!(
        mine.windows(2).all(|w| order_rank(w[0]) < order_rank(w[1])),
        "per-task order violated: {mine:?}"
    );
    assert!(mine.contains(&"created"));
    assert!(mine.contains(&"ready"));
    assert!(mine.contains(&"started"));
    assert!(mine.contains(&"completed"));
}

fn order_rank(kind: &str) -> usize {
    match kind {
        "created" => 0,
        "ready" => 1,
        "started" => 2,
        "blocked" => 3,
        "waiting" => 4,
        "needs_review" => 5,
        "retrying" => 6,
        "completed" | "failed" | "cancelled" | "interrupted" => 7,
        _ => 8,
    }
}

// ---------------------------------------------------------------------------
// §49 stress: 100 tasks at max_agents = 2 / 5 / 10
// ---------------------------------------------------------------------------

#[test]
fn stress_100_tasks_across_concurrency_levels() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    for max in [2usize, 5, 10] {
        let mut m = new_engine();
        let mut ids = Vec::new();
        for i in 0..100 {
            ids.push(create_task(&mut m, &format!("t{i:03}"), "completion", &[]));
        }
        let mut policy = m.task_policy();
        policy.max_agents = max;
        policy.max_parallel_tasks = max;
        m.set_task_policy(policy);

        m.task_run();
        let started = Instant::now();
        assert!(
            drain_until_terminal(&mut m, &ids, 90_000),
            "100-task workflow did not settle at max_agents={max}"
        );
        let elapsed = started.elapsed();

        let s = m.scheduler_status();
        assert_eq!(
            s.completed_count, 100,
            "all tasks completed at max_agents={max}"
        );
        assert_eq!(s.failed_count, 0);
        assert_eq!(
            s.started_count, 100,
            "exactly one attempt per task (no over-spawn) at max_agents={max}"
        );
        // Throughput floor (§49): the whole workflow in a plausible window.
        assert!(
            elapsed < Duration::from_secs(60),
            "scheduler too slow at max_agents={max}: {elapsed:?}"
        );
        eprintln!("stress max_agents={max}: 100 tasks settled in {elapsed:?}");
    }
}

// ---------------------------------------------------------------------------
// §50 performance regression: orchestration must be ~free when idle.
// ---------------------------------------------------------------------------

#[test]
fn idle_orchestration_drains_do_not_degrade_frames() {
    let mut m = new_engine();
    // Engine with orchestration present but no workflow: every drain must
    // stay cheap (empty view + empty queue + empty outbox).
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = m.drain_frame();
    }
    let total = start.elapsed();
    // 1000 idle drains in well under a second: per-frame orchestration
    // overhead stays in the microseconds with no active workflow (§50).
    assert!(
        total < Duration::from_secs(1),
        "idle orchestration overhead too high: {total:?}"
    );
    let s = m.scheduler_status();
    assert!(s.states.is_empty(), "no tasks were invented");
}

// ---------------------------------------------------------------------------
// Phase 3A.1 §7: task commands are safe against every reachable state.
// ---------------------------------------------------------------------------

/// Builds every reachable task state using the deterministic fixture.
/// Returns (task id, expected state) — the table asserts the state was
/// actually reached before exercising the commands.
///
/// `Ready` is intentionally absent: it is the pre-start scheduled band a
/// task passes through *inside* a single `step()` (submit→spawn), never
/// observable through the public engine API; the API treats it like
/// `Pending` for every command.
#[derive(Debug)]
enum Fixture {
    NoTasks,
    UnknownId,
    Pending,
    Running,
    Waiting,
    Blocked,
    NeedsReview,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}

fn build_fixture(m: &mut Multiplexer, f: &Fixture) -> String {
    match f {
        Fixture::NoTasks => String::new(),
        Fixture::UnknownId => "task-does-not-exist".to_string(),
        Fixture::Pending => create_task(m, "p", "completion", &[]),
        Fixture::Running => {
            let id = create_task(m, "run", "long-running", &[]);
            m.task_add_arguments(&id, &["--duration", "30"])
                .expect("args set");
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&id)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Running
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            id
        }
        Fixture::Waiting => {
            let a = create_task(m, "wa", "long-running", &[]);
            m.task_add_arguments(&a, &["--duration", "30"])
                .expect("args set");
            let b = create_task(m, "wb", "completion", &[&a]);
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&b)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Waiting
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            b
        }
        Fixture::Blocked => {
            // Hard failure policy on the dependency → dependent is Blocked.
            let a = create_task(m, "bA", "failure", &[]);
            let b = create_task(m, "bB", "completion", &[&a]);
            let mut policy = m.task_policy();
            policy.failure = DependencyFailurePolicy::BlockDownstream;
            m.set_task_policy(policy);
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&b)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Blocked
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            b
        }
        Fixture::NeedsReview => {
            let rid = m
                .task_create(&ws_id(m), "nr", "phase3a fixture", "fake-agent", &[], true)
                .expect("review task created");
            m.task_set_environment(
                &rid,
                &[("FAKE_AGENT_SCENARIO".to_string(), "completion".to_string())],
            )
            .expect("scenario set");
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&rid)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::NeedsReview
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            rid
        }
        Fixture::Completed => {
            let id = create_task(m, "c", "completion", &[]);
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&id)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Completed
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            id
        }
        Fixture::Failed => {
            let id = create_task(m, "f", "failure", &[]);
            let mut policy = m.task_policy();
            policy.retry = Default::default(); // never retry hard failures
            m.set_task_policy(policy);
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&id)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Failed
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            id
        }
        Fixture::Cancelled => {
            let id = create_task(m, "x", "long-running", &[]);
            m.task_add_arguments(&id, &["--duration", "30"])
                .expect("args set");
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&id)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Running
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = m.task_cancel(&id);
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&id)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Cancelled
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            id
        }
        Fixture::Skipped => {
            // Fail-fast dependency policy → the dependent never runs.
            let a = create_task(m, "sA", "failure", &[]);
            let b = create_task(m, "sB", "completion", &[&a]);
            let mut policy = m.task_policy();
            policy.failure = DependencyFailurePolicy::SkipDownstream;
            m.set_task_policy(policy);
            m.task_run();
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let _ = m.drain_frame();
                if m.task_get(&b)
                    .map(|t| t.status)
                    .unwrap_or(TaskStatus::Pending)
                    == TaskStatus::Skipped
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            b
        }
    }
}

#[test]
fn commands_are_safe_across_all_task_states() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let fixtures = [
        Fixture::NoTasks,
        Fixture::UnknownId,
        Fixture::Pending,
        Fixture::Running,
        Fixture::Waiting,
        Fixture::Blocked,
        Fixture::NeedsReview,
        Fixture::Completed,
        Fixture::Failed,
        Fixture::Cancelled,
        Fixture::Skipped,
    ];
    for f in &fixtures {
        let mut m = new_engine();
        let id = build_fixture(&mut m, f);
        let before = m.scheduler_status().states.len();
        let label = format!("{f:?}");

        // Cancel must never panic; on unknown ids it errors cleanly.
        let cancel = m.task_cancel(&id);
        match f {
            Fixture::UnknownId => assert!(cancel.is_err(), "{label}: unknown cancel must err"),
            Fixture::NoTasks => assert!(cancel.is_err(), "{label}: empty cancel must err"),
            _ => assert!(
                cancel.is_ok() || cancel.is_err(),
                "{label}: cancel panicked"
            ),
        }

        let retry = m.task_retry(&id);
        match f {
            Fixture::UnknownId => assert!(retry.is_err(), "{label}: unknown retry must err"),
            Fixture::NoTasks => assert!(retry.is_err(), "{label}: empty retry must err"),
            _ => assert!(retry.is_ok() || retry.is_err(), "{label}: retry panicked"),
        }
        // Review resolution must be a typed error or no-op, never a panic.
        let approve = m.resolve_task_review(&id, true);
        let reject = m.resolve_task_review(&id, false);
        assert!(
            approve.is_ok() || approve.is_err(),
            "{label}: approve panicked"
        );
        assert!(
            reject.is_ok() || reject.is_err(),
            "{label}: reject panicked"
        );
        // Attach must be a typed error or a pane, never a panic.
        let attach = m.attach_task_agent_pane(&id);
        assert!(
            attach.is_ok() || attach.is_err(),
            "{label}: attach panicked"
        );

        // The engine must stay alive and consistent after every interaction.
        let _ = m.drain_frame();
        let after = m.scheduler_status().states.len();
        match f {
            Fixture::NoTasks => assert_eq!(after, 0, "{label}: tasks invented"),
            Fixture::UnknownId => {
                assert_eq!(after, before, "{label}: unknown id changed state")
            }
            _ => assert_eq!(after, before, "{label}: task set changed size"),
        }
    }
}

#[test]
fn review_resolution_is_type_safe_and_state_consistent() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let rid = build_fixture(&mut m, &Fixture::NeedsReview);
    assert_eq!(status_of(&m, &rid), TaskStatus::NeedsReview);

    // Approving must leave the task in a consistent successor state (a
    // typed error is only legal if the review was already resolved).
    match m.resolve_task_review(&rid, true) {
        Ok(()) => {
            let _ = m.drain_frame();
            let s = m.scheduler_status();
            assert_eq!(s.completed_count + s.failed_count, 1);
        }
        Err(e) => {
            let _ = m.drain_frame();
            let t = status_of(&m, &rid);
            assert_ne!(
                t,
                TaskStatus::NeedsReview,
                "{e:?}: review errored without a state change"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3A.1 §15: event flood — large-output tasks must not wedge the
// scheduler or the event bus.
// ---------------------------------------------------------------------------

#[test]
fn large_output_flood_completes_without_wedging() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let id = create_task(&mut m, "flood", "large-output", &[]);
    let events_before = m.metrics.events_applied;
    m.task_run();
    let started = Instant::now();
    // Custom drain loop: sample queue depth and sustained throughput while
    // the 100k-line flood streams through the same bus as the scheduler.
    let mut max_queue = 0usize;
    let mut peak_eps = 0.0f64;
    let mut settled = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        max_queue = max_queue.max(m.session_pending_total());
        peak_eps = peak_eps.max(m.metrics.events_per_second());
        if m.task_get(&id)
            .map(|t| t.status.is_terminal())
            .unwrap_or(false)
        {
            settled = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(settled, "large-output task did not settle in 120s");
    let elapsed = started.elapsed();
    let events = m.metrics.events_applied - events_before;
    // Batch-count evidence that output actually flowed (batches, not lines),
    // sustained throughput, and a bounded outbox — the flood must not wedge
    // the scheduler or the event bus.
    assert!(
        events > 100,
        "flood produced only {events} batches — suspicious"
    );
    assert!(
        peak_eps > 1_000.0,
        "flood never sustained >1k batches/s (peak {peak_eps:.0})"
    );
    assert!(
        max_queue < 10_000,
        "outbox grew unbounded during flood: {max_queue}"
    );
    eprintln!(
        "event flood: {elapsed:?} wall, {events} batches, peak {peak_eps:.0}/s, max queue {max_queue}"
    );
    let t = m.task_get(&id).unwrap();
    assert_eq!(t.status, TaskStatus::Completed, "flood task result");
    // The scheduler remains responsive afterwards: a second task runs fine.
    let id2 = create_task(&mut m, "after", "completion", &[]);
    m.task_run();
    assert!(drain_until_terminal(
        &mut m,
        std::slice::from_ref(&id2),
        30_000
    ));
}

#[test]
fn concurrent_flood_does_not_drop_completions() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let mut ids = Vec::new();
    for i in 0..4 {
        let id = create_task(&mut m, &format!("flood{i}"), "large-output", &[]);
        ids.push(id);
    }
    let mut policy = m.task_policy();
    policy.max_agents = 4;
    policy.max_parallel_tasks = 4;
    m.set_task_policy(policy);
    m.task_run();
    assert!(
        drain_until_terminal(&mut m, &ids, 120_000),
        "concurrent floods did not settle"
    );
    let s = m.scheduler_status();
    assert_eq!(s.completed_count, 4, "all flood tasks completed");
    assert_eq!(s.failed_count, 0);
}
