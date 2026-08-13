//! Phase 2B.1 §29–§30: agent pane persistence + crash recovery.
//!
//! §29: an agent pane persists its definition, provider, model, credential
//! reference, cwd and launch configuration — but never credential contents.
//! Restarting the application (fresh engine) restores the pane.
//!
//! §30: an agent running when the application dies must restore as a *new*
//! process from its stored launch configuration — never pretend the old
//! process survived. Secrets are never persisted.

use std::time::{Duration, Instant};

use terminal_workspace::persist::{self};
use terminal_workspace::{Multiplexer, PersistedState, SplitDirection};

/// The agent pane (identified by its `agent` metadata) in the active tab.
fn agent_pane(m: &Multiplexer) -> Option<(String, String, serde_json::Value)> {
    let tab = m.active_tab()?;
    let mut panes = Vec::new();
    tab.root.panes(&mut panes);
    panes
        .into_iter()
        .find(|p| p.metadata.get("agent").is_some())
        .map(|p| (p.id.clone(), p.execution_id.0.clone(), p.metadata.clone()))
}

fn tmp_state(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ft-{name}-{}.json", std::process::id()))
}

fn fake_agent_available() -> bool {
    terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_ok()
}

fn launch_with_secrets() -> terminal_session::launch::AgentLaunchConfig {
    terminal_session::launch::AgentLaunchConfig {
        definition_id: "fake-agent".into(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        // Known secret shape: the engine's defensive redaction must mask it
        // in every persisted copy (§28–29).
        arguments: vec![
            "--scenario".into(),
            "waiting".into(),
            "--echo".into(),
            "sk-ant-SUPER_SECRET_TEST_VALUE_PERSIST_0123456789".into(),
        ],
        provider_id: Some("anthropic".into()),
        model_id: Some("claude-sonnet-4-5".into()),
        credential_ref: Some("keychain://flashterminal/anthropic".into()),
        resume_id: None,
        environment: vec![(
            "ANTHROPIC_API_KEY".into(),
            "sk-ant-SUPER_SECRET_TEST_VALUE_PERSIST_0123456789".into(),
        )],
    }
}

const SECRET: &str = "sk-ant-SUPER_SECRET_TEST_VALUE_PERSIST_0123456789";

/// §29: agent panes persist config + references, never credential contents;
/// a fresh engine restores the pane and its launch configuration.
#[test]
fn agent_pane_persists_config_not_secrets() {
    if !fake_agent_available() {
        eprintln!("SKIPPED: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let path = tmp_state("persist");

    let mut m = Multiplexer::new().unwrap();
    m.ensure_workspace();
    m.split_pane_agent(SplitDirection::Vertical, launch_with_secrets())
        .unwrap();
    // Drain briefly so the pane is fully live before snapshotting.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(800) {
        m.drain_frame();
        std::thread::sleep(Duration::from_millis(5));
    }
    m.save(&path).unwrap();

    let file = std::fs::read_to_string(&path).unwrap();
    // Config + references persist…
    for needle in [
        "\"fake-agent\"",
        "\"anthropic\"",
        "\"claude-sonnet-4-5\"",
        "keychain://flashterminal/anthropic",
    ] {
        assert!(file.contains(needle), "missing {needle} in state.json");
    }
    // …credential contents never do.
    assert!(!file.contains(SECRET), "secret leaked into state.json");

    // "Restart the application": a fresh engine restores the persisted
    // state and re-spawns the agent from its stored launch configuration.
    let state = persist::load(&path).unwrap();
    let mut m2 = Multiplexer::new().unwrap();
    let failed = m2.restore(state);
    assert!(
        failed.is_empty(),
        "agent pane must restore cleanly (failed: {failed:?})"
    );

    let mut panes = Vec::new();
    m2.active_tab().unwrap().root.panes(&mut panes);
    let (pane_id, _eid, meta) = agent_pane(&m2).expect("agent pane must restore");
    let meta = meta.to_string();
    assert!(
        meta.contains("\"fake-agent\""),
        "restored launch lost definition"
    );
    assert!(
        meta.contains("keychain://flashterminal/anthropic"),
        "restored launch lost credential reference"
    );
    assert!(
        !meta.contains(SECRET),
        "restored launch re-exposed the secret"
    );
    // The restored pane has a live session.
    assert!(
        m2.terminal_session_for_pane(&pane_id).is_some(),
        "restored pane must have a live session"
    );

    let _ = std::fs::remove_file(&path);
}

/// §30: an agent running at "crash" time restores as a new process from
/// stored configuration — the old process is honestly gone.
#[test]
fn crash_recovery_restores_pane_not_process() {
    if !fake_agent_available() {
        eprintln!("SKIPPED: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let path = tmp_state("crash");

    // "Application running with a live agent."
    let (old_eid, state) = {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        m.split_pane_agent(SplitDirection::Vertical, launch_with_secrets())
            .unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(800) {
            m.drain_frame();
            std::thread::sleep(Duration::from_millis(5));
        }
        let (_, eid, _) = agent_pane(&m).expect("agent pane exists before crash");
        let snap = m.agent_runtime().list_sessions();
        assert!(
            snap.iter().any(|s| s.execution_id == eid),
            "agent must be running before the crash"
        );
        // "Application terminated unexpectedly": the engine is dropped; the
        // persisted state file is what survives.
        let state = m.snapshot_state();
        (eid, state)
    };

    // "Application restarted."
    let state_json = serde_json::to_string(&state).unwrap();
    let restored: PersistedState = serde_json::from_str(&state_json).unwrap();
    let mut m2 = Multiplexer::new().unwrap();
    let failed = m2.restore(restored);
    assert!(
        failed.is_empty(),
        "pane must restore after crash ({failed:?})"
    );

    let mut panes = Vec::new();
    m2.active_tab().unwrap().root.panes(&mut panes);
    let (_pane_id, new_eid, _) = agent_pane(&m2).expect("pane restored after crash");
    assert!(
        new_eid != old_eid,
        "restored pane must NOT reuse the dead process's execution id"
    );
    // The old process is gone; the new one is running from stored config.
    assert!(
        m2.agent_runtime()
            .get_session(&terminal_session::execution::ExecutionId(old_eid.clone()))
            .is_none(),
        "old process must not be pretend-recovered"
    );
    let snaps = m2.agent_runtime().list_sessions();
    assert!(
        snaps.iter().any(|s| s.state == "Starting"
            || s.state == "Working"
            || s.state == "Waiting"
            || s.state == "NeedsApproval"),
        "restored agent must be running fresh: {snaps:?}"
    );
    let _ = std::fs::remove_file(&path);
}

/// Sanity: `AgentLaunchConfig::redact()` is the guard the engine relies on;
/// prove it masks both args and env for the exact secret shape used above.
#[test]
fn launch_redaction_covers_args_and_env() {
    let mut cfg = launch_with_secrets();
    cfg.redact();
    let json = serde_json::to_string(&cfg).unwrap();
    assert!(!json.contains(SECRET), "redact() must mask args + env");
    assert!(
        json.contains("keychain://flashterminal/anthropic"),
        "references survive redaction"
    );
}
