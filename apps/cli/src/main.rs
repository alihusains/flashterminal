//! FlashTerminal CLI (§31): `terminal workspace list|create|open|rename|close`,
//! `terminal tab create|close`, `terminal pane split|close|focus|list`.
//!
//! Talks to a running FlashTerminal instance over the IPC Unix socket
//! (`$FLASHTERMINAL_SOCKET` or `/tmp/flashterminal.sock`). The low-level
//! capability is kept for power users while the GUI remains the primary
//! interface (§18).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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

    // Phase 3A: `terminal task set-policy <key> <value>` — read-modify-write
    // over the socket so untouched policy keys keep their live values.
    if args[0] == "task" && args.get(1).map(|s| s.as_str()) == Some("set-policy") {
        return set_task_policy(&socket, &args[2..]);
    }

    let request = match args[0].as_str() {
        "workspace" => workspace_cmd(&args[1..])?,
        "tab" => tab_cmd(&args[1..])?,
        "pane" => pane_cmd(&args[1..])?,
        "agent" => agent_cmd(&args[1..])?,
        "task" => task_cmd(&args[1..])?,
        "tasks" => Request::TaskList,
        // Phase 3B: `terminal plan create|status|approve|reject|edit|validate|execute|resume|cancel|metrics`.
        "plan" => plan_cmd(&args[1..])?,
        // Phase 3C: `terminal worktree list|inspect|diff|merge|discard|cleanup|orphans|budget`.
        "worktree" | "worktrees" => worktree_cmd(&args[1..])?,
        // Phase 3D: `terminal artifact|review|synthesize|replan ...`.
        "artifact" | "artifacts" => artifact_cmd(&args[1..])?,
        "review" => review_cmd(&args[1..])?,
        "synthesize" => Request::Synthesize {
            task_ids: args[1..].to_vec(),
            artifact_ids: Vec::new(),
        },
        // Phase 3E §41: `terminal replan list|show|approve|reject|edit …`.
        "replan" => match args.get(1).map(|s| s.as_str()) {
            Some("list") => Request::ReplanList,
            Some("show") | Some("inspect") => {
                let id = args.get(2).context("usage: terminal replan show <id>")?;
                Request::ReplanInspect {
                    replan_id: id.clone(),
                }
            }
            Some("approve") => {
                let id = args.get(2).context("usage: terminal replan approve <id>")?;
                Request::ReplanApprove {
                    replan_id: id.clone(),
                }
            }
            Some("reject") => {
                let id = args.get(2).context("usage: terminal replan reject <id> [reason]")?;
                let reason = args.get(3).cloned().unwrap_or_else(|| "rejected".to_string());
                Request::ReplanReject {
                    replan_id: id.clone(),
                    reason,
                }
            }
            Some("edit") => bail!(
                "usage: terminal replan edit is not yet wired to a concrete edit — use the plan edit surface"
            ),
            Some(goal) => Request::Replan { goal: goal.to_string() },
            None => bail!("usage: terminal replan <goal>|list|show|approve|reject"),
        },
        "replan-signals" => Request::ReplanSignals,
        // §40/§41: workflow history + interventions; §42 workflow list/validate.
        "workflow" => match args.get(1).map(|s| s.as_str()) {
            Some("list") => Request::TaskList,
            Some("validate") => Request::WorkflowValidate,
            Some("history") => Request::WorkflowHistory {
                workflow_id: args.get(2).cloned().unwrap_or_default(),
            },
            Some("interventions") => Request::WorkflowInterventions {
                workflow_id: args.get(2).cloned().unwrap_or_default(),
            },
            // Phase 3F §34: live state summary without opening a panel.
            Some("summary") => Request::WorkflowSummary,
            // Plan-version timeline (v1 → v2 → v3) with diffs.
            Some("timeline") => Request::WorkflowHistory {
                workflow_id: args.get(2).cloned().unwrap_or_default(),
            },
            // Structured replan signals behind those versions.
            Some("replans") => Request::ReplanSignals,
            other => bail!(
                "usage: terminal workflow list|validate|history|summary|timeline|replans|interventions (got {other:?})"
            ),
        },
        // Phase 3F (§31–§33): global workflow controls + attention queue.
        "control" => control_cmd(&args[1..])?,
        "attention" => Request::AttentionItems,
        // Phase 3E: adaptive status + explicit invalidations.
        "adaptive" => Request::AdaptiveStatus,
        "invalidate-task" => {
            let task = args.get(1).context("usage: terminal invalidate-task <id> <reason>")?;
            let reason = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "invalidated".to_string());
            Request::InvalidateTask {
                task_id: task.clone(),
                reason,
                evidence: Vec::new(),
                approved: true,
            }
        }
        "invalidate-artifact" => {
            let art = args.get(1).context("usage: terminal invalidate-artifact <id> <reason>")?;
            let reason = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "invalidated".to_string());
            Request::InvalidateArtifact {
                artifact_id: art.clone(),
                reason,
                evidence: Vec::new(),
            }
        }
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

/// Phase 3F (§32–§33): `terminal control stop-all|pause-all|resume-all`
/// — global workflow emergency controls.
fn control_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal control stop-all|pause-all|resume-all");
    };
    Ok(match cmd.as_str() {
        "stop-all" | "stop" => Request::StopAll,
        "pause-all" | "pause" => Request::PauseAll,
        "resume-all" | "resume" => Request::ResumeAll,
        other => bail!("unknown control command `{other}`"),
    })
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

/// Phase 3A (§43): `terminal task create|list|status|run|cancel|retry|
/// review|attach|validate|policy|scheduler`.
fn task_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!(
            "usage: terminal task create|list|status|run|cancel|retry|review|attach|environment|validate|policy|scheduler"
        );
    };
    Ok(match cmd.as_str() {
        "create" => {
            // terminal task create <workspace-id> <title> <agent> [--dep <task-id>]... [--review]
            let workspace_id = required(args, 1, "create <workspace-id> <title> <agent>")?;
            let title = required(args, 2, "create <workspace-id> <title> <agent>")?;
            let agent = required(args, 3, "create <workspace-id> <title> <agent>")?;
            let mut deps = Vec::new();
            let mut review_required = false;
            let mut i = 4;
            while i < args.len() {
                match args[i].as_str() {
                    "--dep" => {
                        let dep = required(args, i + 1, "--dep <task-id>")?;
                        deps.push(dep);
                        i += 2;
                    }
                    "--review" => {
                        review_required = true;
                        i += 1;
                    }
                    other => bail!("unknown task create flag `{other}`"),
                }
            }
            Request::TaskCreate {
                workspace_id,
                title,
                description: String::new(),
                assigned_agent: agent,
                dependencies: deps,
                review_required,
            }
        }
        "list" => Request::TaskList,
        "show" | "status" => Request::TaskStatus {
            task_id: required(args, 1, "show <task-id>")?,
        },
        "run" => Request::TaskRun,
        "cancel" => Request::TaskCancel {
            task_id: required(args, 1, "cancel <task-id>")?,
        },
        "retry" => Request::TaskRetry {
            task_id: required(args, 1, "retry <task-id>")?,
        },
        "review" => match args.get(1).map(|s| s.as_str()) {
            Some("approve") => Request::TaskResolveReview {
                task_id: required(args, 2, "review approve <task-id>")?,
                approve: true,
            },
            Some("reject") => Request::TaskResolveReview {
                task_id: required(args, 2, "review reject <task-id>")?,
                approve: false,
            },
            _ => bail!("usage: terminal task review <approve|reject> <task-id>"),
        },
        "attach" => Request::TaskAttachPane {
            task_id: required(args, 1, "attach <task-id>")?,
        },
        // Phase 3C §29: execution-environment preview for a task.
        "environment" | "env" => Request::TaskEnvironmentPreview {
            task_id: required(args, 1, "environment <task-id>")?,
        },
        "validate" => Request::WorkflowValidate,
        "policy" => Request::TaskPolicy,
        "scheduler" => Request::SchedulerStatus,
        other => bail!("unknown task command `{other}`"),
    })
}

/// Phase 3D: `terminal artifact list|get|payload|lineage|select <task-id>`.
fn artifact_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal artifact list|get|payload|lineage|select");
    };
    Ok(match cmd.as_str() {
        "list" | "ls" => Request::ArtifactList,
        "get" => Request::ArtifactGet {
            artifact_id: required(args, 1, "get <artifact-id>")?,
        },
        "payload" => Request::ArtifactPayload {
            artifact_id: required(args, 1, "payload <artifact-id>")?,
        },
        "lineage" => Request::ArtifactLineage,
        "select" => Request::ArtifactSelect {
            task_id: args.get(1).cloned(),
            max_results: 32,
        },
        other => bail!("unknown artifact command `{other}`"),
    })
}

/// Phase 3D: `terminal review findings|consensus <task-id>`.
fn review_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal review findings|consensus <task-id>");
    };
    Ok(match cmd.as_str() {
        "consensus" | "status" => Request::TaskReviewConsensus {
            task_id: required(args, 1, "consensus <task-id>")?,
        },
        other => bail!("unknown review command `{other}`"),
    })
}

/// Phase 3B (3b.md §16–§19, §23, §26, §43–§44): `terminal plan
/// create|status|approve|reject|edit|validate|execute|resume|cancel|metrics`.
/// Phase 3C: `terminal worktree ...` — worktree management surface
/// (3c.md §44, §52). Git operations run in the engine, never here.
fn worktree_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!("usage: terminal worktree list|inspect|diff|merge|discard|cleanup|orphans|budget");
    };
    Ok(match cmd.as_str() {
        "list" | "ls" => Request::WorktreeList,
        "inspect" | "status" => Request::WorktreeInspect {
            worktree_id: required(args, 1, "inspect <worktree-id>")?,
        },
        "diff" => Request::WorktreeDiff {
            worktree_id: required(args, 1, "diff <worktree-id>")?,
        },
        // terminal worktree merge <worktree-id> [target-branch]
        "merge" => Request::WorktreeMerge {
            worktree_id: required(args, 1, "merge <worktree-id> [target-branch]")?,
            target_branch: args.get(2).cloned().unwrap_or_else(|| "main".into()),
        },
        "discard" => Request::WorktreeDiscard {
            worktree_id: required(args, 1, "discard <worktree-id>")?,
        },
        "cleanup" => Request::WorktreeCleanup,
        "orphans" => Request::WorktreeOrphans,
        "budget" => Request::WorktreeBudget,
        other => bail!("unknown worktree command `{other}`"),
    })
}

fn plan_cmd(args: &[String]) -> Result<Request> {
    let Some(cmd) = args.first() else {
        bail!(
            "usage: terminal plan create|status|approve|reject|edit|validate|execute|resume|cancel|metrics"
        );
    };
    Ok(match cmd.as_str() {
        "create" => Request::PlanCreate {
            intent: args.get(1).cloned().unwrap_or_else(|| "".into()),
        },
        "status" | "show" => Request::PlanStatus,
        "approve" => Request::PlanApprove,
        "reject" => Request::PlanReject {
            reason: args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "rejected via CLI".into()),
        },
        // terminal plan edit <set-agent|set-deps> <step-id> [value]
        "edit" => {
            let kind = required(args, 1, "edit <set-agent|set-deps> <step-id> [value]")?;
            let step_id = required(args, 2, "edit <set-agent|set-deps> <step-id> [value]")?;
            match kind.as_str() {
                "set-agent" => Request::PlanEdit {
                    change: terminal_workspace::terminal_session::planning::PlanEditChange::SetAgent {
                        step_id,
                        agent: required(args, 3, "edit set-agent <step-id> <agent>")?,
                    },
                },
                "set-deps" => Request::PlanEdit {
                    change: terminal_workspace::terminal_session::planning::PlanEditChange::SetDependencies {
                        step_id,
                        dependencies: args[3..].to_vec(),
                    },
                },
                other => bail!("unknown plan edit kind `{other}`"),
            }
        }
        "validate" => Request::PlanValidate,
        "execute" | "run" => Request::PlanExecute,
        "resume" => Request::PlanResume,
        "cancel" => Request::PlanCancel,
        "metrics" => Request::PlannerMetrics,
        other => bail!("unknown plan command `{other}`"),
    })
}

/// `terminal task set-policy <key> <value>` — get, mutate one key, set.
fn set_task_policy(socket: &Path, args: &[String]) -> Result<()> {
    let key = required(args, 0, "set-policy <key> <value>")?;
    let value = required(args, 1, "set-policy <key> <value>")?;
    let Response::TaskPolicy { mut policy } = ipc::roundtrip(socket, &Request::TaskPolicy)? else {
        bail!("unexpected response to task policy request");
    };
    match key.as_str() {
        "max-parallel" => {
            policy.max_parallel_tasks = value
                .parse()
                .map_err(|_| anyhow::anyhow!("max-parallel must be an integer"))?;
        }
        "max-agents" => {
            policy.max_agents = value
                .parse()
                .map_err(|_| anyhow::anyhow!("max-agents must be an integer"))?;
        }
        "max-cost-cents" => {
            policy.max_cost_cents = Some(
                value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("max-cost-cents must be an integer"))?,
            );
        }
        "max-retries" => {
            policy.retry.max_retries = value
                .parse()
                .map_err(|_| anyhow::anyhow!("max-retries must be an integer"))?;
        }
        other => bail!(
            "unknown policy key `{other}` (max-parallel|max-agents|max-cost-cents|max-retries)"
        ),
    }
    match ipc::roundtrip(socket, &Request::SetTaskPolicy { policy }) {
        Ok(resp) => {
            print_response(resp);
            Ok(())
        }
        Err(e) => bail!("set-policy failed: {e}"),
    }
}

fn required(args: &[String], idx: usize, usage: &str) -> Result<String> {
    args.get(idx)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing argument: {usage}"))
}

/// Phase 2C: parses `terminal agents [filter]` filters (deterministic
/// dashboard filters, §13/§15).
fn parse_filter(
    f: Option<&str>,
) -> Result<terminal_workspace::terminal_session::work::AgentFilter> {
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

fn roundtrip_and_print(socket: &Path, request: Request) -> Result<()> {
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
            println!("work {} — {} [{}]", w.id, w.title, w.status.label());
            if !w.description.is_empty() {
                println!("  description: {}", w.description);
            }
            if let Some(secs) = w.summary().duration_secs {
                println!("  duration: {secs}s");
            }
            if let (Some(inp), Some(out)) = (w.usage.input_tokens, w.usage.output_tokens) {
                println!("  tokens: {inp} in / {out} out (est. cost ${:.4})", {
                    w.usage
                        .estimated_cost_cents
                        .map(|c| c as f64 / 100.0)
                        .unwrap_or(0.0)
                });
            } else {
                println!("  tokens: unavailable");
            }
            let s = w.summary();
            println!(
                "  files changed: {} · commands run: {} · tests passed: {}",
                s.files_changed,
                s.commands_run,
                s.tests_passed
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "unknown".into())
            );
            println!(
                "  files: {}",
                w.files_changed
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
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
                println!(
                    "  {} {} — {}",
                    e.at.format("%H:%M:%S"),
                    e.kind.label(),
                    e.detail
                );
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
                        println!(
                            "{}",
                            diff.lines()
                                .take(12)
                                .map(|l| format!("    {l}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        );
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
                    if h.credential_configured {
                        "configured"
                    } else {
                        "not configured"
                    }
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
                if !prefs.on_failure {
                    " · failures off"
                } else {
                    ""
                }
            );
        }
        // --- Phase 3A printers (§43, §52) ---
        Response::Tasks { tasks } => {
            if tasks.is_empty() {
                println!("no tasks");
            }
            for t in &tasks {
                print_task_line(t);
            }
        }
        Response::TaskStatus { task } => {
            print_task_line(&task);
            if let Some(err) = &task.error {
                println!("  error: {} ({:?})", err.message, err.class);
            }
            if let Some(r) = &task.result {
                println!(
                    "  result: {} · {} · {} attempt(s)",
                    r.status.label(),
                    r.summary,
                    r.attempt_count
                );
            }
            if task.review_required {
                println!("  review required");
            }
        }
        Response::TaskPolicy { policy } => {
            println!(
                "task policy: max-parallel {} · max-agents {} · max-cost {}¢ · retries {}",
                policy.max_parallel_tasks,
                policy.max_agents,
                policy
                    .max_cost_cents
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "unbounded".into()),
                policy.retry.max_retries,
            );
        }
        Response::SchedulerStatus { status } => {
            println!(
                "scheduler: {} queued · {} running · started {} · completed {} · failed {} · cancelled {} · retried {} · actual cost {}¢",
                status.queued.len(),
                status.running.len(),
                status.started_count,
                status.completed_count,
                status.failed_count,
                status.cancelled_count,
                status.retried_count,
                status.actual_cost_cents
            );
            for (id, st) in &status.states {
                println!("  {st} {id}");
            }
        }
        Response::WorkflowValidation { issues } => {
            if issues.is_empty() {
                println!("workflow valid");
            } else {
                for issue in &issues {
                    println!("issue: {issue}");
                }
            }
        }
        // --- Phase 3B printers (§42 plan preview, §39 metrics) ---
        Response::PlanStatus { status } => {
            println!("plan: {:?}", status.phase);
            if let Some(intent) = &status.intent {
                println!("  intent: {intent}");
            }
            if let Some(err) = &status.last_error {
                println!("  error: {err}");
            }
            if let Some(plan) = &status.plan {
                println!("  goal: {}", plan.goal);
                for s in &status.steps {
                    let agent = s
                        .step
                        .agent_recommendation
                        .as_ref()
                        .map(|r| r.agent_definition_id.as_str())
                        .unwrap_or("(unassigned)");
                    println!(
                        "  [{}] {} — {} ({agent})",
                        s.status.symbol(),
                        s.step.id,
                        s.step.title
                    );
                    if !s.step.depends_on.is_empty() {
                        println!("       depends on: {}", s.step.depends_on.join(", "));
                    }
                }
                if let Some(cost) = plan.estimated_cost_cents {
                    println!("  estimated: ${}.{:02}", cost / 100, cost % 100);
                }
                if let Some(min) = plan.estimated_duration_min {
                    println!("  duration: ~{min} min");
                }
            }
            if status.edited {
                println!("  edited: yes");
            }
            println!("  parallelism: {}", status.parallelism);
        }
        Response::PlannerMetrics { metrics } => {
            println!(
                "planner: generated {} · valid {} · invalid {} · unknown-agent {} · cycles {} · budget-violations {} · parallelism-violations {} · human edits {} · rejections {} · executions {} ok / {} failed · bypassed {}",
                metrics.plans_generated,
                metrics.plans_valid,
                metrics.plans_invalid,
                metrics.unknown_agent_count,
                metrics.cycle_count,
                metrics.budget_violation_count,
                metrics.parallelism_violation_count,
                metrics.human_edits,
                metrics.human_rejections,
                metrics.executions_succeeded,
                metrics.executions_failed,
                metrics.bypassed_intents,
            );
        }
        // --- Phase 3C worktree printers (3c.md §52, §29) ---
        Response::Worktrees { worktrees } => {
            if worktrees.is_empty() {
                println!("no worktrees");
            }
            for w in &worktrees {
                let owner = w.task_id.as_deref().unwrap_or("<orphan>");
                let orphan = if w.orphaned { " (orphaned)" } else { "" };
                println!(
                    "{} [{}] {} — {} · task {owner}{orphan}",
                    w.id,
                    w.state.label(),
                    w.branch,
                    w.path
                );
            }
        }
        Response::WorktreeInspection { inspection } => {
            println!(
                "{} — {} [{}] @ {}",
                inspection.id,
                inspection.path,
                inspection.branch.unwrap_or_default(),
                inspection.head
            );
            if !inspection.exists {
                println!("  (directory missing)");
            }
        }
        Response::WorktreeDiff { diff } => {
            println!(
                "{} file(s) changed · +{} −{} ({} created, {} deleted)",
                diff.files_total(),
                diff.insertions,
                diff.deletions,
                diff.files_created.len(),
                diff.files_deleted.len()
            );
            for f in &diff.files_changed {
                println!("  ~ {f}");
            }
            for f in &diff.files_created {
                println!("  + {f}");
            }
            for f in &diff.files_deleted {
                println!("  - {f}");
            }
            if let Some(base) = &diff.base_revision {
                println!("  base: {base}");
            }
            if let Some(res) = &diff.result_revision {
                println!("  result: {res}");
            }
        }
        Response::WorktreeMerge { outcome } => match outcome {
            terminal_workspace::terminal_session::worktrees::MergeOutcome::Merged { commit } => {
                println!("merged: {commit}");
            }
            terminal_workspace::terminal_session::worktrees::MergeOutcome::Conflict(c) => {
                println!("merge conflict — no changes were made:");
                for f in &c.files {
                    println!("  ! {f}");
                }
            }
        },
        Response::WorktreeOrphans { worktrees } => {
            if worktrees.is_empty() {
                println!("no orphaned worktrees");
            } else {
                println!(
                    "{} orphaned worktree(s) — review, never auto-deleted:",
                    worktrees.len()
                );
                for w in &worktrees {
                    println!("  {} — {} [{}]", w.id, w.branch, w.state.label());
                }
            }
        }
        Response::TaskEnvironmentPreview { environment } => match environment {
            Some(e) => {
                println!("Execution Environment");
                println!("  Repository:    {}", e.repository);
                println!(
                    "  Base:          {}",
                    e.base_branch.as_deref().unwrap_or("").to_owned()
                        + &e.base_revision
                            .as_ref()
                            .map(|r| format!(" @ {r}"))
                            .unwrap_or_default()
                );
                println!("  Isolation:     {}", e.isolation.label());
                println!("  Branch:        {}", e.branch.as_deref().unwrap_or(""));
                println!("  Working dir:   {}", e.working_directory);
            }
            None => println!("no environment (task not scheduled yet)"),
        },
        Response::WorktreeBudget { budget } => {
            println!("max_worktrees: {}", budget.max_worktrees);
        }
        // --- Phase 3D printers (3d.md §46–§48, §29–§30) ---
        Response::Artifacts { artifacts } => {
            if artifacts.is_empty() {
                println!("no artifacts");
            }
            for r in &artifacts {
                let a = &r.artifact;
                println!(
                    "{} [{:?}] {} — {} (by {} · {} B payload)",
                    a.id,
                    a.kind,
                    a.description,
                    a.created_by_task.as_deref().unwrap_or("<none>"),
                    a.created_by_agent.as_deref().unwrap_or("?"),
                    r.payload_size()
                );
                if let Some(p) = &a.path {
                    println!("    path: {p}");
                }
                if let Some(rev) = &a.revision {
                    println!("    revision: {rev}");
                }
            }
        }
        Response::Artifact { artifact } => match artifact {
            Some(r) => {
                let a = &r.artifact;
                println!(
                    "{} [{:?}] {} — by {}",
                    a.id,
                    a.kind,
                    a.description,
                    a.created_by_agent.as_deref().unwrap_or("?")
                );
                if let Some(p) = &a.path {
                    println!("  path: {p}");
                }
                if let Some(rev) = &a.revision {
                    println!("  revision: {rev}");
                }
                println!("  payload: {} B", r.payload_size());
            }
            None => println!("artifact not found"),
        },
        Response::ArtifactPayload { payload, .. } => match payload {
            Some(p) => print!("{}", String::from_utf8_lossy(&p)),
            None => println!("no payload (metadata only)"),
        },
        Response::ArtifactLineage { lineage } => {
            for (artifact, producer) in &lineage.producers {
                println!("{producer} → artifact {artifact}");
            }
            for (artifact, consumers) in &lineage.consumers {
                println!("artifact {artifact} ← {}", consumers.join(", "));
            }
        }
        Response::ReviewConsensus { aggregation } => match aggregation {
            Some(a) => {
                println!("overall: {}", a.overall.label());
                for (sev, n) in &a.severity_counts {
                    println!("  {} {}", sev.label(), n);
                }
                for e in &a.explanations {
                    println!("  why: {e}");
                }
            }
            None => println!("no review reports recorded"),
        },
        Response::Synthesis { result } => {
            println!("overall: {}", result.overall_status);
            println!("{}", result.summary);
            for w in &result.warnings {
                println!("  warning: {w}");
            }
            for f in &result.failures {
                println!("  failure: {f}");
            }
            println!(
                "  artifacts: {} · inputs: {} task(s) · model: {}",
                result.artifacts.len(),
                result.provenance.input_task_ids.len(),
                result
                    .provenance
                    .model
                    .as_deref()
                    .unwrap_or("deterministic")
            );
        }
        Response::ReplanSignals { signals } => {
            if signals.is_empty() {
                println!("no replan signals");
            }
            for (cause, detail, at) in &signals {
                println!("{cause}: {detail} (at {at})");
            }
        }
        // Phase 3E: adaptive orchestration responses (§40–§41).
        Response::ReplanList { proposals } => {
            if proposals.is_empty() {
                println!("no pending replans");
            }
            for p in &proposals {
                println!(
                    "{} [{}] {} — {} task(s), ${}.{:02}",
                    p.id,
                    p.triggers
                        .iter()
                        .map(|t| format!("{t:?}"))
                        .collect::<Vec<_>>()
                        .join(","),
                    p.reason,
                    p.new_tasks.len(),
                    p.estimated_cost_cents.unwrap_or(0) / 100,
                    p.estimated_cost_cents.unwrap_or(0) % 100
                );
            }
        }
        Response::ReplanInspect { proposal, version } => {
            if let Some(p) = &proposal {
                println!("replan: {}", p.id);
                println!("  reason: {}", p.reason);
                println!("  changes:");
                for c in &p.changes {
                    println!("    + {c}");
                }
                for (t, what) in &p.modified_tasks {
                    println!("    ~ {t}: {what}");
                }
                for t in &p.removed_tasks {
                    println!("    - {t}");
                }
                if let Some(d) = version.as_ref().and_then(|v| v.diff_from_previous.as_ref()) {
                    println!("  diff:");
                    for a in &d.added {
                        println!("    + {a}");
                    }
                    for r in &d.removed {
                        println!("    - {r}");
                    }
                    for m in &d.modified {
                        println!("    ~ {m}");
                    }
                }
            } else {
                println!("no such replan");
            }
        }
        Response::WorkflowHistory { versions } => {
            for v in &versions {
                println!(
                    "v{} {} — {} step(s), approved={}, superseded_by={:?}, diff={}",
                    v.version,
                    v.plan_id,
                    v.plan.steps.len(),
                    v.approved,
                    v.superseded_by,
                    v.diff_from_previous
                        .as_ref()
                        .map(|d| format!(
                            "+{} -{} ~{}",
                            d.added.len(),
                            d.removed.len(),
                            d.modified.len()
                        ))
                        .unwrap_or_else(|| "-".to_string())
                );
            }
        }
        Response::WorkflowInterventions {
            escalations,
            task_invalidations,
            artifact_invalidations,
        } => {
            for e in &escalations {
                println!("escalation {}: {}", e.id, e.what_happened);
                for a in &e.attempted {
                    println!("  attempted: {a}");
                }
            }
            for i in &task_invalidations {
                println!(
                    "task invalidated {}: {} (approved={})",
                    i.task_id, i.reason, i.approved
                );
            }
            for i in &artifact_invalidations {
                println!("artifact invalidated {}: {}", i.artifact_id, i.reason);
            }
        }
        Response::AdaptiveStatus {
            limits,
            limit_reached,
            autonomy,
            metrics,
            quality,
        } => {
            println!("autonomy: {autonomy:?} (Automatic disabled in Phase 3E)");
            println!(
                "limits: max_replans={} cooldown={}s limit_reached={}",
                limits.max_replans, limits.replan_cooldown_seconds, limit_reached
            );
            println!(
                "replans: {} proposed, {} approved, {} rejected, {} edited — approval rate {:.0}%",
                metrics.replan_count,
                metrics.replan_approval_count,
                metrics.replan_rejection_count,
                metrics.replan_edit_count,
                metrics.approval_rate() * 100.0
            );
            println!(
                "planner quality: {} valid, {} invalid, {} edits, {} rejections",
                quality.valid_replan_rate,
                quality.invalid_replan_rate,
                quality.human_edit_rate,
                quality.human_rejection_rate
            );
        }
        Response::Subscribed { .. } => {
            // Only reachable through the streaming path; handled in
            // `agent_watch` before any roundtrip.
        }
        // --- Phase 3F printers (§31–§34) ---
        Response::StopAll { report } => {
            println!(
                "STOP ALL: {} agent(s) stopped · {} task(s) stopped · {} human decision(s) preserved",
                report.agents_stopped, report.tasks_stopped, report.preserved_decisions
            );
        }
        Response::WorkflowSummary { summary } => {
            println!(
                "workflows: {} · running: {} · waiting: {} · needs you: {} · completed today: {} · failed: {} · est. cost: ${}.{:02}{}",
                summary.workflows,
                summary.running,
                summary.waiting,
                summary.needs_approval,
                summary.completed_today,
                summary.failed,
                summary.estimated_cost_cents / 100,
                summary.estimated_cost_cents % 100,
                if summary.paused { " · PAUSED" } else { "" }
            );
        }
        Response::AttentionItems { items } => {
            if items.total == 0 {
                println!("nothing needs you");
                return;
            }
            println!("{} item(s) need a human decision:", items.total);
            for a in &items.agents {
                println!(
                    "  agent {} [{}] — {}{}",
                    a.execution_id,
                    a.display_name,
                    a.state,
                    a.attention
                        .as_ref()
                        .map(|t| format!(" ({t})"))
                        .unwrap_or_default()
                );
            }
            for t in &items.review_tasks {
                println!(
                    "  task {} [{}] — {} needs review",
                    t.task_id, t.title, t.state
                );
            }
            for r in &items.replans {
                println!(
                    "  replan {} — {}{}",
                    r.replan_id,
                    r.reason,
                    r.version.map(|v| format!(" (v{v})")).unwrap_or_default()
                );
            }
        }
    }
}

/// Phase 3A §43: one-line task summary.
fn print_task_line(t: &terminal_workspace::terminal_session::orchestration::Task) {
    let exec = t
        .agent_execution_id
        .as_ref()
        .map(|e| format!(" [{}]", &e.0[..e.0.len().min(8)]))
        .unwrap_or_default();
    println!(
        "{} {} — {} ({} agent{exec}; {} attempt(s))",
        t.status, t.id, t.title, t.assigned_agent, t.attempt_count
    );
    if !t.dependencies.is_empty() {
        println!("  depends on: {}", t.dependencies.join(", "));
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
         terminal agent watch [execution-id-prefix]   # live event stream (no polling)\n\
         terminal tasks\n\
         terminal task create <workspace-id> <title> <agent> [--dep <task-id>]... [--review]\n\
         terminal task status|cancel|retry|attach <task-id>\n\
         terminal task run\n\
         terminal task review <approve|reject> <task-id>\n\
         terminal task policy\n\
         terminal task set-policy <max-parallel|max-agents|max-cost-cents|max-retries> <value>\n\
         terminal task scheduler\n\
         terminal task validate\n\
         terminal workflow list|validate|history|interventions\n\
         terminal workflow summary                # live state: running/needs-you/cost (§34)\n\
         terminal workflow timeline                # plan version timeline (v1 → v2 → v3)\n\
         terminal workflow replans                 # structured replan signals\n\
         terminal control stop-all                 # stop every agent + cancel tasks (§32)\n\
         terminal control pause-all|resume-all     # gate new work while state is kept (§33)\n\
         terminal attention                        # everything that needs a human decision (§31)\n\
         terminal plan create <intent>          # LLM-planned workflow (validated, needs approval)\n\
         terminal plan status\n\
         terminal plan approve|reject\n\
         terminal plan edit <set-agent|set-deps> <step-id> [value]\n\
         terminal plan validate|execute|resume|cancel\n\
         terminal plan metrics\n\
         terminal worktree list|orphans|cleanup|budget\n\
         terminal worktree inspect|diff|merge|discard <worktree-id>\n\
         terminal task environment <task-id>     # execution-environment preview\n\
         terminal artifact list|get|payload|lineage|select <task-id>\n\
         terminal review consensus <task-id>\n\
         terminal synthesize <task-id>...        # deterministic result synthesis\n\
         terminal replan <goal>                  # human replan (new plan needs approval)\n\
         terminal replan list|show <id>         # Phase 3E: proposals\n\
         terminal replan approve|reject <id>    # Phase 3E: approve/reject (original stays intact)\n\
         terminal adaptive                      # replan limits, autonomy, metrics\n\
         terminal invalidate-task <id> <reason>\n\
         terminal invalidate-artifact <id> <reason>\n\
         terminal replan-signals"
    );
}
