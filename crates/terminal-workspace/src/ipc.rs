//! IPC protocol (§31–32).
//!
//! A clean Request/Response/Event protocol over a Unix domain socket.
//! Messages are length-prefixed JSON lines. The protocol is deliberately
//! agent-friendly: command names are action-oriented strings
//! (`pane.create`, `workspace.create`), so future agent/runtime control can
//! reuse the exact same surface without a new API.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::events::EventFilter;
use crate::model::{PaneId, SplitDirection, WorkspaceId};
use terminal_session::execution::ApplicationEvent;
use terminal_session::launch::AgentLaunchConfig;

/// Default control socket path (overridable via env for tests).
pub fn default_socket_path() -> std::path::PathBuf {
    std::env::var("FLASHTERMINAL_SOCKET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
            std::path::PathBuf::from(dir).join("flashterminal.sock")
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: PaneId,
    pub title: String,
    pub cwd: String,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    pub active: bool,
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: WorkspaceId,
    pub name: String,
    pub project_root: String,
    pub active: bool,
    pub tabs: Vec<TabInfo>,
}

/// Client → application commands (Phase 1 & 2A subset).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    WorkspaceList,
    WorkspaceCreate {
        name: String,
        project_root: String,
    },
    WorkspaceOpen {
        workspace_id: WorkspaceId,
    },
    WorkspaceRename {
        workspace_id: WorkspaceId,
        name: String,
    },
    WorkspaceClose {
        workspace_id: WorkspaceId,
    },
    TabCreate,
    TabClose {
        tab_id: String,
    },
    PaneSplit {
        direction: SplitDirection,
    },
    PaneClose {
        pane_id: PaneId,
    },
    PaneFocus {
        pane_id: PaneId,
    },
    PaneList,
    // Phase 2A: Agent commands
    AgentList,
    AgentSpawn {
        definition_id: String,
    },
    AgentSpawnPane {
        definition_id: String,
        cwd: String,
        direction: SplitDirection,
    },
    AgentStatus {
        execution_id: String,
    },
    AgentStop {
        execution_id: String,
    },
    AgentRestart {
        execution_id: String,
    },
    AgentResume {
        execution_id: String,
    },
    AgentPause {
        execution_id: String,
    },
    /// Responds to an agent permission prompt (Phase 2B.1 §17–18). Routed
    /// through the runtime — clients never write to the process directly.
    AgentPermission {
        execution_id: String,
        decision: terminal_session::agent::PermissionDecision,
    },
    // Phase 2B.1: event streaming (§25)
    Subscribe {
        filter: EventFilter,
    },
    Unsubscribe {
        subscription_id: u64,
    },
    Ping,
}

/// Application → client responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok {
        message: String,
    },
    Workspaces {
        workspaces: Vec<WorkspaceInfo>,
    },
    Panes {
        panes: Vec<PaneInfo>,
    },
    Agents {
        agents: Vec<AgentInfo>,
    },
    AgentStatus {
        agent: AgentInfo,
    },
    /// Ack for a `Subscribe` request; events follow as a stream of `Event`
    /// frames on the same connection until it closes.
    Subscribed {
        subscription_id: u64,
    },
    Err {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub execution_id: String,
    pub definition_id: String,
    pub display_name: String,
    pub state: String,
    pub activity: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub duration_secs: Option<i64>,
}

impl From<terminal_session::agent::AgentSnapshot> for AgentInfo {
    fn from(s: terminal_session::agent::AgentSnapshot) -> Self {
        Self {
            execution_id: s.execution_id,
            definition_id: s.definition_id,
            display_name: s.display_name,
            state: s.state,
            activity: s.activity,
            cwd: s.cwd,
            exit_code: s.exit_code,
            duration_secs: s.duration_secs,
        }
    }
}

impl Response {
    pub fn err(message: impl Into<String>) -> Self {
        Response::Err {
            message: message.into(),
        }
    }
}

/// Server → client events (async notifications over the same socket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PaneCreated {
        pane_id: PaneId,
    },
    PaneClosed {
        pane_id: PaneId,
    },
    SessionExited {
        pane_id: PaneId,
    },
    /// A raw application-bus event (Phase 2B.1 §24–27). All payloads are
    /// redacted at the source — the bus never carries credentials.
    Application {
        event: ApplicationEvent,
    },
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

const MAX_MSG: usize = 16 * 1024 * 1024;

fn write_msg(stream: &mut UnixStream, msg: &[u8]) -> Result<()> {
    let len = (msg.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(msg)?;
    stream.flush()?;
    Ok(())
}

fn read_msg(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_MSG {
        bail!("message too large: {len}");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// One request/response round-trip over a Unix socket.
pub fn roundtrip(socket: &Path, request: &Request) -> Result<Response> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    let json = serde_json::to_vec(request)?;
    write_msg(&mut stream, &json)?;
    let resp = read_msg(&mut stream)?;
    let resp: Response = serde_json::from_slice(&resp)?;
    Ok(resp)
}

/// Builds a [`WorkspaceInfo`] snapshot from the engine (used by the server
/// and the CLI). Lives here to keep the protocol module self-contained.
pub fn describe(engine: &crate::engine::Multiplexer) -> Vec<WorkspaceInfo> {
    let active_ws = engine.active_workspace();
    engine
        .workspaces()
        .iter()
        .map(|ws| {
            let active = ws.id == active_ws.id;
            let tabs = ws
                .tabs
                .iter()
                .map(|t| {
                    let mut panes = Vec::new();
                    t.root.panes(&mut panes);
                    let focused = t.active_pane.clone();
                    TabInfo {
                        id: t.id.clone(),
                        title: t.title.clone(),
                        active: ws.active_tab.as_deref() == Some(&t.id),
                        panes: panes
                            .into_iter()
                            .map(|p| PaneInfo {
                                id: p.id.clone(),
                                title: p.title.clone(),
                                cwd: p.cwd.clone(),
                                focused: focused.as_deref() == Some(&p.id),
                            })
                            .collect(),
                    }
                })
                .collect();
            WorkspaceInfo {
                id: ws.id.clone(),
                name: ws.name.clone(),
                project_root: ws.project_root.clone(),
                active,
                tabs,
            }
        })
        .collect()
}

/// Handles one request against the engine. Pure function — the server loop
/// just feeds it connections.
pub fn handle(engine: &mut crate::engine::Multiplexer, req: Request) -> Response {
    // The engine invariant is at least one workspace exists; create the
    // default one lazily before tab/pane operations (workspace create/list
    // handle their own cases).
    if !matches!(
        req,
        Request::WorkspaceList | Request::WorkspaceCreate { .. } | Request::Ping
    ) {
        engine.ensure_workspace();
    }
    match req {
        Request::Ping => Response::Ok {
            message: "pong".into(),
        },
        Request::WorkspaceList => Response::Workspaces {
            workspaces: describe(engine),
        },
        Request::WorkspaceCreate { name, project_root } => {
            match engine.create_workspace(&name, &project_root) {
                Ok(id) => Response::Ok {
                    message: format!("created workspace {id}"),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::WorkspaceOpen { workspace_id } => match engine.switch_workspace(&workspace_id) {
            Ok(()) => Response::Ok {
                message: format!("opened workspace {workspace_id}"),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::WorkspaceRename { workspace_id, name } => {
            match engine.rename_workspace(&workspace_id, &name) {
                Ok(()) => Response::Ok {
                    message: format!("renamed workspace to {name}"),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::WorkspaceClose { workspace_id } => match engine.close_workspace(&workspace_id) {
            Ok(()) => Response::Ok {
                message: "workspace closed".into(),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::TabCreate => match engine.new_tab() {
            Ok(id) => Response::Ok {
                message: format!("created tab {id}"),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::TabClose { tab_id } => match engine.close_tab(&tab_id) {
            Ok(()) => Response::Ok {
                message: "tab closed".into(),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::PaneSplit { direction } => match engine.split_pane(direction) {
            Ok(id) => Response::Ok {
                message: format!("created pane {id}"),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::PaneClose { pane_id } => match engine.close_pane(&pane_id) {
            Ok(()) => Response::Ok {
                message: "pane closed".into(),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::PaneFocus { pane_id } => match engine.focus_pane(&pane_id) {
            Ok(()) => Response::Ok {
                message: format!("focused pane {pane_id}"),
            },
            Err(e) => Response::err(e.to_string()),
        },
        Request::PaneList => {
            let ws = engine.active_workspace();
            let panes = ws
                .active_tab()
                .map(|t| {
                    let mut v = Vec::new();
                    t.root.panes(&mut v);
                    let focused = t.active_pane.clone();
                    v.into_iter()
                        .map(|p| PaneInfo {
                            id: p.id.clone(),
                            title: p.title.clone(),
                            cwd: p.cwd.clone(),
                            focused: focused.as_deref() == Some(&p.id),
                        })
                        .collect()
                })
                .unwrap_or_default();
            Response::Panes { panes }
        }
        // Phase 2A/2B: Agent commands
        Request::AgentList => {
            let agents: Vec<AgentInfo> = engine
                .agent_runtime()
                .list_sessions()
                .into_iter()
                .map(AgentInfo::from)
                .collect();
            Response::Agents { agents }
        }
        Request::AgentSpawn { definition_id } => {
            let launch = AgentLaunchConfig {
                definition_id,
                cwd: "/".to_string(),
                arguments: vec![],
                provider_id: None,
                model_id: None,
                credential_ref: None,
                resume_id: None,
                environment: vec![],
            };
            match engine.spawn_agent_session(launch, 80, 24) {
                Ok(eid) => Response::Ok {
                    message: format!("spawned agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentSpawnPane {
            definition_id,
            cwd,
            direction,
        } => {
            let launch = AgentLaunchConfig {
                definition_id,
                cwd,
                arguments: vec![],
                provider_id: None,
                model_id: None,
                credential_ref: None,
                resume_id: None,
                environment: vec![],
            };
            match engine.split_pane_agent(direction, launch) {
                Ok(pane_id) => Response::Ok {
                    message: format!("spawned agent pane {pane_id}"),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentStatus { execution_id } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            if let Some(session) = engine.agent_runtime().get_session(&eid) {
                Response::AgentStatus {
                    agent: AgentInfo::from(session),
                }
            } else {
                Response::err(format!("Agent {} not found", eid.0))
            }
        }
        Request::AgentStop { execution_id } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            match engine.agent_runtime_mut().stop(&eid) {
                Ok(()) => Response::Ok {
                    message: format!("stopped agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentRestart { execution_id } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            match engine.restart_agent_session(&eid) {
                Ok(()) => Response::Ok {
                    message: format!("restarted agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentResume { execution_id } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            match engine.resume_agent_session(&eid) {
                Ok(()) => Response::Ok {
                    message: format!("resumed agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentPause { execution_id } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            match engine.pause_agent_session(&eid) {
                Ok(()) => Response::Ok {
                    message: format!("paused agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::AgentPermission {
            execution_id,
            decision,
        } => {
            let eid = terminal_session::execution::ExecutionId(execution_id);
            match engine.agent_runtime().respond_permission(&eid, decision) {
                Ok(()) => Response::Ok {
                    message: format!("permission response sent to agent {}", eid.0),
                },
                Err(e) => Response::err(e.to_string()),
            }
        }
        Request::Unsubscribe { subscription_id } => {
            if engine.events.unsubscribe(subscription_id) {
                Response::Ok {
                    message: format!("unsubscribed {subscription_id}"),
                }
            } else {
                Response::err(format!("subscription {subscription_id} not found"))
            }
        }
        // Never reaches `handle` (serve() intercepts Subscribe for the
        // streaming connection) — defensive only.
        Request::Subscribe { .. } => Response::err("subscribe is handled on the connection"),
    }
}

/// Runs a Unix-socket control server on `socket_path` driving `engine`
/// (behind a mutex so the UI thread and the server share it). Blocks until
/// the listener errors; the socket file is removed when it stops.
///
/// A `Request::Subscribe` keeps the connection open and streams `Event`
/// frames until the client disconnects (Phase 2B.1 §25). Every other
/// request is one-shot request/response.
pub fn serve(
    engine: std::sync::Arc<std::sync::Mutex<crate::engine::Multiplexer>>,
    socket_path: &Path,
) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    let path = socket_path.to_path_buf();
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    // A client that stops reading would otherwise wedge the
                    // connection thread forever in a blocking write; a
                    // write that cannot complete quickly aborts the
                    // connection (slow-client policy, §27).
                    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(100)));
                    let engine = Arc::clone(&engine);
                    thread::spawn(move || {
                        let Ok(req) = read_msg(&mut stream).and_then(|b| {
                            serde_json::from_slice::<Request>(&b).context("parse request")
                        }) else {
                            return;
                        };
                        if let Request::Subscribe { filter } = req {
                            // Streaming connection: ack, then push events.
                            let (id, rx) = {
                                let mut eng = engine.lock().expect("engine lock");
                                eng.events.subscribe(filter)
                            };
                            let ack = Response::Subscribed {
                                subscription_id: id,
                            };
                            let json = serde_json::to_vec(&ack).unwrap_or_default();
                            if write_msg(&mut stream, &json).is_err() {
                                return;
                            }
                            loop {
                                match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                                    Ok(event) => {
                                        let frame =
                                            serde_json::to_vec(&Event::Application { event })
                                                .unwrap_or_default();
                                        if write_msg(&mut stream, &frame).is_err() {
                                            break;
                                        }
                                    }
                                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                                    // Disconnected: the bus removed us
                                    // (unsubscribe or slow-client policy).
                                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                                }
                            }
                            // Best-effort cleanup if the bus didn't already.
                            let mut eng = engine.lock().expect("engine lock");
                            eng.events.unsubscribe(id);
                            return;
                        }
                        let resp = {
                            let mut eng = engine.lock().expect("engine lock");
                            handle(&mut eng, req)
                        };
                        let json = serde_json::to_vec(&resp).unwrap_or_default();
                        let _ = write_msg(&mut stream, &json);
                    });
                }
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_file(&path);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serde_snake_case() {
        let r = Request::PaneSplit {
            direction: SplitDirection::Horizontal,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("pane_split"));
        assert!(json.contains("\"type\":\"pane_split\""));
    }

    #[test]
    fn socket_roundtrip_ping() {
        let engine = std::sync::Arc::new(std::sync::Mutex::new(
            crate::engine::Multiplexer::new().unwrap(),
        ));
        let path = std::env::temp_dir().join(format!("ft-ipc-{}.sock", std::process::id()));
        serve(engine, &path).unwrap();
        // Give the listener a moment to bind.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let resp = roundtrip(&path, &Request::Ping).unwrap();
        assert!(matches!(resp, Response::Ok { .. }));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn socket_create_workspace_and_list() {
        let engine = std::sync::Arc::new(std::sync::Mutex::new(
            crate::engine::Multiplexer::new().unwrap(),
        ));
        let path = std::env::temp_dir().join(format!("ft-ipc2-{}.sock", std::process::id()));
        serve(engine, &path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let r1 = roundtrip(
            &path,
            &Request::WorkspaceCreate {
                name: "cli".into(),
                project_root: "/tmp".into(),
            },
        )
        .unwrap();
        assert!(matches!(r1, Response::Ok { .. }));
        let r2 = roundtrip(&path, &Request::WorkspaceList).unwrap();
        match r2 {
            Response::Workspaces { workspaces } => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].name, "cli");
                assert_eq!(workspaces[0].tabs.len(), 1);
                assert_eq!(workspaces[0].tabs[0].panes.len(), 1);
            }
            _ => panic!("expected workspaces response"),
        }
        let _ = std::fs::remove_file(&path);
    }
}
