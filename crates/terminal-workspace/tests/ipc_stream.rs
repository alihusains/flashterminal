//! Phase 2B.1: IPC event streaming (§24–27) and secret safety (§28).
//!
//! Exercises the full path: AgentRuntime → ApplicationEvent → EventBus →
//! IPC server → socket client, and proves sentinel secrets never cross it.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use terminal_session::redact::Redactor;
use terminal_workspace::events::EventFilter;
use terminal_workspace::ipc::{self, Event, Request, Response};
use terminal_workspace::Multiplexer;

fn tmp_socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ft-{name}-{}.sock", std::process::id()))
}

fn read_frame(stream: &mut UnixStream) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame(stream: &mut UnixStream, msg: &[u8]) -> anyhow::Result<()> {
    let len = (msg.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(msg)?;
    stream.flush()?;
    Ok(())
}

fn subscribe(stream: &mut UnixStream, filter: EventFilter) -> anyhow::Result<u64> {
    write_frame(stream, &serde_json::to_vec(&Request::Subscribe { filter })?)?;
    let resp: Response = serde_json::from_slice(&read_frame(stream)?)?;
    match resp {
        Response::Subscribed { subscription_id } => Ok(subscription_id),
        other => anyhow::bail!("subscribe failed: {other:?}"),
    }
}

/// Reads frames until an `AgentEvent::Exited` for the given execution id
/// arrives (or timeout); returns all agent events received, redacted-text
/// only (payloads are already redacted server-side).
fn collect_until_exit(
    stream: &mut UnixStream,
    execution_prefix: &str,
    timeout: Duration,
) -> Vec<terminal_session::execution::AgentEvent> {
    use terminal_session::execution::{AgentEvent, ApplicationEvent};
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    while Instant::now() < deadline {
        match read_frame(stream) {
            Ok(bytes) => {
                let Ok(Event::Application { event }) = serde_json::from_slice::<Event>(&bytes)
                else {
                    continue;
                };
                let ApplicationEvent::AgentEvent {
                    execution_id,
                    event,
                } = event
                else {
                    continue;
                };
                if !execution_id.0.starts_with(execution_prefix) {
                    continue;
                }
                match &event {
                    AgentEvent::Exited { .. } => {
                        out.push(event);
                        return out;
                    }
                    _ => out.push(event),
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn drain_loop(engine: Arc<Mutex<Multiplexer>>, stop: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok(mut eng) = engine.lock() {
                eng.drain_frame();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
}

fn agent_launch(scenario: &str, cwd: &str) -> terminal_session::launch::AgentLaunchConfig {
    terminal_session::launch::AgentLaunchConfig {
        definition_id: "fake-agent".into(),
        cwd: cwd.into(),
        arguments: vec!["--scenario".into(), scenario.into()],
        provider_id: None,
        model_id: None,
        credential_ref: None,
        resume_id: None,
        environment: vec![],
    }
}

fn fake_agent_available() -> bool {
    terminal_session::adapters::fake::FakeAgentAdapter::resolve_binary().is_ok()
}

#[test]
fn agent_events_stream_live_to_subscriber() {
    if !fake_agent_available() {
        eprintln!("SKIPPED: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let path = tmp_socket("stream");
    let engine = Arc::new(Mutex::new(Multiplexer::new().unwrap()));
    ipc::serve(Arc::clone(&engine), &path).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // dran_frame requires the engine invariant of ≥1 workspace (the desktop
    // guarantees this in init_engine).
    engine.lock().unwrap().ensure_workspace();

    // Subscribe FIRST so no lifecycle event is missed.
    let mut stream = UnixStream::connect(&path).unwrap();
    let id = subscribe(&mut stream, EventFilter::agent_only()).unwrap();
    assert!(id > 0);

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    drain_loop(Arc::clone(&engine), Arc::clone(&stop));

    // Spawn a working agent through the real IPC surface.
    let resp = ipc::roundtrip(
        &path,
        &Request::AgentSpawnPane {
            definition_id: "fake-agent".into(),
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            direction: terminal_workspace::model::SplitDirection::Vertical,
        },
    )
    .unwrap();
    let execution_id = match resp {
        Response::Ok { .. } => {
            let agents = match ipc::roundtrip(&path, &Request::AgentList).unwrap() {
                Response::Agents { agents } => agents,
                _ => panic!("expected agents"),
            };
            agents[0].execution_id.clone()
        }
        other => panic!("spawn failed: {other:?}"),
    };

    // Stream until the agent exits — no polling.
    let events = collect_until_exit(&mut stream, &execution_id, Duration::from_secs(30));
    use terminal_session::execution::{AgentEvent, AgentState};
    let states: Vec<AgentState> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::StateChanged { new_state, .. } => Some(*new_state),
            _ => None,
        })
        .collect();
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Started)),
        "stream must carry Started: {events:?}"
    );
    assert!(
        states.contains(&AgentState::Working) || states.contains(&AgentState::Starting),
        "stream must carry activity states: {states:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Completed)),
        "stream must carry Completed: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Exited { code: Some(0) })),
        "stream must carry a clean Exited"
    );

    // Every payload crossing the socket must be secret-free (no sk- shapes
    // in the streamed frames even from the raw transcript).
    let mut frame_ok = true;
    for ev in &events {
        if let AgentEvent::Output { text } = ev {
            frame_ok &= Redactor::is_secret_free(text);
        }
    }
    assert!(frame_ok, "streamed output frames contain redaction gaps");

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn sentinel_secret_never_reaches_events_or_persistence() {
    if !fake_agent_available() {
        eprintln!("SKIPPED: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    const SENTINEL: &str = "SUPER_SECRET_TEST_VALUE_2B1";
    Redactor::register_secret(SENTINEL);

    let path = tmp_socket("secret");
    let engine = Arc::new(Mutex::new(Multiplexer::new().unwrap()));
    ipc::serve(Arc::clone(&engine), &path).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    engine.lock().unwrap().ensure_workspace();

    // The agent itself emits the sentinel (`--echo`); it must be masked by
    // the pipeline before it reaches events, logs, or persistence.
    let mut launch = agent_launch("completion", &std::env::temp_dir().to_string_lossy());
    launch.arguments = vec![
        "--scenario".into(),
        "completion".into(),
        "--echo".into(),
        SENTINEL.into(),
    ];
    let resp = {
        let mut eng = engine.lock().unwrap();
        eng.ensure_workspace();
        eng.split_pane_agent(terminal_workspace::model::SplitDirection::Vertical, launch)
    };
    assert!(resp.is_ok());
    let _pane_id = resp.unwrap();

    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    drain_loop(Arc::clone(&engine), Arc::clone(&stop));

    // 1. Event stream.
    let mut stream = UnixStream::connect(&path).unwrap();
    subscribe(&mut stream, EventFilter::all()).unwrap();
    let mut all_frames = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Ok(bytes) = read_frame(&mut stream) {
            all_frames.push_str(&String::from_utf8_lossy(&bytes));
            if all_frames.contains("exited") && all_frames.contains("completed") {
                break;
            }
        }
    }
    assert!(
        !all_frames.contains(SENTINEL),
        "sentinel leaked into the IPC event stream"
    );

    // 2. Persisted state (snapshot + file).
    let (state_json, file_json) = {
        let eng = engine.lock().unwrap();
        let state_json = serde_json::to_string(&eng.snapshot_state()).unwrap();
        let path =
            std::env::temp_dir().join(format!("ft-secret-state-{}.json", std::process::id()));
        eng.save(&path).unwrap();
        let file_json = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        (state_json, file_json)
    };
    assert!(
        !state_json.contains(SENTINEL),
        "sentinel leaked into snapshot_state"
    );
    assert!(
        !file_json.contains(SENTINEL),
        "sentinel leaked into state.json"
    );

    // 3. Agent snapshots.
    {
        let eng = engine.lock().unwrap();
        for snap in eng.agent_runtime().list_sessions() {
            let json = serde_json::to_string(&snap).unwrap();
            assert!(
                !json.contains(SENTINEL),
                "sentinel leaked into AgentSnapshot"
            );
        }
    }

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    Redactor::unregister_secret(SENTINEL);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn slow_ipc_client_is_disconnected_and_engine_keeps_running() {
    if !fake_agent_available() {
        eprintln!("SKIPPED: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let path = tmp_socket("slow");
    let engine = Arc::new(Mutex::new(Multiplexer::new().unwrap()));
    ipc::serve(Arc::clone(&engine), &path).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    engine.lock().unwrap().ensure_workspace();

    // A client that subscribes and NEVER reads its socket.
    let mut stream = UnixStream::connect(&path).unwrap();
    subscribe(&mut stream, EventFilter::all()).unwrap();

    // Two heavy-output agents to flood the bus.
    for _ in 0..2 {
        let mut launch = agent_launch("large-output", &std::env::temp_dir().to_string_lossy());
        launch.cwd = std::env::temp_dir().to_string_lossy().to_string();
        let resp = {
            let mut eng = engine.lock().unwrap();
            eng.ensure_workspace();
            eng.split_pane_agent(terminal_workspace::model::SplitDirection::Vertical, launch)
        };
        assert!(resp.is_ok());
    }

    // Drive the engine and time the frames. The slow client must never
    // block the engine (bounded queues + coalescing) and must eventually be
    // disconnected by the slow-client policy.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut frames = 0usize;
    let mut stalled_frames = 0usize;
    let mut max_frame_ms = 0.0f64;
    while Instant::now() < deadline {
        let t0 = Instant::now();
        let changed = engine.lock().unwrap().drain_frame();
        let dt = t0.elapsed().as_secs_f64() * 1e3;
        max_frame_ms = max_frame_ms.max(dt);
        frames += 1;
        // A client-blocked engine would hang drain_frame for seconds; the
        // terminal-path apply cost of the flood sits well under this line.
        if dt > 1000.0 {
            stalled_frames += 1;
        }
        if changed.events_applied == 0 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let now = Instant::now();
        if frames.is_multiple_of(500) {
            let eng = engine.lock().unwrap();
            eprintln!(
                "DBG t+{:.1}s frames={frames} max={max_frame_ms:.1}ms subs={} stats={:?}",
                now.duration_since(deadline - Duration::from_secs(20))
                    .as_secs_f64(),
                eng.events.subscriber_count(),
                eng.events.subscriber_stats()
            );
        }
        if engine.lock().unwrap().events.subscriber_count() == 0 {
            break;
        }
    }
    assert!(
        frames >= 3,
        "engine must keep running with a slow client (frames={frames})"
    );
    assert_eq!(
        stalled_frames, 0,
        "engine frames stalled with a slow IPC client: max {max_frame_ms:.1}ms, {stalled_frames}/{frames}"
    );
    // The never-reading client must have been disconnected (bounded drops
    // → policy → receiver closed → server closes the socket).
    assert_eq!(
        engine.lock().unwrap().events.subscriber_count(),
        0,
        "slow client was never disconnected"
    );
    // The server must have closed the connection: drain whatever the kernel
    // already buffered for the wedged client, then reads must fail (EOF).
    let mut saw_close = false;
    for _ in 0..64 {
        match read_frame(&mut stream) {
            Ok(_) => continue,
            Err(_) => {
                saw_close = true;
                break;
            }
        }
    }
    assert!(
        saw_close,
        "server must close the connection after disconnecting the subscriber"
    );
    let _ = std::fs::remove_file(&path);
}
