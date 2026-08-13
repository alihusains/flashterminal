//! FlashTerminal CLI (§31): `terminal workspace list|create|open|rename|close`,
//! `terminal tab create|close`, `terminal pane split|close|focus|list`.
//!
//! Talks to a running FlashTerminal instance over the IPC Unix socket
//! (`$FLASHTERMINAL_SOCKET` or `/tmp/flashterminal.sock`). The low-level
//! capability is kept for power users while the GUI remains the primary
//! interface (§18).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use terminal_workspace::events::EventFilter;
use terminal_workspace::ipc::{self, Event, Request, Response};
use terminal_workspace::Multiplexer;
use terminal_workspace::SplitDirection;

const MAX_MSG: usize = 16 * 1024 * 1024;

/// Length-prefixed frame read (mirrors the server's framing).
fn read_frame(stream: &mut std::os::unix::net::UnixStream) -> Result<Vec<u8>> {
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

fn write_frame(stream: &mut std::os::unix::net::UnixStream, msg: &[u8]) -> Result<()> {
    let len = (msg.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(msg)?;
    stream.flush()?;
    Ok(())
}

/// `terminal agent watch` — subscribes to the live agent event stream and
/// prints events as they arrive. No polling: the server pushes frames
/// (Phase 2B.1 §26).
fn agent_watch(socket: &PathBuf, filter_execution: Option<&str>) -> Result<()> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connect to {}", socket.display()))?;
    let req = Request::Subscribe {
        filter: EventFilter::agent_only(),
    };
    write_frame(&mut stream, &serde_json::to_vec(&req)?)?;
    let resp: Response =
        serde_json::from_slice(&read_frame(&mut stream)?).context("parse subscription response")?;
    match resp {
        Response::Subscribed { subscription_id } => {
            println!("watching agent events (subscription {subscription_id}) — Ctrl+C to stop");
        }
        Response::Err { message } => bail!("subscribe failed: {message}"),
        other => bail!("subscribe failed: unexpected response {other:?}"),
    }
    loop {
        let bytes = match read_frame(&mut stream) {
            Ok(b) => b,
            Err(e) => bail!("stream closed: {e}"),
        };
        let Ok(event) = serde_json::from_slice::<Event>(&bytes) else {
            continue;
        };
        let Event::Application { event: app_event } = event else {
            continue;
        };
        print_agent_event(&app_event, filter_execution);
    }
}

fn print_agent_event(
    event: &terminal_workspace::terminal_session::execution::ApplicationEvent,
    filter_execution: Option<&str>,
) {
    use terminal_workspace::terminal_session::execution::{AgentEvent, ApplicationEvent};
    // For an execution-filtered watch: skip everything that isn't that agent.
    let matches = |run: &str| filter_execution.map(|f| run.starts_with(f)).unwrap_or(true);
    match event {
        ApplicationEvent::AgentEvent {
            execution_id,
            event,
        } => {
            if !matches(&execution_id.0) {
                return;
            }
            let short = &execution_id.0[..execution_id.0.len().min(8)];
            match event {
                AgentEvent::Started => println!("[{short}] agent started"),
                AgentEvent::StateChanged { new_state, .. } => {
                    println!("[{short}] state -> {new_state:?}")
                }
                AgentEvent::Output { text } => {
                    for line in text.lines().take(1) {
                        println!("[{short}] {line}");
                    }
                }
                AgentEvent::Error { message } => println!("[{short}] error: {message}"),
                AgentEvent::PermissionRequested { action, context } => {
                    println!("[{short}] PERMISSION REQUESTED ({action}): {context}")
                }
                AgentEvent::Completed => println!("[{short}] completed"),
                AgentEvent::Exited { code } => {
                    println!("[{short}] exited (code {code:?})");
                    std::process::exit(0);
                }
                AgentEvent::UsageUpdated { tokens } => {
                    println!("[{short}] usage updated ({tokens} tokens)")
                }
                AgentEvent::Activity { kind, detail, .. } => {
                    println!("[{short}] activity: {kind:?} {}", detail)
                }
            }
        }
        ApplicationEvent::SessionExited { execution_id, code } if matches(&execution_id.0) => {
            println!("[{}] session exited (code {code:?})", &execution_id.0[..8]);
        }
        _ => {}
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return Ok(());
    }
    let socket: PathBuf = ipc::default_socket_path();

    // `terminal serve` starts a headless control surface (engine + IPC
    // socket) so the CLI can be exercised end-to-end without the GUI, and
    // so automation can drive a running instance the same way.
    if args[0] == "serve" {
        let engine = Arc::new(Mutex::new(Multiplexer::new()?));
        println!("serving on {}", socket.display());
        ipc::serve(engine, &socket)?;
        println!("listening...");
        std::thread::park();
        return Ok(());
    }

    // Streaming command: `terminal agent watch [execution-id-prefix]`.
    if args[0] == "agent" && args.get(1).map(|s| s.as_str()) == Some("watch") {
        return agent_watch(&socket, args.get(2).map(|s| s.as_str()));
    }

    // Phase 2C dashboard: `terminal agents [filter]` and
    // `terminal agent work|timeline|review|health <id>`.
    if args[0] == "agents" {
        let request = Request::AgentDashboard {
            filter: parse_filter(args.get(1).map(|s| s.as_str()))?,
        };
        return roundtrip_and_print(&socket, request);
    }

    let request = match args[0].as_str() {
        "workspace" => workspace_cmd(&args[1..])?,
        "tab" => tab_cmd(&args[1..])?,
        "pane" => pane_cmd(&args[1..])?,
        "agent" => agent_cmd(&args[1..])?,
        "help" | "--help" | "-h" => {
            print_help();
            return Ok(());
        }
        other => bail!("unknown command `{other}` — try `terminal help`"),
    };

    match ipc::roundtrip(&socket, &request) {
        Ok(response) => print_response(response),
        Err(e) => {
            eprintln!(
                "terminal: cannot reach FlashTerminal at {} — is the app running? ({e})",
                socket.display()
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

fn agent_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal agent list|spawn|spawn-pane|status|stop|restart|resume|pause|permission|watch");
    };
    Ok(match cmd.as_str() {
        "list" => Request::AgentList,
        "spawn" => Request::AgentSpawn {
            definition_id: required(args, 1, "spawn <definition-id>")?,
        },
        "permission" => {
            let decision = match args.get(2).map(|s| s.as_str()) {
                Some("deny") => {
                    terminal_workspace::terminal_session::agent::PermissionDecision::Deny
                }
                Some("allow-once" | "once") => {
                    terminal_workspace::terminal_session::agent::PermissionDecision::AllowOnce
                }
                Some("allow") => {
                    terminal_workspace::terminal_session::agent::PermissionDecision::Allow
                }
                _ => {
                    bail!("usage: terminal agent permission <execution-id> <deny|allow-once|allow>")
                }
            };
            Request::AgentPermission {
                execution_id: required(args, 1, "permission <execution-id>")?,
                decision,
            }
        }
        "spawn-pane" => {
            let dir = match args.get(2).map(|s| s.as_str()) {
                Some("h" | "horizontal") => SplitDirection::Horizontal,
                Some("v" | "vertical") => SplitDirection::Vertical,
                _ => bail!(
                    "usage: terminal agent spawn-pane <definition-id> <horizontal|vertical> [cwd]"
                ),
            };
            let cwd = args.get(3).cloned().unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "/".into())
            });
            Request::AgentSpawnPane {
                definition_id: args[1].clone(),
                cwd,
                direction: dir,
            }
        }
        "status" => Request::AgentStatus {
            execution_id: required(args, 1, "status <execution-id>")?,
        },
        "stop" => Request::AgentStop {
            execution_id: required(args, 1, "stop <execution-id>")?,
        },
        "restart" => Request::AgentRestart {
            execution_id: required(args, 1, "restart <execution-id>")?,
        },
        "resume" => Request::AgentResume {
            execution_id: required(args, 1, "resume <execution-id>")?,
        },
        "pause" => Request::AgentPause {
            execution_id: required(args, 1, "pause <execution-id>")?,
        },
        "work" => Request::AgentWork {
            execution_id: required(args, 1, "work <execution-id>")?,
        },
        "timeline" => Request::AgentTimeline {
            execution_id: required(args, 1, "timeline <execution-id>")?,
        },
        "review" => Request::AgentReview {
            execution_id: required(args, 1, "review <execution-id>")?,
        },
        "health" => Request::AgentHealth,
        other => bail!("unknown agent command `{other}`"),
    })
}

fn workspace_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal workspace list|create|open|rename|close");
    };
    Ok(match cmd.as_str() {
        "list" => Request::WorkspaceList,
        "create" => {
            let name = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "untitled".to_string());
            let root = args.get(2).cloned().unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "/".into())
            });
            Request::WorkspaceCreate {
                name,
                project_root: root,
            }
        }
        "open" => Request::WorkspaceOpen {
            workspace_id: required(args, 1, "open <workspace-id>")?,
        },
        "rename" => Request::WorkspaceRename {
            workspace_id: required(args, 1, "rename <workspace-id> <name>")?,
            name: required(args, 2, "rename <workspace-id> <name>")?,
        },
        "close" => Request::WorkspaceClose {
            workspace_id: required(args, 1, "close <workspace-id>")?,
        },
        other => bail!("unknown workspace command `{other}`"),
    })
}

fn tab_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal tab create|close");
    };
    Ok(match cmd.as_str() {
        "create" => Request::TabCreate,
        "close" => Request::TabClose {
            tab_id: required(args, 1, "close <tab-id>")?,
        },
        other => bail!("unknown tab command `{other}`"),
    })
}

fn pane_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal pane split|close|focus|list");
    };
    Ok(match cmd.as_str() {
        "split" => {
            let dir = match args.get(1).map(|s| s.as_str()) {
                Some("h" | "horizontal") => SplitDirection::Horizontal,
                Some("v" | "vertical") => SplitDirection::Vertical,
                _ => bail!("usage: terminal pane split <horizontal|vertical>"),
            };
            Request::PaneSplit { direction: dir }
        }
        "close" => Request::PaneClose {
            pane_id: required(args, 1, "close <pane-id>")?,
        },
        "focus" => Request::PaneFocus {
            pane_id: required(args, 1, "focus <pane-id>")?,
        },
        "list" => Request::PaneList,
        other => bail!("unknown pane command `{other}`"),
    })
}

fn required(args: &[String], idx: usize, usage: &str) -> Result<String> {
    args.get(idx)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing argument: {usage}"))
}

/// Phase 2C: parses `terminal agents [filter]` filters (deterministic
/// dashboard filters, §13/§15).
fn parse_filter(f: Option<&str>) -> Result<terminal_workspace::terminal_session::work::AgentFilter> {
    use terminal_workspace::terminal_session::work::AgentFilter::*;
    Ok(match f.map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("all") => All,
        Some("needs-me" | "needs_you" | "attention" | "needing") => NeedsAttention,
        Some("running") => Running,
        Some("failed") => Failed,
        Some("completed" | "done") => Completed,
        Some("needs-input") => NeedingInput,
        Some("needs-approval") => NeedingApproval,
        Some(other) => bail!("unknown filter `{other}` (all|needs-me|running|failed|completed|needs-input|needs-approval)"),
    })
}

fn roundtrip_and_print(socket: &PathBuf, request: Request) -> Result<()> {
    match ipc::roundtrip(socket, &request) {
        Ok(response) => {
            print_response(response);
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "terminal: cannot reach FlashTerminal at {} — is the app running? ({e})",
                socket.display()
            );
            std::process::exit(1);
        }
    }
}

fn print_response(resp: Response) {
    match resp {
        Response::Ok { message } => println!("{message}"),
        Response::Err { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
        Response::Workspaces { workspaces } => {
            for ws in &workspaces {
                let marker = if ws.active { "*" } else { " " };
                println!("{marker} {} ({}) — {}", ws.name, ws.id, ws.project_root);
                for tab in &ws.tabs {
                    let tmarker = if tab.active { "*" } else { " " };
                    println!("  {tmarker} tab {} ({} panes)", tab.id, tab.panes.len());
                    for p in &tab.panes {
                        let fmarker = if p.focused { "*" } else { " " };
                        println!("    {fmarker} pane {} @ {} [{}]", p.id, p.cwd, p.title);
                    }
                }
            }
        }
        Response::Panes { panes } => {
            for p in panes {
                let marker = if p.focused { "*" } else { " " };
                println!("{marker} {} @ {} [{}]", p.id, p.cwd, p.title);
            }
        }
        Response::Agents { agents } => {
            for a in agents {
                println!(
                    "{} [{}] {} — {} ({})",
                    a.execution_id, a.definition_id, a.display_name, a.activity, a.state
                );
            }
        }
        Response::AgentStatus { agent } => {
            println!(
                "{} [{}] {} — {} ({})",
                agent.execution_id,
                agent.definition_id,
                agent.display_name,
                agent.activity,
                agent.state
            );
            if let Some(code) = agent.exit_code {
                println!("  exit code: {code}");
            }
            if let Some(secs) = agent.duration_secs {
                println!("  duration: {secs}s");
            }
            if let Some(atn) = &agent.attention {
                println!("  attention: {atn}");
            }
        }
        // --- Phase 2C printers (§13–§17, §30) ---
        Response::AgentDashboard { dashboard } => {
            println!(
                "agents: {} total · {} running · {} need you · {} failed · {} completed",
                dashboard.total,
                dashboard.running,
                dashboard.needs_you,
                dashboard.failed,
                dashboard.completed
            );
            for r in &dashboard.rows {
                let mark = match &r.snapshot.attention {
                    Some(a) => format!("▲ {}", a.label()),
                    None => format!("· {}", r.snapshot.work_status),
                };
                println!(
                    "  {} {} [{}] {}",
                    mark,
                    r.snapshot.execution_id,
                    r.snapshot.display_name,
                    r.snapshot.activity_detail
                );
                if let Some(pid) = &r.pane_id {
                    println!("    pane {pid}");
                }
            }
        }
        Response::AgentWork { work } => {
            let Some(w) = work else {
                println!("no work record for this execution");
                return;
            };
            println!(
                "work {} — {} [{}]",
                w.id,
                w.title,
                w.status.label()
            );
            if !w.description.is_empty() {
                println!("  description: {}", w.description);
            }
            if let Some(secs) = w.summary().duration_secs {
                println!("  duration: {secs}s");
            }
            if let (Some(inp), Some(out)) = (w.usage.input_tokens, w.usage.output_tokens) {
                println!("  tokens: {inp} in / {out} out (est. cost ${:.4})", {
                    w.usage.estimated_cost_cents.map(|c| c as f64 / 100.0).unwrap_or(0.0)
                });
            } else {
                println!("  tokens: unavailable");
            }
            let s = w.summary();
            println!(
                "  files changed: {} · commands run: {} · tests passed: {}",
                s.files_changed,
                s.commands_run,
                s.tests_passed.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())
            );
            println!("  files: {}", w.files_changed.iter().cloned().collect::<Vec<_>>().join(", "));
            let current = w
                .current_activity()
                .map(|a| a.display())
                .unwrap_or_else(|| "—".into());
            println!("  activity: {current}");
        }
        Response::AgentTimeline {
            execution_id,
            entries,
        } => {
            println!("timeline for {execution_id}:");
            for e in entries {
                println!("  {} {} — {}", e.at.format("%H:%M:%S"), e.kind.label(), e.detail);
            }
        }
        Response::AgentReview { review } => {
            let Some(r) = review else {
                println!("no review available for this execution");
                return;
            };
            println!("files changed ({}):", r.files.len());
            for f in &r.files {
                println!("  {}", f.path);
                if let Some(diff) = &f.diff {
                    if !diff.is_empty() {
                        println!("{}", diff.lines().take(12).map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n"));
                    }
                }
            }
            println!("commands ({}):", r.commands.len());
            for c in &r.commands {
                println!("  $ {c}");
            }
        }
        Response::AgentHealth { rows } => {
            println!("agent health:");
            for h in &rows {
                println!(
                    "  {} {} — {} ({} at {}, credential {})",
                    h.status.symbol(),
                    h.display_name,
                    h.status.label(),
                    h.definition_id,
                    h.binary_path.clone().unwrap_or_else(|| "not found".into()),
                    if h.credential_configured { "configured" } else { "not configured" }
                );
            }
        }
        Response::WorkspaceAgentSummary { summary } => {
            println!(
                "workspace agents: {} total · {} running · {} need you · {} failed · {} completed",
                summary.agents,
                summary.running,
                summary.needs_you,
                summary.failed,
                summary.completed
            );
        }
        Response::NotificationPrefs { prefs } => {
            println!(
                "notifications: needs-me {}{}",
                if prefs.on_needs_me { "on" } else { "off" },
                if !prefs.on_failure { " · failures off" } else { "" }
            );
        }
        Response::Subscribed { .. } => {
            // Only reachable through the streaming path; handled in
            // `agent_watch` before any roundtrip.
        }
    }
}

fn print_help() {
    println!(
        "FlashTerminal CLI — control a running instance\n\n\
         terminal serve                    # headless control surface\n\
         terminal workspace list\n\
         terminal workspace create <name> [root]\n\
         terminal workspace open <workspace-id>\n\
         terminal workspace rename <workspace-id> <name>\n\
         terminal workspace close <workspace-id>\n\
         terminal tab create\n\
         terminal tab close <tab-id>\n\
         terminal pane split <horizontal|vertical>\n\
         terminal pane close <pane-id>\n\
         terminal pane focus <pane-id>\n\
         terminal pane list\n\
         terminal agent list\n\
         terminal agent spawn <definition-id>\n\
         terminal agent spawn-pane <definition-id> <horizontal|vertical> [cwd]\n\
         terminal agent status|stop|restart|resume|pause <execution-id>\n\
         terminal agent permission <execution-id> <deny|allow-once|allow>\n\
         terminal agent watch [execution-id-prefix]   # live event stream (no polling)"
    );
}
