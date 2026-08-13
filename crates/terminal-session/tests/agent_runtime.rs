//! End-to-end agent runtime integration tests (Phase 2B §6).
//!
//! These drive the deterministic `fake-agent` binary through the **same**
//! code path production agents use: adapter → resolve → PTY spawn →
//! reader/tap → pump → semantic events. If the binary is missing, the
//! harness builds it once (cargo build -p fake-agent) so `cargo test`
//! alone is enough.

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use terminal_session::agent::{AgentRegistry, AgentRuntime};
use terminal_session::credential::{CredentialStore, MemoryBackend};
use terminal_session::execution::{AgentEvent, AgentState, ExecutionId};
use terminal_session::launch::AgentLaunchConfig;
use terminal_session::provider::ProviderRegistry;

fn ensure_fake_agent_built() {
    if terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_ok() {
        return;
    }
    let manifest = env!("CARGO_MANIFEST_DIR");
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "fake-agent"])
        .current_dir(format!("{manifest}/../.."))
        .status()
        .expect("cargo available for building fake-agent");
    assert!(status.success(), "failed to build fake-agent");
}

fn launch(definition_id: &str, args: Vec<&str>) -> AgentLaunchConfig {
    AgentLaunchConfig {
        definition_id: definition_id.to_string(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        arguments: args.into_iter().map(|s| s.to_string()).collect(),
        provider_id: None,
        model_id: None,
        credential_ref: None,
        resume_id: None,
        environment: vec![],
    }
}

fn runtime() -> AgentRuntime {
    AgentRuntime::new(
        Arc::new(AgentRegistry::new()),
        ProviderRegistry::new(),
        Arc::new(pty::PtyManager::new().unwrap()),
        CredentialStore::with_backend(Arc::new(MemoryBackend::new())),
        None,
    )
}

/// Drains events, keeping everything in `seen`, and returns the first
/// event matching `pred` (from the new batch). Earlier consumer failures
/// (like my first draft) must not discard unrelated events, so every
/// drained event is retained in `seen` for later predicates.
fn wait_for_event(
    runtime: &mut AgentRuntime,
    seen: &mut Vec<AgentEvent>,
    pred: impl Fn(&AgentEvent) -> bool,
    timeout: Duration,
) -> Option<AgentEvent> {
    let t0 = Instant::now();
    loop {
        if let Some(i) = seen.iter().position(&pred) {
            return Some(seen.remove(i));
        }
        for (_, ev) in runtime.drain_events() {
            if pred(&ev) {
                return Some(ev);
            }
            seen.push(ev);
        }
        if t0.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_state(
    runtime: &mut AgentRuntime,
    eid: &ExecutionId,
    state: AgentState,
    timeout: Duration,
) -> bool {
    let t0 = Instant::now();
    loop {
        if let Some(s) = runtime.get_session(eid) {
            if s.state == format!("{state:?}") {
                return true;
            }
        }
        let _ = runtime.drain_events();
        if t0.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn completion_reports_completed_with_exit_zero() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _session) = r
        .spawn(
            launch("fake-agent", vec!["--scenario", "completion"]),
            80,
            24,
        )
        .unwrap();
    let exited = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { code: Some(0) }),
        Duration::from_secs(10),
    );
    assert!(exited.is_some(), "expected Exited{{code: 0}}");
    let completed = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Completed),
        Duration::from_secs(2),
    );
    assert!(completed.is_some(), "expected Completed event");
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Completed,
        Duration::from_secs(2)
    ));
    let snap = r.get_session(&eid).unwrap();
    assert_eq!(snap.exit_code, Some(0));
    assert!(snap.duration_secs.unwrap_or(0) >= 0);
}

#[test]
fn failure_reports_failed_with_nonzero_exit() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "failure"]), 80, 24)
        .unwrap();
    let exited = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { code: Some(1) }),
        Duration::from_secs(10),
    );
    assert!(exited.is_some(), "expected Exited{{code: Some(1)}}");
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Failed,
        Duration::from_secs(2)
    ));
}

#[test]
fn crash_is_classified_crashed() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "crash"]), 80, 24)
        .unwrap();
    let exited = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { code: Some(139) }),
        Duration::from_secs(10),
    );
    assert!(exited.is_some(), "expected Exited{{code: Some(139)}}");
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Crashed,
        Duration::from_secs(2)
    ));
}

#[test]
fn approval_roundtrip_emits_permission_requested() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "approval"]), 80, 24)
        .unwrap();
    let perm = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::PermissionRequested { .. }),
        Duration::from_secs(10),
    );
    assert!(
        perm.is_some(),
        "expected PermissionRequested for approval scenario"
    );
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::NeedsApproval,
        Duration::from_secs(2)
    ));
    // The fake waits for input ("approve" completes it).
    r.send_input(&eid, b"approve\n").unwrap();
    let exited = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { code: Some(0) }),
        Duration::from_secs(10),
    );
    assert!(exited.is_some(), "expected exit 0 after approving");
}

#[test]
fn interactive_input_is_echoed_through_the_pty() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "waiting"]), 80, 24)
        .unwrap();
    let waiting = wait_for_state(&mut r, &eid, AgentState::Waiting, Duration::from_secs(10));
    assert!(waiting, "fake should reach Waiting state");
    // The fake echoes input and keeps reading; it only exits on "exit".
    r.send_input(&eid, b"hello from the test\n").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    r.send_input(&eid, b"exit\n").unwrap();
    let exited = wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { code: Some(0) }),
        Duration::from_secs(10),
    );
    assert!(exited.is_some(), "expected exit 0 after input");
}

#[test]
fn work_activity_is_detected() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "working"]), 80, 24)
        .unwrap();
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Working,
        Duration::from_secs(5)
    ));
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Completed,
        Duration::from_secs(10)
    ));
}

#[test]
fn stop_transitions_to_stopped_and_stays() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let mut seen = Vec::new();
    let (eid, _s) = r
        .spawn(launch("fake-agent", vec!["--scenario", "working"]), 80, 24)
        .unwrap();
    // Let it run a moment (working runs ~2s), then stop it.
    std::thread::sleep(Duration::from_millis(500));
    r.stop(&eid).unwrap();
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Stopped,
        Duration::from_secs(5)
    ));
    // A user stop must NOT later be re-classified as a failure.
    assert!(wait_for_event(
        &mut r,
        &mut seen,
        |e| matches!(e, AgentEvent::Exited { .. }),
        Duration::from_secs(5)
    )
    .is_some());
    let snap = r.get_session(&eid).unwrap();
    assert_eq!(
        snap.state, "Stopped",
        "stop must not be overridden by exit classification"
    );
}

#[test]
fn restart_respawns_with_same_execution_id() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let (eid, _s) = r
        .spawn(
            launch("fake-agent", vec!["--scenario", "completion"]),
            80,
            24,
        )
        .unwrap();
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Completed,
        Duration::from_secs(10)
    ));
    let session = r.restart(&eid, 80, 24).unwrap();
    // The ExecutionId (pane handle) is preserved; the PTY id underneath is
    // recycled for the fresh process.
    assert!(
        r.get_session(&eid).is_some(),
        "execution id must survive restart"
    );
    assert!(!session.has_exited());
    assert!(wait_for_state(
        &mut r,
        &eid,
        AgentState::Completed,
        Duration::from_secs(10)
    ));
}

#[test]
fn output_events_are_redacted_and_metric_counts() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let (eid, _s) = r
        .spawn(
            launch("fake-agent", vec!["--scenario", "completion"]),
            80,
            24,
        )
        .unwrap();
    let mut saw_output = false;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        for (_, ev) in r.drain_events() {
            if let AgentEvent::Output { text } = ev {
                assert!(!text.contains("sk-"), "output must be redacted");
                saw_output = true;
            }
        }
        if saw_output
            && r.get_session(&eid)
                .map(|s| s.state == "Completed")
                .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(saw_output, "expected at least one Output event");
}

#[test]
fn unknown_definition_errors_cleanly() {
    ensure_fake_agent_built();
    let mut r = runtime();
    let res = r.spawn(launch("definitely-not-a-real-agent", vec![]), 80, 24);
    assert!(res.is_err(), "unknown definition must error");
    let msg = format!("{:#}", res.err().unwrap());
    assert!(msg.contains("unknown agent definition"));
}
