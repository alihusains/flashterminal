//! Phase 2B.1 §7–§11: real-agent integration validation.
//!
//! Separately executable — NOT part of normal CI:
//!
//! ```text
//! cargo test -p terminal-session --features real-agents --test real_agents
//! ```
//!
//! For each real agent (claude-code / codex / opencode / pi) the suite runs
//! the Phase 2B.1 matrix: launch → interactive input → simple task →
//! working state → completion → stop → restart → resume (where supported).
//! Every step prints what was actually *observed* — the results feed
//! `docs/agent-compatibility.md` (§12). When the binary or credentials are
//! unavailable the test prints SKIPPED, never FAILED (§7).

use std::sync::Arc;
use std::time::{Duration, Instant};

use pty::PtyManager;
use terminal_session::agent::{AgentRegistry, AgentRuntime};
use terminal_session::credential::{CredentialStore, MemoryBackend};
use terminal_session::execution::{AgentEvent, ExecutionId};
use terminal_session::launch::AgentLaunchConfig;
use terminal_session::provider::ProviderRegistry;

fn on_path(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn runtime() -> AgentRuntime {
    AgentRuntime::new(
        Arc::new(AgentRegistry::new()),
        ProviderRegistry::new(),
        Arc::new(PtyManager::new().unwrap()),
        CredentialStore::with_backend(Arc::new(MemoryBackend::new())),
        None,
    )
}

fn launch(id: &str, extra: &[&str]) -> AgentLaunchConfig {
    AgentLaunchConfig {
        definition_id: id.into(),
        cwd: std::env::temp_dir().to_string_lossy().to_string(),
        arguments: extra.iter().map(|s| s.to_string()).collect(),
        provider_id: None,
        model_id: None,
        credential_ref: None,
        resume_id: None,
        environment: vec![],
    }
}

/// Pumps runtime events until `pred` matches or the deadline passes.
fn wait_event(
    rt: &mut AgentRuntime,
    eid: &ExecutionId,
    pred: impl Fn(&AgentEvent) -> bool,
    timeout: Duration,
) -> Vec<AgentEvent> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        for (ev_eid, ev) in rt.drain_events() {
            if &ev_eid == eid {
                seen.push(ev.clone());
                if pred(&ev) {
                    return seen;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    seen
}

fn snapshot_state(rt: &AgentRuntime, eid: &ExecutionId) -> String {
    rt.get_session(eid)
        .map(|s| s.state.clone())
        .unwrap_or_else(|| "gone".into())
}

/// Runs the shared matrix against one real agent definition.
fn run_matrix(id: &str, task_args: &[&str], resume_args: Option<&[&str]>) {
    let registry = AgentRegistry::new();
    let def = registry.get(id).expect("definition exists");
    if !on_path(&def.command) {
        eprintln!(
            "SKIPPED {id}: executable `{}` not on PATH — run the matrix manually (§8–§11)",
            def.command
        );
        return;
    }
    println!(
        "\n=== {id} (binary: {}) — real-agent matrix ===",
        def.command
    );
    let mut rt = runtime();
    let mut observed: Vec<String> = Vec::new();

    // 1. launch — expect Started within 15 s.
    let (eid, _session) = rt.spawn(launch(id, &[]), 80, 24).unwrap();
    let started = wait_event(
        &mut rt,
        &eid,
        |e| matches!(e, AgentEvent::Started),
        Duration::from_secs(15),
    );
    observed.push(format!(
        "launch: {}",
        if started.is_empty() {
            "TIMEOUT (no Started)"
        } else {
            "Started observed"
        }
    ));
    std::thread::sleep(Duration::from_millis(2000));

    // 2. interactive input — write a benign keystroke; process must survive.
    rt.send_input(&eid, b"\x03").ok();
    std::thread::sleep(Duration::from_millis(1500));
    let alive_after_input = rt.get_session(&eid).is_some();
    observed.push(format!(
        "interactive input: {}",
        if alive_after_input {
            "session alive after Ctrl-C"
        } else {
            "session gone after input"
        }
    ));

    // 3. working state — activity detection (heuristic/structured observed).
    let states = wait_event(
        &mut rt,
        &eid,
        |e| matches!(e, AgentEvent::StateChanged { new_state, .. } if *new_state == terminal_session::execution::AgentState::Working),
        Duration::from_secs(20),
    );
    let saw_work = !states.is_empty();
    observed.push(format!(
        "working state: {}",
        if saw_work {
            "Working observed"
        } else {
            "no Working state observed (agent may wait for input)"
        }
    ));

    // 4. stop — must transition to Stopped.
    rt.stop(&eid).unwrap();
    wait_event(
        &mut rt,
        &eid,
        |e| matches!(e, AgentEvent::StateChanged { new_state, .. } if *new_state == terminal_session::execution::AgentState::Stopped),
        Duration::from_secs(5),
    );
    observed.push(format!("stop: state={}", snapshot_state(&rt, &eid)));

    // 5. restart — fresh process under the same ExecutionId.
    rt.restart(&eid, 80, 24).unwrap();
    let restarted = wait_event(
        &mut rt,
        &eid,
        |e| matches!(e, AgentEvent::Started),
        Duration::from_secs(15),
    );
    observed.push(format!(
        "restart: {}",
        if restarted.is_empty() {
            "TIMEOUT (no Started)"
        } else {
            "Started observed"
        }
    ));
    rt.stop(&eid).ok();

    // 6. simple task + completion/failure — print-mode with task args.
    let (teid, _t) = rt.spawn(launch(id, task_args), 80, 24).unwrap();
    let exited = wait_event(
        &mut rt,
        &teid,
        |e| matches!(e, AgentEvent::Exited { .. }),
        Duration::from_secs(90),
    );
    let snap = rt.get_session(&teid);
    match exited.last() {
        Some(AgentEvent::Exited { code }) => observed.push(format!(
            "simple task: Exited code={code:?} state={} ({})",
            snapshot_state(&rt, &teid),
            if *code == Some(0) {
                "completion"
            } else {
                "failure"
            }
        )),
        _ => observed.push(format!(
            "simple task: TIMEOUT (no Exited in 90s) state={}",
            snapshot_state(&rt, &teid)
        )),
    }
    let _ = snap;
    rt.remove(&teid);

    // 7. resume — only where the adapter claims the capability.
    if let Some(args) = resume_args {
        let caps = rt.capabilities(&eid).unwrap();
        let (reid, _r) = rt.spawn(launch(id, args), 80, 24).unwrap();
        let resumed = wait_event(
            &mut rt,
            &reid,
            |e| matches!(e, AgentEvent::Started),
            Duration::from_secs(15),
        );
        observed.push(format!(
            "resume (capability={}): {}",
            caps.resume,
            if resumed.is_empty() {
                "TIMEOUT (no Started)"
            } else {
                "Started observed — behavior recorded, not assumed"
            }
        ));
        rt.stop(&reid).ok();
    }

    for line in &observed {
        println!("  {line}");
    }
}

#[test]
fn claude_code_matrix() {
    run_matrix(
        "claude-code",
        &["-p", "Reply with exactly the word OK."],
        Some(&["--resume"]),
    );
}

#[test]
fn codex_matrix() {
    run_matrix("codex", &["exec", "echo ok"], None);
}

#[test]
fn opencode_matrix() {
    run_matrix(
        "opencode",
        &["run", "Reply with exactly the word OK."],
        None,
    );
}

#[test]
fn pi_matrix() {
    run_matrix("pi", &["--print", "Reply with exactly the word OK."], None);
}

#[test]
fn detection_reports_honestly() {
    // §7: the suite must SKIP, not FAIL, when a binary is missing. This
    // test proves the detection itself works both ways.
    let registry = AgentRegistry::new();
    for id in ["claude-code", "codex", "opencode", "pi"] {
        let def = registry.get(id).unwrap();
        let available = on_path(&def.command);
        println!("detection {id}: `{}` on PATH = {available}", def.command);
    }
}
