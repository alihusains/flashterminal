//! Phase 2C regression suite (2c1.md §34).
//!
//! Permanent coverage for the Phase 2C observability surface:
//! AgentWork, Activity, Timeline, Summary, Attention, Dashboard, Usage,
//! Pricing, Health, Replay, Intent, Notifications, QuietMode, Persistence.
//!
//! Runtime tests use the deterministic `fake-agent` binary and skip when it
//! is not built (same policy as the engine's own tests); every other test is
//! pure and deterministic.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use terminal_workspace::engine::Multiplexer;
use terminal_workspace::notify::{Notification, NotificationKind, NotificationPrefs};
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::agent::PermissionDecision;
use terminal_workspace::terminal_session::execution::{AgentEvent, AgentState, ExecutionId};
use terminal_workspace::terminal_session::launch::AgentLaunchConfig;
use terminal_workspace::terminal_session::work::{
    all_fixtures, attention_for, replay_into, ActivityKind, AgentActivityState, AgentFilter,
    AgentIntent, AgentTimeline, AgentUsage, AgentWork, IntentResolver, PricingRegistry,
    TimelineKind, WorkStatus,
};

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

/// Drains the engine until the agent reaches `state` or the timeout passes.
fn drain_until(eng: &mut Multiplexer, eid: &ExecutionId, state: &str, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = eng.drain_frame();
        if let Some(s) = eng.agent_runtime().get_session(eid) {
            if s.state == state {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn spawn_fake(m: &mut Multiplexer, scenario: &str) -> ExecutionId {
    let launch = AgentLaunchConfig {
        definition_id: "fake-agent".to_string(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        arguments: vec!["--scenario".to_string(), scenario.to_string()],
        provider_id: None,
        model_id: None,
        credential_ref: None,
        resume_id: None,
        environment: vec![],
    };
    // Spawn inside a pane: notifications and per-workspace summaries only
    // apply to agents attached to a pane.
    let pane = m
        .split_pane_agent(terminal_workspace::model::SplitDirection::Vertical, launch)
        .expect("agent pane");
    m.execution_id_for_pane(&pane)
        .expect("pane carries an execution id")
}

// ---------------------------------------------------------------------------
// AgentWork lifecycle + session separation (§7, §34 "AgentWork")
// ---------------------------------------------------------------------------

#[test]
fn work_lifecycle_and_idempotent_finish() {
    let mut w = AgentWork::new("exec-1", "fix auth");
    assert_eq!(w.status, WorkStatus::Running);
    w.files_changed.insert("src/auth.rs".into());
    w.commands.push("cargo test".into());
    w.push_activity(AgentActivityState::heuristic(
        ActivityKind::Editing,
        "src/auth.rs",
    ));
    w.push_error(terminal_workspace::terminal_session::work::WorkError::new(
        terminal_workspace::terminal_session::work::ErrorKind::CommandFailure,
        "test failed",
    ));
    w.finish(WorkStatus::Completed);
    w.finish(WorkStatus::Failed); // idempotent: completed wins
    assert_eq!(w.status, WorkStatus::Completed);
    let s = w.summary();
    assert_eq!(s.files_changed, 1);
    assert_eq!(s.commands_run, 1);
    assert_eq!(s.errors, 1);
    assert!(s.duration_secs.is_some());
}

#[test]
fn work_items_share_a_session() {
    // §7: AgentWork is the work, AgentSession is the process. Two works may
    // reference the same session while being independent units.
    let mut w1 = AgentWork::new("session-42", "task one");
    let w2 = AgentWork::new("session-42", "task two");
    assert_eq!(w1.session_id, w2.session_id);
    w1.finish(WorkStatus::Completed);
    assert_eq!(w1.status, WorkStatus::Completed);
    assert_eq!(w2.status, WorkStatus::Running, "sibling work is untouched");
    assert_ne!(w1.id, w2.id);
}

#[test]
fn session_survives_work_completion_and_restart() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("w", "/tmp").unwrap();
    let eid = spawn_fake(&mut m, "completion");
    assert!(
        drain_until(&mut m, &eid, "Completed", 15000),
        "fake completion agent must reach Completed"
    );
    let w1 = m
        .agent_runtime()
        .get_work(&eid)
        .expect("work exists after completion");
    assert_eq!(w1.status, WorkStatus::Completed);
    // Session container survives the completed work.
    assert!(m.agent_runtime().get_session(&eid).is_some());

    // Restart: same session container (execution id), new work item.
    m.restart_agent_session(&eid).unwrap();
    let w2 = m
        .agent_runtime()
        .get_work(&eid)
        .expect("new work after restart");
    assert_ne!(w1.id, w2.id, "restart creates a fresh work item");
    assert_eq!(w2.status, WorkStatus::Running);
    assert!(m.agent_runtime().get_session(&eid).is_some());
    assert!(
        drain_until(&mut m, &eid, "Completed", 15000),
        "restarted agent must complete again"
    );
    // Limitation (documented, 2c1 §7): the runtime keeps one work per
    // execution at a time — concurrent works per session are not supported.
}

// ---------------------------------------------------------------------------
// Activity fixtures (§8, §34 "Activity")
// ---------------------------------------------------------------------------

#[test]
fn activity_kinds_are_deterministic() {
    let kinds = [
        ActivityKind::Starting,
        ActivityKind::Reading,
        ActivityKind::Thinking,
        ActivityKind::Planning,
        ActivityKind::Editing,
        ActivityKind::RunningCommand,
        ActivityKind::RunningTests,
        ActivityKind::WaitingForInput,
        ActivityKind::WaitingForPermission,
        ActivityKind::Reviewing,
        ActivityKind::Finishing,
        ActivityKind::Idle,
        ActivityKind::Unknown,
    ];
    for kind in kinds {
        let mut w = AgentWork::new("s", "t");
        w.push_activity(AgentActivityState::heuristic(kind, "detail"));
        let cur = w.current_activity().expect("activity recorded");
        assert_eq!(cur.kind, kind, "expected activity matches");
        assert_eq!(
            cur.source,
            terminal_workspace::terminal_session::work::ActivitySource::Heuristic
        );
        assert_eq!(cur.confidence, 60, "heuristic confidence is Medium=60");
        assert!(!cur.display().is_empty());
    }
}

#[test]
fn activity_coalescing_folds_rapid_events() {
    // §9: 100 activity events within 100 ms of the same kind fold into one
    // normalized record — the UI sees one stable activity, not a flood.
    let mut w = AgentWork::new("s", "t");
    let base = 1_000_000u64;
    for i in 0..100u64 {
        w.push_activity(AgentActivityState {
            kind: ActivityKind::Editing,
            source: terminal_workspace::terminal_session::work::ActivitySource::Heuristic,
            confidence: 60,
            detail: "src/main.rs".into(),
            at_ms: base + i, // 99 ms span < ACTIVITY_COALESCE_MS (400)
            count: 1,
        });
    }
    assert_eq!(w.activity.len(), 1, "100 raw events -> 1 coalesced record");
    assert_eq!(
        w.activity[0].count, 100,
        "occurrences folded into the count"
    );
}

#[test]
fn activity_histories_are_bounded() {
    // Alternating kinds never coalesce: 1000 pushes must stay bounded at
    // DEFAULT_ACTIVITY_CAPACITY (32) — no unbounded UI event growth.
    let mut w = AgentWork::new("s", "t");
    let kinds = [
        ActivityKind::Reading,
        ActivityKind::Editing,
        ActivityKind::Thinking,
    ];
    for i in 0..1000u64 {
        w.push_activity(AgentActivityState {
            kind: kinds[(i % 3) as usize],
            source: terminal_workspace::terminal_session::work::ActivitySource::Heuristic,
            confidence: 60,
            detail: format!("step {i}"),
            at_ms: 2_000_000 + i * 500, // outside the coalesce window
            count: 1,
        });
    }
    assert!(w.activity.len() <= 32, "bounded: {}", w.activity.len());
    assert_eq!(
        w.current_activity().unwrap().kind,
        ActivityKind::Reading,
        "last pushed kind wins (i=999 → Reading)"
    );
}

// ---------------------------------------------------------------------------
// Timeline bounds (§10, §34 "Timeline")
// ---------------------------------------------------------------------------

#[test]
fn timeline_is_bounded_ordered_deterministic() {
    let t = AgentTimeline::new(512);
    assert_eq!(t.len(), 0);
    assert!(t.is_empty());
    let mut t = t;
    for i in 0..5000u64 {
        t.push(TimelineKind::Activity, format!("e{i}"));
    }
    assert!(t.len() <= 512, "bounded: {}", t.len());
    let recent: Vec<String> = t.recent(3).map(|e| e.detail.clone()).collect();
    assert_eq!(
        recent,
        vec![
            "e4999".to_string(),
            "e4998".to_string(),
            "e4997".to_string()
        ]
    );
    t.clear();
    assert_eq!(t.len(), 0);
}

#[test]
fn timeline_growth_is_bounded_via_work() {
    // The AgentWork timeline must not become a memory leak under a flood.
    let mut w = AgentWork::new("s", "t");
    for i in 0..5000u64 {
        w.push_activity(AgentActivityState {
            kind: ActivityKind::RunningCommand,
            source: terminal_workspace::terminal_session::work::ActivitySource::Heuristic,
            confidence: 60,
            detail: format!("c{i}"),
            at_ms: 3_000_000 + i * 500,
            count: 1,
        });
        w.timeline.push(TimelineKind::Activity, format!("t{i}"));
    }
    assert!(w.timeline.len() <= 512);
    assert!(w.activity.len() <= 32);
}

// ---------------------------------------------------------------------------
// Attention (§13, §34 "Attention")
// ---------------------------------------------------------------------------

#[test]
fn attention_map_is_exact() {
    use terminal_workspace::terminal_session::work::AttentionReason;
    let expected: &[(AgentState, Option<AttentionReason>)] = &[
        (AgentState::Created, None),
        (AgentState::Starting, None),
        (AgentState::Working, None),
        (AgentState::Waiting, Some(AttentionReason::NeedsInput)),
        (
            AgentState::NeedsApproval,
            Some(AttentionReason::PermissionRequested),
        ),
        (AgentState::Blocked, Some(AttentionReason::Ambiguous)),
        (AgentState::Completed, None),
        (AgentState::Failed, Some(AttentionReason::ErrorIntervention)),
        (
            AgentState::Crashed,
            Some(AttentionReason::ErrorIntervention),
        ),
        (AgentState::Stopped, None),
        (AgentState::Disconnected, None),
    ];
    for (state, want) in expected {
        assert_eq!(
            &attention_for(*state),
            want,
            "attention for {state:?} must match"
        );
    }
}

#[test]
fn dashboard_and_summary_counts_with_live_agents() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("w", "/tmp").unwrap();
    let done = spawn_fake(&mut m, "completion");
    let failed = spawn_fake(&mut m, "failure");
    let approval = spawn_fake(&mut m, "approval");
    let working = spawn_fake(&mut m, "long-running");

    assert!(
        drain_until(&mut m, &done, "Completed", 20000),
        "completion agent"
    );
    assert!(
        drain_until(&mut m, &failed, "Failed", 20000),
        "failure agent"
    );
    assert!(
        drain_until(&mut m, &approval, "NeedsApproval", 20000),
        "approval agent"
    );
    // long-running emits until stdin closes; must at least reach Working.
    assert!(
        drain_until(&mut m, &working, "Working", 20000),
        "working agent"
    );

    let d = m.agent_dashboard(AgentFilter::All);
    assert_eq!(d.total, 4, "dashboard sees all four agents");
    assert!(d.completed >= 1, "completion counted: {}", d.completed);
    assert!(d.failed >= 1, "failure counted: {}", d.failed);
    assert!(d.needs_you >= 1, "approval needs you: {}", d.needs_you);
    assert!(d.running >= 1, "long-running counted: {}", d.running);

    let need = m.agent_dashboard(AgentFilter::NeedsAttention);
    assert_eq!(need.rows.len(), 2, "approval + failed both need attention");
    assert!(
        need.rows
            .iter()
            .all(|r| matches!(r.snapshot.state.as_str(), "NeedsApproval" | "Failed")),
        "filtered rows are exactly the attention-needing agents"
    );

    // Per-workspace summary mirrors the dashboard math; needs_you overlaps
    // with failed (attention includes ErrorIntervention).
    let s = m.workspace_agent_summary();
    assert_eq!(s.agents, 4);
    assert_eq!(s.completed, 1, "completion agent");
    assert_eq!(s.failed, 1, "failure agent");
    assert_eq!(s.needs_you, 2, "approval + failed need attention");
    assert_eq!(s.running, 1, "long-running agent");

    // Allow the pending approval: agent proceeds and completes.
    m.agent_runtime()
        .respond_permission(&approval, PermissionDecision::AllowOnce)
        .unwrap();
    assert!(
        drain_until(&mut m, &approval, "Completed", 20000),
        "approval allowed"
    );
    let d2 = m.agent_dashboard(AgentFilter::All);
    assert_eq!(d2.failed, 1, "the failure agent is still failed");
    assert_eq!(d2.needs_you, 1, "failed agents still need attention");
    assert_eq!(d2.completed, 2, "both extra agents completed");
}

// ---------------------------------------------------------------------------
// Usage / pricing (§27, §34 "Usage" / "Pricing")
// ---------------------------------------------------------------------------

#[test]
fn pricing_estimates_only_with_known_data() {
    let reg = PricingRegistry::new();
    let zero = AgentUsage::default();
    let full = AgentUsage {
        input_tokens: Some(1_000_000),
        output_tokens: Some(100_000),
        cached_tokens: Some(500_000),
        ..Default::default()
    };
    // Known model → estimate.
    let est = reg
        .estimate_cents("anthropic", "claude-sonnet-4-5", &full)
        .expect("known model has pricing");
    assert!(est > 0);
    // Unknown model / provider → None (never guess a price).
    assert!(reg
        .estimate_cents("anthropic", "claude-unknown-9", &full)
        .is_none());
    assert!(reg
        .estimate_cents("not-a-provider", "claude-sonnet-4-5", &full)
        .is_none());
    // Zero usage → no meaningful cost.
    assert!(reg
        .estimate_cents("anthropic", "claude-sonnet-4-5", &zero)
        .is_none());
    // Partial usage (input weighted more than output) → estimate grows with tokens.
    let partial = AgentUsage {
        input_tokens: Some(2_000_000),
        output_tokens: Some(100_000),
        ..Default::default()
    };
    let p1 = reg.estimate_cents("anthropic", "claude-sonnet-4-5", &partial);
    let more = AgentUsage {
        input_tokens: Some(4_000_000),
        output_tokens: Some(100_000),
        ..Default::default()
    };
    let p2 = reg.estimate_cents("anthropic", "claude-sonnet-4-5", &more);
    // 2M×300/1M + 100k×1500/1M = 600 + 150; 4M×300/1M = 1200 + 150.
    assert_eq!(p1, Some(750));
    assert_eq!(p2, Some(1350));
}

// ---------------------------------------------------------------------------
// Health (§23, §34 "Health")
// ---------------------------------------------------------------------------

#[test]
fn health_rows_are_present_and_secret_free() {
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("w", "/tmp").unwrap();
    let rows = m.agent_runtime().health();
    assert!(
        !rows.is_empty(),
        "builtin agent definitions yield health rows"
    );
    for row in &rows {
        assert!(!row.definition_id.is_empty());
        assert!(!row.display_name.is_empty());
        // Never leak credential material into health details.
        for needle in ["sk-", "api_key=", "key=", "Bearer "] {
            assert!(
                !row.detail.to_lowercase().contains(&needle.to_lowercase()),
                "health detail must not contain {needle:?}: {:?}",
                row.detail
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Replay (§11 / §28, §34 "Replay")
// ---------------------------------------------------------------------------

#[test]
fn replay_fixtures_are_deterministic() {
    let fixtures = all_fixtures();
    assert!(!fixtures.is_empty());
    for (name, events) in &fixtures {
        assert!(!events.is_empty(), "{name} has events");
        let run = |w: &mut AgentWork| -> (Vec<AgentEvent>, Vec<String>) {
            let mut sink: Vec<AgentEvent> = Vec::new();
            replay_into(w, events, &mut |ev| sink.push(ev));
            (sink, w.timeline.iter().map(|e| e.detail.clone()).collect())
        };
        let mut a = AgentWork::new("replay", *name);
        let mut b = AgentWork::new("replay", *name);
        let (ev_a, tl_a) = run(&mut a);
        let (ev_b, tl_b) = run(&mut b);
        assert!(!ev_a.is_empty(), "{name} emits events");
        assert_eq!(ev_a.len(), ev_b.len(), "{name}: event count deterministic");
        assert_eq!(tl_a, tl_b, "{name}: timeline deterministic");
        assert_eq!(a.summary().files_changed, b.summary().files_changed);
    }
}

// ---------------------------------------------------------------------------
// Intent (§34 "Intent")
// ---------------------------------------------------------------------------

#[test]
fn intent_resolution_is_deterministic() {
    use AgentIntent::*;
    let cases: &[(&str, AgentIntent)] = &[
        (
            "show agents",
            ShowAgents {
                filter: AgentFilter::All,
            },
        ),
        (
            "show failed agents",
            ShowAgents {
                filter: AgentFilter::Failed,
            },
        ),
        (
            "agents needing approval",
            ShowAgents {
                filter: AgentFilter::NeedingApproval,
            },
        ),
        (
            "show agents needing input",
            ShowAgents {
                filter: AgentFilter::NeedingInput,
            },
        ),
        (
            "focus claude",
            FocusAgent {
                name: "claude".into(),
            },
        ),
        ("open agent logs", OpenAgentLogs),
        ("review my changes", ReviewChanges),
        ("stop the agent", StopAgent),
        ("restart it", RestartAgent),
        ("resume", ResumeAgent),
        ("approve", Approve),
        ("deny", Deny),
    ];
    for (query, want) in cases {
        assert_eq!(
            IntentResolver::resolve(query).as_ref(),
            Some(want),
            "resolve({query:?})"
        );
    }
    assert_eq!(IntentResolver::resolve("xyzzy plugh"), None);
    assert_eq!(IntentResolver::resolve(""), None);
}

// ---------------------------------------------------------------------------
// Command palette coverage (§16, §34 "Dashboard" plumbing)
// ---------------------------------------------------------------------------

#[test]
fn palette_covers_all_phase2c_commands() {
    let reg = terminal_workspace::command::CommandRegistry::with_defaults();
    let palette = reg.palette();
    for cmd in [
        "Show Agents",
        "Focus Agent",
        "Show Agents Needing Attention",
        "Show Failed Agents",
        "Show Completed Agents",
        "Review Agent Changes",
        "Open Agent Logs",
        "Stop Agent",
        "Restart Agent",
        "Resume Agent",
    ] {
        assert!(
            palette.iter().any(|c| c.to_label() == cmd),
            "palette must offer `{cmd}`"
        );
    }
    // Every palette entry executes through run_command — the desktop routes
    // every palette selection through the same dispatch as key bindings.
    let bound: Vec<&str> = palette.iter().map(|c| c.to_label()).collect();
    assert!(bound.len() >= 22, "palette is not empty: {}", bound.len());
}

// ---------------------------------------------------------------------------
// Notifications + quiet mode (§18–19, §34 "Notifications" / "QuietMode")
// ---------------------------------------------------------------------------

#[test]
fn approval_notifications_fire_once_and_respect_quiet_mode() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("w", "/tmp").unwrap();
    let counts = Arc::new(AtomicUsize::new(0));
    let counts2 = Arc::clone(&counts);
    m.notifications.subscribe(Arc::new(move |n: Notification| {
        if matches!(n.kind, NotificationKind::AgentNeedsApproval { .. }) {
            counts2.fetch_add(1, Ordering::SeqCst);
        }
    }));

    // Default prefs: needs-me notifications ON.
    let eid = spawn_fake(&mut m, "approval");
    assert!(drain_until(&mut m, &eid, "NeedsApproval", 20000));
    assert_eq!(
        counts.load(Ordering::SeqCst),
        1,
        "one approval notification"
    );
    // Dedup: repeated drains must not re-notify the same state.
    for _ in 0..5 {
        let _ = m.drain_frame();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(
        counts.load(Ordering::SeqCst),
        1,
        "no duplicate notifications"
    );
    // Quiet mode: needs-me OFF → a second approval agent stays silent.
    m.set_notification_prefs(&NotificationPrefs {
        on_needs_me: false,
        on_failure: false,
        on_completion: false,
        on_start: false,
    });
    let eid2 = spawn_fake(&mut m, "approval");
    assert!(
        drain_until(&mut m, &eid2, "NeedsApproval", 20000),
        "second agent reaches NeedsApproval"
    );
    assert_eq!(
        counts.load(Ordering::SeqCst),
        1,
        "quiet mode suppresses approval notifications"
    );
}

#[test]
fn quiet_mode_prefs_persist_across_restore() {
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("w", "/tmp").unwrap();
    assert_eq!(
        m.notification_prefs(),
        NotificationPrefs::default(),
        "defaults: needs-me + failure on"
    );
    m.set_notification_prefs(&NotificationPrefs {
        on_needs_me: false,
        on_failure: true,
        on_completion: true,
        on_start: false,
    });
    let state = m.snapshot_state();
    let mut m2 = Multiplexer::new().unwrap();
    m2.create_workspace("w", "/tmp").unwrap();
    m2.restore(state);
    assert_eq!(
        m2.notification_prefs(),
        NotificationPrefs {
            on_needs_me: false,
            on_failure: true,
            on_completion: true,
            on_start: false,
        },
        "quiet-mode prefs survive snapshot/restore"
    );
}

// ---------------------------------------------------------------------------
// Persistence (§34 "Persistence")
// ---------------------------------------------------------------------------

#[test]
fn work_serializes_roundtrip() {
    let mut w = AgentWork::new("exec-9", "persist me");
    w.files_changed.insert("a.rs".into());
    w.commands.push("cargo build".into());
    w.push_activity(AgentActivityState::heuristic(ActivityKind::Planning, ""));
    let json = serde_json::to_string(&w).unwrap();
    let back: AgentWork = serde_json::from_str(&json).unwrap();
    assert_eq!(back.id, w.id);
    assert_eq!(back.summary().files_changed, 1);
    assert_eq!(
        back.current_activity().unwrap().kind,
        ActivityKind::Planning
    );
}
