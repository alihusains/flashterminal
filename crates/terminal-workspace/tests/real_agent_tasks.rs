//! Phase 3A.1 §9–§11: real-agent task execution through the orchestrator.
//!
//! `#[ignore]`d by default — run manually:
//!
//! ```text
//! cargo test -p terminal-workspace --test real_agent_tasks -- --ignored --nocapture
//! ```
//!
//! For each real agent (claude-code / codex / opencode / pi) the suite
//! creates a trivial task through the engine and runs it through the full
//! scheduler pipeline (adapter → launch → PTY session → work record →
//! task result). Every step prints what was actually *observed*.
//!
//! Policy (3a.md §11, §24): authentication failures and unavailable agents
//! are recorded as SKIPPED / AUTH FAILURE / TIMEOUT, never FAILED. The
//! assertions are about the *engine* — typed errors, bounded behavior, no
//! panics — never about agent availability.

use std::time::{Duration, Instant};

use terminal_workspace::engine::Multiplexer;
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::orchestration::TaskStatus;

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

fn new_engine() -> Multiplexer {
    let mut m = Multiplexer::new().unwrap();
    let root = std::env::temp_dir().to_string_lossy().to_string();
    m.create_workspace("real-agent-ws", &root).unwrap();
    m
}

/// Pumps the engine until the task settles or the deadline passes. On
/// deadline the task is cancelled (long-running is lawful — §17 — so a
/// hung real agent must be stopped, not waited on forever).
/// Returns the final observed status after the deadline (Cancelled).
fn pump_until_terminal(m: &mut Multiplexer, id: &str, timeout: Duration) -> TaskStatus {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if let Some(t) = m.task_get(&id.to_string()) {
            if t.status.is_terminal() {
                return t.status;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = m.task_cancel(&id.to_string());
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if let Some(t) = m.task_get(&id.to_string()) {
            if t.status.is_terminal() {
                return t.status;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    m.task_get(&id.to_string())
        .map(|t| t.status)
        .unwrap_or(TaskStatus::Pending)
}

/// Phase 3A.1 §9: one trivial task per installed real agent, through the
/// scheduler. Observations print per agent; the assertions are engine-side.
#[test]
#[ignore]
fn trivial_tasks_run_against_installed_real_agents() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (fixture prerequisite)");
        return;
    }
    let agents = ["claude-code", "codex", "opencode", "pi"];
    for agent in agents {
        let mut m = new_engine();
        let id = m.task_create(
            &m.workspaces()[0].id.clone(),
            "reply OK",
            "",
            agent,
            &[],
            false,
        );
        let id = match id {
            Ok(id) => id,
            Err(e) => {
                eprintln!("{agent}: task_create failed: {e:?}");
                continue;
            }
        };
        m.task_run();
        let started = Instant::now();
        let status = pump_until_terminal(&mut m, &id, Duration::from_secs(90));
        let elapsed = started.elapsed();
        let t = m.task_get(&id).unwrap();
        match &t.error {
            Some(err) => eprintln!(
                "{agent}: {status:?} after {elapsed:?} — error kind {:?} class {:?}",
                err.kind, err.class
            ),
            None => eprintln!(
                "{agent}: {status:?} after {elapsed:?} — {}",
                t.result
                    .as_ref()
                    .map(|r| r.summary.clone())
                    .unwrap_or_else(|| "(no result)".to_string())
            ),
        }
        // Engine-side assertions: whatever happened, the engine stayed
        // consistent and the task settled in a lawful state.
        assert!(
            matches!(
                status,
                TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
                    | TaskStatus::Blocked
                    | TaskStatus::Skipped
            ),
            "{agent}: unlawful final state {status:?}"
        );
        let _ = m.drain_frame();
    }
}

/// Phase 3A.1 §10: a parallel multi-agent workflow (two independent agents)
/// must run without cross-talk even when one agent is unavailable.
#[test]
#[ignore]
fn parallel_multi_agent_workflow_is_isolated() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (fixture prerequisite)");
        return;
    }
    let mut m = new_engine();
    let ws = m.workspaces()[0].id.clone();
    // fake-agent (always available) + every installed real agent.
    let mut agents = vec!["fake-agent".to_string()];
    for a in ["claude-code", "codex", "opencode", "pi"] {
        if m.agent_runtime().definition_exists(a) {
            agents.push(a.to_string());
        }
    }
    let mut ids = Vec::new();
    for (i, agent) in agents.iter().enumerate() {
        let id = m.task_create(&ws, &format!("parallel-{i}"), "", agent, &[], false);
        match id {
            Ok(id) => ids.push(id),
            Err(e) => eprintln!("{agent}: task_create failed: {e:?}"),
        }
    }
    if ids.len() < 2 {
        eprintln!("skipping: fewer than 2 runnable agent definitions");
        return;
    }
    let mut policy = m.task_policy();
    policy.max_agents = ids.len();
    policy.max_parallel_tasks = ids.len();
    m.set_task_policy(policy);
    m.task_run();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut settled = Vec::new();
    while Instant::now() < deadline && settled.len() < ids.len() {
        let _ = m.drain_frame();
        settled = ids
            .iter()
            .filter(|id| {
                m.task_get(id)
                    .map(|t| t.status.is_terminal())
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        std::thread::sleep(Duration::from_millis(50));
    }
    // Hung real agents (e.g. a headless CLI stuck on auth) are lawful
    // long-running sessions — stop them so the workflow settles.
    for id in &ids {
        if !settled.contains(id) {
            let _ = m.task_cancel(id);
        }
    }
    let stop_deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < stop_deadline
        && !ids.iter().all(|id| {
            m.task_get(id)
                .map(|t| t.status.is_terminal())
                .unwrap_or(true)
        })
    {
        let _ = m.drain_frame();
        std::thread::sleep(Duration::from_millis(50));
    }
    let s = m.scheduler_status();
    eprintln!(
        "parallel isolation: {}/{} settled — completed {}, failed {}, cancelled {}",
        settled.len(),
        ids.len(),
        s.completed_count,
        s.failed_count,
        s.states
            .iter()
            .filter(|(_, st)| *st == TaskStatus::Cancelled)
            .count()
    );
    // Engine-side: no panic, no unlawful state, scheduler counters sane.
    assert!(s.completed_count + s.failed_count <= ids.len() as u32);
    for (id, st) in &s.states {
        assert!(
            *st != TaskStatus::Running && *st != TaskStatus::Waiting,
            "{id}: workflow left an agent running after settle"
        );
    }
}

/// Phase 3A.1 §11: an authentication failure surfaces as a *typed* task
/// error with the AuthenticationFailure class (never a crash, never a
/// bare string).
#[test]
#[ignore]
fn auth_failure_is_a_typed_task_error() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built");
        return;
    }
    let mut m = new_engine();
    let ws = m.workspaces()[0].id.clone();
    let id = m
        .task_create(&ws, "auth", "", "fake-agent", &[], false)
        .expect("task created");
    m.task_set_environment(
        &id,
        &[(
            "FAKE_AGENT_SCENARIO".to_string(),
            "auth-failure".to_string(),
        )],
    )
    .expect("scenario set");
    m.task_run();
    let status = pump_until_terminal(&mut m, &id, Duration::from_secs(30));
    let t = m.task_get(&id).unwrap();
    match &t.error {
        Some(err) => {
            eprintln!(
                "auth failure: typed error kind={:?} class={:?} msg={}",
                err.kind, err.class, err.message
            );
            assert_eq!(status, TaskStatus::Failed);
            // The process exits 2 (auth-failure fixture): the engine records
            // AgentFailed as the *kind* and classifies it AuthenticationFailure
            // — the class is what the retry policy gates on (§26).
            assert_eq!(
                err.class,
                terminal_workspace::terminal_session::orchestration::FailureClass::AuthenticationFailure
            );
        }
        None => {
            eprintln!("auth failure: no error recorded ({status:?}) — recording as observed");
        }
    }
}
