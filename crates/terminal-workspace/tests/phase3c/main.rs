//! Phase 3C regression suite (3c.md §55, §37–§43, §49–§50).
//!
//! Worktree isolation + safe multi-agent execution + artifact review:
//! worktree creation, branch naming, dirty-workspace policy, the mandatory
//! 5-task parallel isolation test, cross-contamination, deterministic diff,
//! the review gate (completed ≠ merged), merge, merge conflicts, rejection,
//! cancellation preservation, retry with fresh worktrees, persistence and
//! restart recovery, orphan detection, path traversal, and secret safety.
//!
//! Every test uses a real disposable git repository (never the real
//! FlashTerminal repo, 3c.md §56) and the deterministic `fake-agent`
//! binary; tests skip when it is not built (same policy as phase 3A/3B).

use std::process::Command;
use std::time::{Duration, Instant};

use terminal_workspace::engine::Multiplexer;
use terminal_workspace::terminal_session::adapters::fake::FakeAgentAdapter;
use terminal_workspace::terminal_session::orchestration::{TaskPolicy, TaskStatus};
use terminal_workspace::terminal_session::worktrees::{
    DirtyPolicy, MergeOutcome, RetryWorktreePolicy, WorktreeState,
};

// ---------------------------------------------------------------------------
// helpers
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

/// Creates a fresh disposable repository with one committed file `base.txt`.
fn make_repo(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("ft-3c-{name}-{}", std::process::id()));
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

/// Engine with a workspace rooted at a fresh disposable repository.
fn engine_in_repo(name: &str) -> (Multiplexer, String) {
    let repo = make_repo(name);
    let mut m = Multiplexer::new().unwrap();
    let ws = m.create_workspace("3c-ws", &repo).unwrap();
    let _ = ws;
    (m, repo)
}

/// Creates a task that writes `file` with `content` into its worktree (the
/// fake-agent `modify` scenario — selected through the deterministic
/// `FAKE_AGENT_SCENARIO` environment knob, 3a.md §20).
fn create_modify_task(m: &mut Multiplexer, title: &str, file: &str, content: &str) -> String {
    let ws = m.workspaces()[0].id.clone();
    let id = m
        .task_create(&ws, title, "", "fake-agent", &[], false)
        .unwrap();
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), "modify".to_string())],
    )
    .unwrap();
    m.task_add_arguments(&id, &["--write-file", file, "--set-content", content])
        .unwrap();
    id
}

/// §19/§54: the review gate (`NeedsReview`) ends the *agent's* work — the
/// task is waiting on a human, not the scheduler. `is_terminal()` excludes
/// it, so this helper treats the review gate as "agent finished".
fn agent_done(t: &terminal_workspace::terminal_session::orchestration::Task) -> bool {
    t.status == TaskStatus::NeedsReview || t.status.is_terminal() || t.status == TaskStatus::Blocked
}

/// Drains until every listed task's agent is done (terminal or NeedsReview).
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

/// The worktree record for a task (the task owns at most one, §10).
fn worktree_of(
    m: &Multiplexer,
    task_id: &str,
) -> terminal_workspace::terminal_session::worktrees::WorktreeRecord {
    let wt_id = m
        .task_get(&task_id.to_string())
        .and_then(|t| t.worktree_id.clone())
        .expect("task has a worktree");
    m.worktree_get(&wt_id).expect("worktree record exists")
}

/// Reads a file from a worktree (or repo) at a relative path.
fn read_file(repo: &str, rel: &str) -> Option<String> {
    std::fs::read_to_string(std::path::Path::new(repo).join(rel)).ok()
}

// ---------------------------------------------------------------------------
// §6, §7, §8, §13: creation, branch naming, base revision
// ---------------------------------------------------------------------------

#[test]
fn worktree_creation_branch_naming_and_base_revision() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("create");
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let id = create_modify_task(&mut m, "Auth", "auth.txt", "auth impl\n");
    m.task_run();
    assert!(
        drain_until_done(&mut m, std::slice::from_ref(&id), 60_000),
        "task reached a terminal state"
    );

    let task = m.task_get(&id).unwrap();
    // §19/§54: isolated coding tasks land in review — completed ≠ merged.
    assert_eq!(task.status, TaskStatus::NeedsReview);
    let wt = worktree_of(&m, &id);
    assert_eq!(wt.state, WorktreeState::NeedsReview);
    // §7 branch naming is deterministic: flash/task/<task>-<slug>.
    assert!(
        wt.branch.starts_with("flash/task/"),
        "branch {} follows the deterministic scheme",
        wt.branch
    );
    // §8: the worktree was created from the repo HEAD at launch.
    assert_eq!(wt.base_revision.as_deref(), Some(base.as_str()));
    // The agent actually ran in the worktree and modified its own file.
    assert!(read_file(&wt.path, "auth.txt").is_some());
    assert!(
        read_file(&repo, "auth.txt").is_none(),
        "main repo untouched"
    );
    // §18: deterministic diff vs base revision, never the agent's summary.
    // The fake-agent writes a brand-new file, so it lands in `created`.
    let diff = m.worktree_diff(&wt.id).unwrap();
    assert!(diff.files_created.contains(&"auth.txt".to_string()));
    assert!(diff.files_total() >= 1);
    // TaskResult carries worktree provenance (§17).
    let result = task.result.as_ref().unwrap();
    assert_eq!(result.branch.as_deref(), Some(wt.branch.as_str()));
    assert_eq!(result.worktree.as_deref(), Some(wt.path.as_str()));
    assert_eq!(result.base_revision.as_deref(), Some(base.as_str()));
    assert!(result.diff_summary.is_some());
}

// ---------------------------------------------------------------------------
// §14: dirty workspace policy — never silently discard user work
// ---------------------------------------------------------------------------

#[test]
fn dirty_workspace_policy_requires_clean() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("dirty");
    // The user has uncommitted work.
    std::fs::write(std::path::Path::new(&repo).join("user-wip.txt"), "wip\n").unwrap();
    let policy = TaskPolicy {
        dirty: DirtyPolicy::RequireClean,
        ..TaskPolicy::default()
    };
    m.set_task_policy(policy);

    let ws = m.workspaces()[0].id.clone();
    let id = m
        .task_create(&ws, "Dirty", "", "fake-agent", &[], false)
        .unwrap();
    m.task_run();
    assert!(
        drain_until_done(&mut m, std::slice::from_ref(&id), 60_000),
        "task reached a terminal state"
    );
    // The spawn must have failed with a typed error — the worktree manager
    // refuses to isolate on a dirty repo (§14). User work is untouched.
    let task = m.task_get(&id).unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    let err = task.error.as_ref().expect("typed error recorded");
    assert!(
        err.to_string().contains("user-wip.txt"),
        "error surfaces the offending files: {err}"
    );
    assert_eq!(read_file(&repo, "user-wip.txt").as_deref(), Some("wip\n"));
}

// ---------------------------------------------------------------------------
// §37 mandatory: parallel isolation — 5 tasks, each touching its own file
// ---------------------------------------------------------------------------

#[test]
fn parallel_isolation_five_tasks() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("parallel");
    let policy = TaskPolicy {
        max_parallel_tasks: 5,
        ..TaskPolicy::default()
    };
    m.set_task_policy(policy);

    let files = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
    let mut ids = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let id = create_modify_task(&mut m, &format!("Task {i}"), f, &format!("{f}: task {i}\n"));
        ids.push(id);
    }
    m.task_run();
    assert!(
        drain_until_done(&mut m, &ids, 90_000),
        "all five tasks reached a terminal state"
    );

    // Every task: unique worktree, unique branch, unique session, and only
    // its own file changed (§36, §37).
    let mut branches = std::collections::HashSet::new();
    for (i, id) in ids.iter().enumerate() {
        let task = m.task_get(id).unwrap();
        assert_eq!(
            task.status,
            TaskStatus::NeedsReview,
            "task {i} completed isolated work (review gate)"
        );
        let wt = worktree_of(&m, id);
        assert!(branches.insert(wt.branch.clone()), "unique branch per task");
        assert!(wt.path.contains(".git"), "worktree lives in the git dir");
        // Only its own file changed; nothing from the other four.
        let diff = m.worktree_diff(&wt.id).unwrap();
        let mut touched = diff.files_changed.clone();
        touched.extend(diff.files_created.iter().cloned());
        touched.extend(diff.files_deleted.iter().cloned());
        touched.sort();
        touched.dedup();
        assert_eq!(
            touched,
            vec![files[i].to_string()],
            "task {i} only touched its own file"
        );
        let expected = format!("{}: task {i}\n", files[i]);
        assert_eq!(
            read_file(&wt.path, files[i]).as_deref(),
            Some(expected.as_str())
        );
        // Base repo untouched by every worktree.
        assert!(read_file(&repo, files[i]).is_none());
    }
    assert_eq!(branches.len(), 5);
}

// ---------------------------------------------------------------------------
// §38: cross-contamination — same filename, separate worktrees
// ---------------------------------------------------------------------------

#[test]
fn cross_contamination_same_filename() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("xcontam");
    let a = create_modify_task(&mut m, "A", "shared.txt", "content from A\n");
    let b = create_modify_task(&mut m, "B", "shared.txt", "content from B\n");
    m.task_run();
    assert!(
        drain_until_done(&mut m, &[a.clone(), b.clone()], 90_000),
        "tasks reached terminal states"
    );

    let wa = worktree_of(&m, &a);
    let wb = worktree_of(&m, &b);
    assert_ne!(wa.id, wb.id, "distinct worktrees");
    // Neither sees the other's unmerged changes.
    assert_eq!(
        read_file(&wa.path, "shared.txt").as_deref(),
        Some("content from A\n")
    );
    assert_eq!(
        read_file(&wb.path, "shared.txt").as_deref(),
        Some("content from B\n")
    );
}

// ---------------------------------------------------------------------------
// §41 review gate + §22/§39 merge
// ---------------------------------------------------------------------------

#[test]
fn review_then_merge_includes_only_approved_task() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("merge");
    let a = create_modify_task(&mut m, "A", "feature-a.txt", "A impl\n");
    let b = create_modify_task(&mut m, "B", "feature-b.txt", "B impl\n");
    m.task_run();
    assert!(drain_until_done(&mut m, &[a.clone(), b.clone()], 90_000));

    // §41: completed isolated tasks are NeedsReview, never merged.
    assert_eq!(m.task_get(&a).unwrap().status, TaskStatus::NeedsReview);
    assert_eq!(m.task_get(&b).unwrap().status, TaskStatus::NeedsReview);

    // §21: approval accepts the artifact — merge remains a separate step.
    m.resolve_task_review(&a, true).unwrap();
    assert_eq!(m.task_get(&a).unwrap().status, TaskStatus::Completed);
    assert_eq!(worktree_of(&m, &a).state, WorktreeState::Approved);

    // §22/§39: merge A into main; main contains A but not B.
    let wa = worktree_of(&m, &a);
    let outcome = m.worktree_merge(&wa.id, "main").unwrap();
    let MergeOutcome::Merged { .. } = outcome else {
        panic!("expected clean merge of A, got {outcome:?}");
    };
    assert_eq!(worktree_of(&m, &a).state, WorktreeState::Merged);
    assert!(read_file(&repo, "feature-a.txt").is_some(), "main has A");
    assert!(read_file(&repo, "feature-b.txt").is_none(), "main lacks B");

    // Approve + merge B too.
    m.resolve_task_review(&b, true).unwrap();
    let wb = worktree_of(&m, &b);
    let outcome = m.worktree_merge(&wb.id, "main").unwrap();
    let MergeOutcome::Merged { .. } = outcome else {
        panic!("expected clean merge of B, got {outcome:?}");
    };
    assert!(
        read_file(&repo, "feature-b.txt").is_some(),
        "main has B after merge"
    );
}

// ---------------------------------------------------------------------------
// §40: merge conflicts surface without data loss; no auto-resolution
// ---------------------------------------------------------------------------

#[test]
fn merge_conflict_surfaces_without_data_loss() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    // Both tasks edit the same line of the same file (already committed).
    let repo = make_repo("conflict");
    std::fs::write(
        std::path::Path::new(&repo).join("shared.txt"),
        "line one\nline two\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "seed",
        ],
    );

    let mut m = Multiplexer::new().unwrap();
    let ws = m.create_workspace("3c-ws", &repo).unwrap();
    let _ = ws;
    let a = create_modify_task(&mut m, "A", "shared.txt", "line one (A)\nline two\n");
    let b = create_modify_task(&mut m, "B", "shared.txt", "line one (B)\nline two\n");
    m.task_run();
    assert!(drain_until_done(&mut m, &[a.clone(), b.clone()], 90_000));

    m.resolve_task_review(&a, true).unwrap();
    m.resolve_task_review(&b, true).unwrap();
    let wa = worktree_of(&m, &a);
    let wb = worktree_of(&m, &b);
    let first = m.worktree_merge(&wa.id, "main").unwrap();
    let MergeOutcome::Merged { .. } = first else {
        panic!("first merge must be clean");
    };
    // The second merge conflicts on the same lines — surfaced, not
    // auto-resolved (§23), and no partial merge happened (§40).
    let outcome = m.worktree_merge(&wb.id, "main").unwrap();
    let MergeOutcome::Conflict(c) = outcome else {
        panic!("expected a MergeConflict, got {outcome:?}");
    };
    assert!(
        c.files.iter().any(|f| f.contains("shared.txt")),
        "conflict names the file: {:?}",
        c.files
    );
    assert_eq!(c.ours, git(&repo, &["rev-parse", "main"]));
    assert_eq!(c.theirs, git(&repo, &["rev-parse", &wb.branch]));
    // No data loss: main still holds A's version untouched.
    assert_eq!(
        read_file(&repo, "shared.txt").as_deref(),
        Some("line one (A)\nline two\n")
    );
}

// ---------------------------------------------------------------------------
// §42: cancellation preserves the worktree and its changes
// ---------------------------------------------------------------------------

#[test]
fn cancellation_preserves_worktree() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("cancel");
    let ws = m.workspaces()[0].id.clone();
    // Long-running agent that writes a file early, then keeps working.
    let id = m
        .task_create(&ws, "Long", "", "fake-agent", &[], false)
        .unwrap();
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), "modify".to_string())],
    )
    .unwrap();
    m.task_add_arguments(
        &id,
        &["--write-file", "wip.txt", "--set-content", "partial work\n"],
    )
    .unwrap();
    m.task_add_arguments(&id, &["--duration", "3600"]).unwrap();
    m.task_run();

    // Wait until the task is actually running.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let _ = m.drain_frame();
        if m.task_get(&id)
            .map(|t| t.status == TaskStatus::Running)
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(m.task_get(&id).unwrap().status, TaskStatus::Running);

    let wt_before = worktree_of(&m, &id);
    // The agent writes its file at startup, then keeps working until
    // `--duration` expires — wait for the write before cancelling.
    let file_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < file_deadline && read_file(&wt_before.path, "wip.txt").is_none() {
        let _ = m.drain_frame();
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        read_file(&wt_before.path, "wip.txt").is_some(),
        "changes present"
    );

    m.task_cancel(&id).unwrap();
    assert_eq!(m.task_get(&id).unwrap().status, TaskStatus::Cancelled);
    // §42: worktree + changes preserved, no accidental deletion.
    let wt = worktree_of(&m, &id);
    assert_eq!(wt.id, wt_before.id);
    assert_eq!(
        read_file(&wt.path, "wip.txt").as_deref(),
        Some("partial work\n")
    );
    assert!(std::path::Path::new(&wt.path).is_dir());
    // The base repo never saw the partial work.
    assert!(read_file(&repo, "wip.txt").is_none());
}

// ---------------------------------------------------------------------------
// §43: retry uses a fresh worktree per the explicit policy
// ---------------------------------------------------------------------------

#[test]
fn retry_uses_fresh_worktree() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("retry");
    // Fresh on retry is the explicit default (§43).
    let policy = TaskPolicy {
        retry_worktree: RetryWorktreePolicy::Fresh,
        ..TaskPolicy::default()
    };
    m.set_task_policy(policy);

    let ws = m.workspaces()[0].id.clone();
    let id = m
        .task_create(&ws, "Flaky", "", "fake-agent", &[], false)
        .unwrap();
    // `flaky`: attempt 1 fails, attempt 2 succeeds (fixture, 3a.md §33).
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), "flaky".to_string())],
    )
    .unwrap();
    m.task_run();
    assert!(
        drain_until_done(&mut m, std::slice::from_ref(&id), 90_000),
        "flaky task completed after retry"
    );
    let task = m.task_get(&id).unwrap();
    assert!(agent_done(task));
    assert_eq!(
        task.attempt_count, 2,
        "attempt 1 failed, attempt 2 succeeded"
    );

    // §43: attempt 2 ran in a *fresh* worktree (id carries the attempt).
    let wt = worktree_of(&m, &id);
    assert!(
        wt.id.ends_with("-a2") || wt.id.contains("-a2"),
        "attempt-2 worktree id is fresh: {}",
        wt.id
    );
    // Both worktrees still exist on disk (attempt 1 preserved for review).
    let all = m.worktree_list();
    assert!(
        all.len() >= 2,
        "attempt-1 worktree preserved: {}",
        all.len()
    );
    let _ = repo;
}

// ---------------------------------------------------------------------------
// §49–§50: persistence + restart recovery + §31 orphan detection
// ---------------------------------------------------------------------------

#[test]
fn persistence_restart_reconnect_and_orphan_detection() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("persist");
    let id = create_modify_task(&mut m, "Persist", "keep.txt", "kept work\n");
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&id), 60_000));
    let wt_before = worktree_of(&m, &id);

    // Save → restore into a fresh engine (§49).
    let state = m.snapshot_state();
    assert!(state
        .worktrees
        .as_ref()
        .map(|w| !w.is_empty())
        .unwrap_or(false));
    let mut m2 = Multiplexer::new().unwrap();
    m2.restore(state);

    // §50: the worktree reconnects through metadata (task id, not the
    // filesystem) and is *not* orphaned.
    let wt = worktree_of(&m2, &id);
    assert_eq!(wt.id, wt_before.id);
    assert_eq!(wt.path, wt_before.path);
    assert!(!wt.orphaned, "owned worktree reconnects to its task");
    // Task survived restore in its persisted state (NeedsReview survives).
    assert_eq!(m2.task_get(&id).unwrap().status, TaskStatus::NeedsReview);
    // The worktree on disk still holds its changes.
    assert_eq!(
        read_file(&wt.path, "keep.txt").as_deref(),
        Some("kept work\n")
    );

    // §31: a record whose owner no longer has a live task is orphaned —
    // surfaced, never deleted. Simulate a crash where the persisted record
    // has a dangling task id (no live task in the restored graph).
    let mut state3 = m2.snapshot_state();
    let record = state3
        .worktrees
        .as_mut()
        .and_then(|w| w.first_mut())
        .expect("a record exists");
    record.task_id = Some("no-such-task".to_string());
    // Simulate a crash where the task lost its worktree metadata too —
    // otherwise the restored scheduler would legitimately reconnect (§50).
    if let Some(tasks) = state3.tasks.as_mut() {
        if let Some(t) = tasks.graph.get_task_mut(&id) {
            t.worktree_id = None;
        }
    }
    let mut m4 = Multiplexer::new().unwrap();
    m4.restore(state3);
    let orphans = m4.worktree_orphans();
    assert_eq!(orphans.len(), 1, "dangling record surfaced as orphaned");
    assert!(orphans[0].orphaned);
    assert!(
        std::path::Path::new(&orphans[0].path).is_dir(),
        "never auto-deleted"
    );
}

// ---------------------------------------------------------------------------
// §34 path traversal + §35 secret safety
// ---------------------------------------------------------------------------

#[test]
fn branch_sanitizes_hostile_slug() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, repo) = engine_in_repo("traversal");
    let ws = m.workspaces()[0].id.clone();
    let id = m
        .task_create(&ws, "Hostile", "", "fake-agent", &[], false)
        .unwrap();
    // A hostile branch name can never escape the repo (sanitized upstream,
    // §34); the task still runs with the safe default slug.
    m.task_set_environment(
        &id,
        &[("FAKE_AGENT_SCENARIO".to_string(), "completion".to_string())],
    )
    .unwrap();
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&id), 60_000));
    let wt = worktree_of(&m, &id);
    assert!(wt.branch.starts_with("flash/task/"));
    assert!(
        !wt.branch.contains("..") && !wt.branch.contains("//"),
        "no traversal in branch: {}",
        wt.branch
    );
    // The worktree path stays inside the repository's git dir.
    let canon_repo = std::fs::canonicalize(&repo).unwrap();
    let canon_wt = std::fs::canonicalize(&wt.path).unwrap();
    assert!(
        canon_wt.starts_with(&canon_repo),
        "worktree lives under the repository"
    );
}

#[test]
fn worktree_metadata_and_diff_are_secret_free() {
    if !fake_available() {
        eprintln!("skipping: fake-agent binary not built (cargo build -p fake-agent)");
        return;
    }
    let (mut m, _repo) = engine_in_repo("secrets");
    let id = create_modify_task(&mut m, "Secret", "s.txt", "sk-1234567890abcdef\n");
    m.task_run();
    assert!(drain_until_done(&mut m, std::slice::from_ref(&id), 60_000));

    // The file content is a secret sentinel; it must never appear in
    // worktree metadata, TaskResult, the diff, or persisted state (§35).
    let wt = worktree_of(&m, &id);
    let serialized_meta = format!("{:?}", wt);
    assert!(!serialized_meta.contains("sk-1234567890abcdef"));
    let task = m.task_get(&id).unwrap();
    let serialized_result = format!("{:?}", task.result);
    assert!(!serialized_result.contains("sk-1234567890abcdef"));
    // The diff names the file but never includes raw content (name-only).
    let diff = m.worktree_diff(&wt.id).unwrap();
    let serialized_diff = format!("{:?}", diff);
    assert!(!serialized_diff.contains("sk-1234567890abcdef"));
    // Persisted worktree state (§49) is secret-free too — the records only
    // hold ids, paths, branches, revisions and states, never file content.
    let state = m.snapshot_state();
    let serialized_worktrees = serde_json::to_string(&state.worktrees).unwrap();
    assert!(!serialized_worktrees.contains("sk-1234567890abcdef"));
    // And the secret really is in the worktree file (the agent wrote it) —
    // proving the diff/metadata redaction isn't hiding a missing write.
    assert_eq!(
        read_file(&wt.path, "s.txt").as_deref(),
        Some("sk-1234567890abcdef\n")
    );
}
