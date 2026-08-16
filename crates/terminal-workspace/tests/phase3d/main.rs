//! Phase 3D regression suite (3d.md §51–§55, §57).
//!
//! Multi-agent collaboration + artifact handoffs + result synthesis:
//! artifact creation/metadata, lineage, selection, cross-worktree
//! consumption, artifact readiness, access control (cross-contamination),
//! structured results, review findings + deterministic consensus, result
//! synthesis (no hallucinated ids), secret redaction before artifact
//! persistence, restart recovery, and the replan signal + human replan.
//!
//! Everything is deterministic: real disposable git repositories, the
//! `fake-agent` binary (skipped when not built), and a mock planner
//! provider for the replan path — no LLM required in CI.

use std::collections::VecDeque;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use terminal_workspace::engine::Multiplexer;
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::artifacts::{ArtifactReference, ArtifactSelector};
use terminal_workspace::terminal_session::collaboration::{
    ReviewAggregator, ReviewConsensus, ReviewFinding, ReviewPolicy, ReviewReport, ReviewVerdict,
    Severity,
};
use terminal_workspace::terminal_session::orchestration::{Task, TaskStatus};
use terminal_workspace::terminal_session::planning::{
    parse_plan_response, PlannerConfig, PlannerError, PlannerPhase, PlannerProvider,
    PlannerRequest, ProposedPlan,
};

// ---------------------------------------------------------------------------
// helpers (mirrors phase3c)
// ---------------------------------------------------------------------------

fn fake_available() -> bool {
    FakeAgentAdapter::resolve_binary().is_ok()
}

fn git(repo: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|e| panic!("git {args:?} in {repo}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn make_repo(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ft-3d-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir.to_string_lossy(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("base.txt"), "base content\n").unwrap();
    git(&dir.to_string_lossy(), &["add", "."]);
    git(
        &dir.to_string_lossy(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
    dir.to_string_lossy().to_string()
}

fn engine_in_repo(name: &str) -> (Multiplexer, String) {
    let repo = make_repo(name);
    let mut m = Multiplexer::new().unwrap();
    m.create_workspace("3d-ws", &repo).unwrap();
    (m, repo)
}

fn ws_id(m: &Multiplexer) -> String {
    m.workspaces()[0].id.clone()
}

/// Creates a task with the given scenario and args.
fn create_task(
    m: &mut Multiplexer,
    title: &str,
    scenario: &str,
    args: &[&str],
    deps: &[&str],
) -> String {
    let id = m
        .task_create(&ws_id(m), title, "", "fake-agent", &[], false)
        .unwrap();
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), scenario.to_string())],
    )
    .unwrap();
    m.task_add_arguments(&id, args).unwrap();
    for d in deps {
        m.task_add_dependency(&id, &d.to_string()).unwrap();
    }
    id
}

fn agent_done(t: &Task) -> bool {
    t.status == TaskStatus::NeedsReview || t.status.is_terminal() || t.status == TaskStatus::Blocked
}

fn drain_until_done(m: &mut Multiplexer, ids: &[String], timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        let done = ids
            .iter()
            .all(|id| m.task_get(id).map(agent_done).unwrap_or(false));
        if done {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// ---------------------------------------------------------------------------
// §3–§6: artifact creation, metadata, lineage, selection
// ---------------------------------------------------------------------------

#[test]
fn artifact_creation_metadata_and_selection() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("create");
    let id = create_task(
        &mut m,
        "Architecture",
        "modify",
        &[
            "--write-file",
            "docs/architecture.md",
            "--set-content",
            "# Architecture\n",
        ],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&id), 60_000));

    let artifacts = m.artifact_list();
    assert!(!artifacts.is_empty(), "completion registers artifacts");
    // §4: every artifact carries engine-stamped identity.
    let a = artifacts
        .iter()
        .find(|r| r.artifact.path.as_deref() == Some("docs/architecture.md"))
        .expect("architecture artifact registered");
    assert_eq!(a.artifact.created_by_task.as_deref(), Some(id.as_str()));
    assert_eq!(a.artifact.created_by_agent.as_deref(), Some("fake-agent"));
    assert_eq!(a.artifact.workspace_id.as_deref(), Some(ws_id(&m).as_str()));
    assert!(
        a.artifact.revision.is_some(),
        "revision stamped from the worktree HEAD"
    );
    assert!(a.artifact.created_at_ms > 0);
    // §10: structured reference, not a raw path.
    assert!(a.artifact.id.starts_with("artifact:"));
    // Payload is available and bounded.
    assert!(m.artifact_payload(&a.artifact.id).is_some());
    // §6: deterministic selection by kind + task.
    let sel = ArtifactSelector {
        task_id: Some(id.clone()),
        ..Default::default()
    };
    let selected = m.artifact_select(&sel);
    assert_eq!(
        selected.len(),
        artifacts.len(),
        "all artifacts from this task"
    );
    // Reference round-trips.
    let uri = ArtifactReference::format(&id, &a.artifact.id);
    let parsed = ArtifactReference::parse(&uri).unwrap();
    assert_eq!(parsed.artifact_id, a.artifact.id);
    let _ = repo;
}

#[test]
fn artifact_lineage_maps_producers_and_consumers() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("lineage");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &["--write-file", "a.txt", "--set-content", "A\n"],
        &[],
    );
    let b = create_task(
        &mut m,
        "B",
        "modify",
        &["--write-file", "b.txt", "--set-content", "B\n"],
        &[a.as_str()],
    );
    let _ = b;
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));

    let lineage = m.artifact_lineage();
    assert!(!lineage.producers.is_empty());
    // Every producer is a real task with registered artifacts.
    for (art_id, producer) in &lineage.producers {
        assert_eq!(producer, &a, "artifact {art_id} produced by A");
        assert!(m.artifact_get(art_id).is_some());
    }
}

// ---------------------------------------------------------------------------
// §8–§11: artifact readiness + cross-worktree consumption + access control
// ---------------------------------------------------------------------------

#[test]
fn cross_worktree_consumption() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("consume");
    // A produces `report.txt` in its worktree (committed, like 3C).
    let a = create_task(
        &mut m,
        "Architecture",
        "modify",
        &[
            "--write-file",
            "report.txt",
            "--set-content",
            "research findings",
        ],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    // B depends on A — only `Completed` dependencies release (§9). Approve
    // A's review so the dependency gate opens.
    m.resolve_task_review(&a, true).unwrap();
    assert_eq!(m.task_get(&a).unwrap().status, TaskStatus::Completed);

    // B depends on A and consumes A's artifact cross-worktree (§11).
    let art_a = m
        .artifact_list()
        .into_iter()
        .find(|r| r.artifact.path.as_deref() == Some("report.txt"))
        .expect("A's artifact");
    let b = create_task(
        &mut m,
        "Implementation",
        "consume",
        &["--read-file", "report.txt"],
        &[a.as_str()],
    );
    // Explicit grant: B declares the input artifact (§8). Subscribe to the
    // bus *before* B runs so the ArtifactConsumed event is observable.
    m.task_add_input_artifact(&b, &art_a.artifact.id).unwrap();
    let (sub_id, rx) = m
        .events
        .subscribe(terminal_workspace::events::EventFilter::all());
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&b), 60_000));
    let _ = sub_id;

    let b_task = m.task_get(&b).unwrap();
    assert!(
        b_task.status == TaskStatus::NeedsReview || b_task.status.is_terminal(),
        "B completed after consuming A's artifact"
    );
    // B's agent actually read the materialized content (deterministic proof
    // the handoff worked — B ran in its own worktree, not A's). The raw
    // "CONSUMED …" line is Unknown-kind, so it surfaces via the output
    // stream, not the structured timeline.
    m.events.flush();
    // Collect the subscriber's events once — the bus is drained in a single
    // pass (a second try_iter would see an empty queue).
    let all: Vec<terminal_workspace::terminal_session::execution::ApplicationEvent> =
        rx.try_iter().collect();
    let output: String = all
        .iter()
        .filter_map(|e| match e {
            terminal_workspace::terminal_session::execution::ApplicationEvent::AgentEvent {
                event: terminal_workspace::terminal_session::execution::AgentEvent::Output { text },
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        output.contains("CONSUMED report.txt: research findings"),
        "B's agent consumed the artifact: {output}"
    );
    // ArtifactConsumed events were published on the bus.
    let consumed = all.iter().any(|e| {
        matches!(
            e,
            terminal_workspace::terminal_session::execution::ApplicationEvent::ArtifactConsumed {
                task_id,
                ..
            } if task_id == &b
        )
    });
    assert!(consumed, "ArtifactConsumed event published");
}

#[test]
fn missing_input_artifact_blocks_task() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("blocked");
    let b = create_task(&mut m, "B", "completion", &[], &[]);
    // B declares an artifact that will never exist (§8–§9).
    m.task_add_input_artifact(&b, "artifact:does-not-exist")
        .unwrap();
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&b), 30_000));
    let task = m.task_get(&b).unwrap();
    assert_eq!(
        task.status,
        TaskStatus::Blocked,
        "missing input artifact → Blocked, never silently continued"
    );
    assert!(
        task.error
            .as_ref()
            .map(|e| e.message.contains("artifact"))
            .unwrap_or(false),
        "blocked reason names the artifact"
    );
}

#[test]
fn access_control_denies_unrelated_tasks() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("access");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &[
            "--write-file",
            "secret.txt",
            "--set-content",
            "A's private work",
        ],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    let art_a = m
        .artifact_list()
        .into_iter()
        .find(|r| r.artifact.path.as_deref() == Some("secret.txt"))
        .expect("A's artifact");

    // C has NO dependency on A and did NOT declare the artifact (§39).
    let c = create_task(&mut m, "C", "consume", &["--read-file", "secret.txt"], &[]);
    let (sub_id, rx) = m
        .events
        .subscribe(terminal_workspace::events::EventFilter::all());
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&c), 60_000));
    let c_task = m.task_get(&c).unwrap();
    // The materializer refused (no grant) and C's agent could not read the
    // file — deterministic failure, no silent access.
    assert!(c_task.status.is_terminal());
    // The denial is observable in the agent's raw output stream (the
    // "cannot read" line is Unknown-kind, so it never appears in the
    // structured timeline).
    m.events.flush();
    let c_output: String = rx
        .try_iter()
        .filter_map(|e| match e {
            terminal_workspace::terminal_session::execution::ApplicationEvent::AgentEvent {
                event: terminal_workspace::terminal_session::execution::AgentEvent::Output { text },
                ..
            } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        c_output.contains("cannot read secret.txt"),
        "C could not read A's artifact: {c_output}"
    );
    let _ = sub_id;
    // The selector enforces the same policy: C sees no artifacts.
    let sel = ArtifactSelector {
        referenced_by: Some(c.clone()),
        ..Default::default()
    };
    let visible = m.artifact_select(&sel);
    assert!(
        visible.iter().all(|r| r.artifact.id != art_a.artifact.id),
        "C cannot see A's artifact through the selector"
    );
    let _ = repo;
}

// ---------------------------------------------------------------------------
// §13, §18–§21: structured results + review findings + consensus
// ---------------------------------------------------------------------------

#[test]
fn structured_results_and_review_consensus() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("review");
    let impl_task = create_task(
        &mut m,
        "Implementation",
        "modify",
        &[
            "--write-file",
            "auth.rs",
            "--set-content",
            "pub fn auth() {}\n",
        ],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        std::slice::from_ref(&impl_task),
        60_000
    ));
    let result = m.task_get(&impl_task).unwrap().result.clone().unwrap();
    // §13: deterministic structured fields, no LLM summary required.
    assert!(result.metrics.iter().any(|(k, _)| k == "attempts"));
    assert!(result.metrics.iter().any(|(k, _)| k == "files_changed"));
    assert!(!result.files_changed.is_empty());

    // §20/§55: Reviewer A=PASS, B=WARNING, C=FAIL → NeedsReview.
    let reports = vec![
        ReviewReport {
            reviewer_task_id: Some("rev-a".into()),
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            reason: "no issues".into(),
        },
        ReviewReport {
            reviewer_task_id: Some("rev-b".into()),
            verdict: ReviewVerdict::Warning,
            findings: vec![ReviewFinding::new(
                Severity::Medium,
                "style nit",
                Some("rev-b".into()),
            )],
            reason: "medium finding".into(),
        },
        ReviewReport {
            reviewer_task_id: Some("rev-c".into()),
            verdict: ReviewVerdict::Fail,
            findings: vec![ReviewFinding::new(
                Severity::High,
                "missing token validation",
                Some("rev-c".into()),
            )
            .at("src/auth.rs", 142)],
            reason: "high finding".into(),
        },
    ];
    // §20/§55: A=PASS, B=WARNING, C=FAIL → NeedsReview (the consensus is
    // over all reports — a single PASS alone is not a consensus).
    for r in &reports {
        m.record_review_report(&impl_task, r);
    }
    let agg = m.task_review_consensus(&impl_task).unwrap();
    assert_eq!(agg.overall, ReviewConsensus::NeedsReview);
    // §55: replace C with PASS → ApprovedCandidate under the explicit
    // policy (a WARNING verdict without blocking findings does not block).
    let c_pass = vec![
        ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            reason: "".into(),
        },
        ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Warning,
            findings: vec![ReviewFinding::new(Severity::Low, "style nit", None)],
            reason: "low finding".into(),
        },
        ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            reason: "".into(),
        },
    ];
    let agg_pass = ReviewAggregator::aggregate(&c_pass, &ReviewPolicy::default());
    assert_eq!(agg_pass.overall, ReviewConsensus::ApprovedCandidate);
    // Findings became first-class artifacts (§18).
    assert!(
        m.artifact_list()
            .iter()
            .any(|r| r.artifact.metadata.iter().any(|(k, _)| k == "finding")),
        "review findings registered as artifacts"
    );
    // Explainability (§30): the user can answer "why?".
    let agg = m.task_review_consensus(&impl_task).unwrap();
    assert!(agg.explanations.iter().any(|e| e.contains("FAIL")));
    // Independent reviewers: B's report never mutated by C.
    let reports_now = m.review_reports(&impl_task);
    assert_eq!(reports_now.len(), 3);
}

#[test]
fn review_consensus_all_pass_is_approved_candidate() {
    let policy = ReviewPolicy::default();
    let reports = vec![
        ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            reason: "".into(),
        },
        ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Pass,
            findings: vec![],
            reason: "".into(),
        },
    ];
    let agg = ReviewAggregator::aggregate(&reports, &policy);
    assert_eq!(agg.overall, ReviewConsensus::ApprovedCandidate);
}

// ---------------------------------------------------------------------------
// §14–§15, §54: synthesis
// ---------------------------------------------------------------------------

#[test]
fn synthesis_references_all_inputs_and_rejects_unknown() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("synthesis");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &["--write-file", "a.txt", "--set-content", "A"],
        &[],
    );
    let b = create_task(
        &mut m,
        "B",
        "modify",
        &["--write-file", "b.txt", "--set-content", "B"],
        &[],
    );
    let c = create_task(
        &mut m,
        "C",
        "modify",
        &["--write-file", "c.txt", "--set-content", "C"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(
        &mut m,
        &[a.clone(), b.clone(), c.clone()],
        90_000
    ));
    // (multi-element slices keep clones — the borrow of three distinct
    // locals cannot use `from_ref`.)

    let ids: Vec<String> = m
        .artifact_list()
        .into_iter()
        .map(|r| r.artifact.id.clone())
        .collect();
    assert!(ids.len() >= 3, "three tasks produced artifacts");
    let result = m
        .synthesize(
            Some("plan-1".into()),
            Some("wf-1".into()),
            &[a.clone(), b.clone(), c.clone()],
            &ids[..ids.len().min(3)],
        )
        .unwrap();
    // §54: references exactly the provided artifacts — no invention.
    assert_eq!(result.artifacts.len(), 3);
    assert!(result.artifacts.iter().all(|id| ids.contains(id)));
    assert!(result.provenance.input_task_ids.len() == 3);
    assert!(result.provenance.plan_id.as_deref() == Some("plan-1"));
    assert!(result.provenance.workflow_id.as_deref() == Some("wf-1"));
    assert!(result.completed_work.len() >= 3);

    // Hallucinated ids are rejected, not silently ignored.
    let err = m
        .synthesize(
            None,
            None,
            std::slice::from_ref(&a),
            &["artifact:hallucinated".into()],
        )
        .unwrap_err();
    assert!(
        err.contains("hallucinated"),
        "unknown artifact id rejected: {err}"
    );
}

// ---------------------------------------------------------------------------
// §38: secret redaction before artifact persistence
// ---------------------------------------------------------------------------

#[test]
fn artifact_payloads_are_redacted() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    terminal_session::redact::Redactor::register_secret("sk-3d-super-secret");
    let (mut m, _repo) = engine_in_repo("redact");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &[
            "--write-file",
            "keys.txt",
            "--set-content",
            "key=sk-3d-super-secret",
        ],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    let rec = m
        .artifact_list()
        .into_iter()
        .find(|r| r.artifact.path.as_deref() == Some("keys.txt"))
        .expect("artifact");
    // The artifact payload is redacted — the secret never reaches the
    // store or the persisted state (§38, §35).
    let payload = m.artifact_payload(&rec.artifact.id).unwrap_or_default();
    assert!(
        !String::from_utf8_lossy(&payload).contains("sk-3d-super-secret"),
        "artifact payload redacted"
    );
    // …but the persisted artifact records never do (§38, §35). Task
    // arguments legitimately carry the fixture content; the check targets
    // the artifact store + review records specifically.
    let state = m.snapshot_state();
    let artifacts = state.artifacts.clone().unwrap_or_default();
    let serialized = serde_json::to_string(&artifacts).unwrap();
    assert!(
        !serialized.contains("sk-3d-super-secret"),
        "persisted artifact records are secret-free"
    );
    terminal_session::redact::Redactor::clear_registered();
}

// ---------------------------------------------------------------------------
// §35–§36: persistence + restart recovery
// ---------------------------------------------------------------------------

#[test]
fn artifacts_reviews_and_signals_survive_restart() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("persist");
    let a = create_task(
        &mut m,
        "A",
        "modify",
        &["--write-file", "keep.txt", "--set-content", "kept"],
        &[],
    );
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    m.record_review_report(
        &a,
        &ReviewReport {
            reviewer_task_id: None,
            verdict: ReviewVerdict::Warning,
            findings: vec![ReviewFinding::new(Severity::High, "check this", None)],
            reason: "deterministic".into(),
        },
    );
    m.signal_replan("test_cause", "test detail");
    let before = m.artifact_list().len();
    assert!(before > 0);

    let state = m.snapshot_state();
    assert!(state
        .artifacts
        .as_ref()
        .map(|a| !a.is_empty())
        .unwrap_or(false));
    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);
    // §36: completed artifacts, findings and signals remain available.
    assert_eq!(m2.artifact_list().len(), before);
    assert!(m2.task_review_consensus(&a).is_some());
    assert!(m2
        .replan_signals()
        .iter()
        .any(|(cause, _, _)| cause == "test_cause"));
}

// ---------------------------------------------------------------------------
// §44–§45: replan signal + human replan
// ---------------------------------------------------------------------------

/// Deterministic mock planner provider (mirrors phase3b §47).
struct MockPlannerProvider {
    responses: Mutex<VecDeque<String>>,
}

impl MockPlannerProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn plan(goal: &str, steps: &[(&str, &str, &str)]) -> String {
        let mut tasks = String::new();
        for (i, (id, title, agent)) in steps.iter().enumerate() {
            if i > 0 {
                tasks.push(',');
            }
            tasks.push_str(&format!(
                r#"{{"id":"{id}","title":"{title}","description":"fixture","agent":"{agent}","depends_on":[]}}"#
            ));
        }
        format!(r#"{{"goal":"{goal}","tasks":[{tasks}]}}"#)
    }
}

impl PlannerProvider for MockPlannerProvider {
    fn provider_id(&self) -> &str {
        "mock"
    }

    fn generate(
        &self,
        _request: &PlannerRequest,
        _config: &PlannerConfig,
    ) -> Result<ProposedPlan, PlannerError> {
        let Some(raw) = self.responses.lock().unwrap().pop_front() else {
            return Err(PlannerError::InvalidResponse {
                message: "mock exhausted".to_string(),
            });
        };
        parse_plan_response(&raw)
    }
}

#[test]
fn failed_task_emits_replan_signal() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("signal");
    let c = create_task(&mut m, "C", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&c), 60_000));
    // §44: the failure produces a structured replan signal (never an
    // autonomous replan).
    let signals = m.replan_signals();
    assert!(
        signals.iter().any(|(cause, _, _)| cause == "task_failure"),
        "replan signal for the failure: {signals:?}"
    );
}

#[test]
fn human_replan_builds_new_plan_requiring_approval() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("replan");
    // First workflow: one failing task → a replan signal.
    let a = create_task(&mut m, "A", "failure", &[], &[]);
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&a), 60_000));
    assert!(!m.replan_signals().is_empty());

    // §45: human replan constructs a new PlannerRequest from the current
    // graph/results — the new plan goes through the normal pipeline and
    // requires approval (never silently rewrites the running workflow).
    m.set_planner_provider(Box::new(MockPlannerProvider::new(vec![
        MockPlannerProvider::plan("redone", &[("retry", "Retry with fix", "fake-agent")]),
    ])));
    m.replan_workflow("retry the failed step with a fix")
        .unwrap();
    assert_eq!(m.planner_status().phase, PlannerPhase::NeedsApproval);
    assert!(
        m.replan_signals().is_empty(),
        "replan clears the signals it addressed"
    );
    // The new plan must pass the normal validate gate before execution.
    m.plan_validate().unwrap();
}
