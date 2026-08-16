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

/// Phase 4 §25: a pending approval and the budget ledger survive an
/// application restart; the persisted state never contains secrets; and a
/// restored approval is still integrity-bound (identity, hash, expiry).
#[test]
fn restart_preserves_pending_approval_and_budget() {
    use terminal_session::policy::{Action, BudgetDimension, PolicyContext};

    let path = tmp_state("approval-restart");
    let secret = "sk-ant-SENTINEL_APPROVAL_RESTART_0000";

    let (approval_id, hash) = {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        // Budget ledger is mutated (spent commands) before the crash.
        m.policy_state_mut()
            .budget
            .record(BudgetDimension::CommandCount, 42);
        // A require-approval action is pending when the app dies.
        let mut ctx = PolicyContext::new("wf-restart");
        ctx.agent_id = Some("agent-a".into());
        let action = Action::Shell(format!("echo {secret} > /tmp/leak"));
        let hash = terminal_session::policy::action_hash("shell", &["echo *"]);
        let eval = m.evaluate_action(&action, &ctx);
        assert_eq!(
            eval.decision,
            terminal_session::policy::PolicyDecision::RequireApproval,
            "shell redirection must require approval"
        );
        let id = m.request_policy_approval(&eval, &ctx, &hash);
        assert_eq!(m.policy_state().approvals.pending_count(), 1);
        // Persist (as a save/crash would).
        let state = m.snapshot_state();
        let file = serde_json::to_string(&state).unwrap();
        assert!(
            !file.contains(secret),
            "pending approval leaked the secret into persisted state"
        );
        let _ = persist::save(&state, &path);
        (id, hash)
    };

    // "Restart the application": fresh engine restores policy + approvals.
    let state = persist::load(&path).unwrap();
    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);

    // Budget survived the restart.
    assert_eq!(
        m2.policy_state()
            .budget
            .value(BudgetDimension::CommandCount),
        42,
        "budget counters must survive restart"
    );
    // The pending approval survived as *pending* — never auto-executed.
    assert_eq!(m2.policy_state().approvals.pending_count(), 1);
    let audit = m2.audit_trail();
    assert!(
        audit
            .of_kind(terminal_session::audit::AuditEventKind::ApprovalRequested)
            .iter()
            .any(|e| e.action.contains("restored after restart")),
        "restore must surface restored approvals in the audit trail"
    );
    // Granting + honoring the restored approval re-verifies integrity.
    m2.grant_policy_approval(&approval_id, "Ali").unwrap();
    assert_eq!(m2.policy_state().approvals.pending_count(), 0);
    m2.policy_state_mut()
        .approvals
        .honor(&approval_id, "wf-restart", Some("agent-a"), &hash)
        .expect("restored approval honors when identity+hash match");

    let _ = std::fs::remove_file(&path);
}

/// Phase 4 §17: the audit trail survives restarts and keeps answering
/// "why did FlashTerminal do this?" after an application restart.
#[test]
fn restart_preserves_audit_trail() {
    let path = tmp_state("audit-restart");

    let first_plan = {
        let mut m = Multiplexer::new().unwrap();
        m.ensure_workspace();
        let id = m.audit_kind(
            terminal_session::audit::AuditEventKind::PlanCreated,
            "wf-audit",
            "plan v1: implement oauth",
            "planner",
        );
        m.audit_kind(
            terminal_session::audit::AuditEventKind::PlanApproved,
            "wf-audit",
            "plan v1 approved",
            "user",
        );
        let state = m.snapshot_state();
        persist::save(&state, &path).unwrap();
        id
    };

    let state = persist::load(&path).unwrap();
    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);

    let events = m2.audit_records();
    assert!(
        events.iter().any(|e| e.id == first_plan),
        "original audit records must survive restart (got {} events)",
        events.len()
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == terminal_session::audit::AuditEventKind::PlanApproved),
        "plan-approved record must survive restart"
    );
    // The explain surface still works after restart.
    assert!(
        m2.audit_explain(&first_plan).is_some(),
        "audit_explain must work after restart"
    );
    let _ = std::fs::remove_file(&path);
}
