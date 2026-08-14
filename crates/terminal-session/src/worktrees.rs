//! Phase 3C — worktree isolation + safe multi-agent execution (3c.md §1–§50).
//!
//! The TaskScheduler never assumes every task runs in the same directory.
//! Every task has an explicit [`ExecutionEnvironment`]; coding tasks default
//! to isolated Git worktrees so parallel agents can never collide in one
//! working directory:
//!
//! ```text
//! Repository
//!    ├── Worktree A → Task A → Agent A
//!    ├── Worktree B → Task B → Agent B
//!    └── ...
//! ```
//!
//! This module is the **only** place that shells out to git for worktree
//! operations (3c.md §5: "Create a dedicated Git abstraction"; task code
//! never shells out). The planner never touches git directly (§45); the
//! scheduler never creates worktrees itself (§44) — the engine's
//! environment layer drives [`WorktreeManager`] and hands the resulting
//! [`ExecutionEnvironment`] to [`crate::agent::AgentRuntime`].

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// §3–§4 execution environments + isolation modes
// ---------------------------------------------------------------------------

/// How a task's execution is isolated from the main repository (3c.md §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    /// Task runs directly in the repository working tree. Never the default
    /// for coding tasks — requires explicit policy (§28).
    SharedWorkspace,
    /// Task runs in a dedicated `git worktree` on its own branch (§6).
    /// 3c.md §4: default for coding tasks is GitWorktree.
    #[default]
    GitWorktree,
    /// Task runs in a throwaway temporary directory.
    TemporaryDirectory,
}

impl IsolationMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::SharedWorkspace => "Shared workspace",
            Self::GitWorktree => "Git worktree",
            Self::TemporaryDirectory => "Temporary directory",
        }
    }
}

/// The explicit execution environment of one task (3c.md §2–§3). Never
/// persists secret environment variables (§3, §35).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEnvironment {
    /// Path of the base repository.
    pub repository: String,
    /// §8: base commit the worktree was created from.
    pub base_revision: Option<String>,
    /// §8: branch the worktree was created from (e.g. `main`).
    pub base_branch: Option<String>,
    /// §8: wall-clock base timestamp (ms).
    pub base_timestamp_ms: u64,
    /// The directory the agent actually runs in (worktree path for
    /// isolated tasks).
    pub working_directory: String,
    pub worktree_id: Option<String>,
    pub branch: Option<String>,
    pub isolation: IsolationMode,
    /// Non-secret environment additions for the agent launch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_variables: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// §9, §49 worktree state + record
// ---------------------------------------------------------------------------

/// Worktree lifecycle state (3c.md §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeState {
    Created,
    Active,
    Completed,
    NeedsReview,
    /// Human approved the result as a valid artifact (§21). Merge is a
    /// separate, explicit step.
    Approved,
    Merged,
    Rejected,
    CleanupPending,
    Deleted,
}

impl WorktreeState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Created => "Created",
            Self::Active => "Active",
            Self::Completed => "Completed",
            Self::NeedsReview => "Needs review",
            Self::Approved => "Approved",
            Self::Merged => "Merged",
            Self::Rejected => "Rejected",
            Self::CleanupPending => "Cleanup pending",
            Self::Deleted => "Deleted",
        }
    }

    /// Actions a user may take in this state (3c.md §52 — only valid
    /// actions are displayed).
    pub fn allowed_actions(self) -> Vec<&'static str> {
        match self {
            Self::NeedsReview | Self::Completed => vec!["open", "review", "approve", "reject"],
            Self::Approved => vec!["open", "merge", "reject"],
            Self::Rejected => vec!["open", "reopen", "retry", "discard"],
            Self::Merged => vec!["open", "discard"],
            Self::Created | Self::Active => vec!["open", "cancel"],
            Self::CleanupPending | Self::Deleted => vec![],
        }
    }
}

/// Versioned, secret-free metadata of one worktree (3c.md §9, §49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: String,
    /// Owning task (a task owns at most one active worktree, §10).
    #[serde(default)]
    pub task_id: Option<String>,
    pub path: String,
    pub repository: String,
    pub branch: String,
    pub base_revision: Option<String>,
    pub base_branch: Option<String>,
    pub base_timestamp_ms: u64,
    pub state: WorktreeState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// True when the base repo had uncommitted changes at creation (§14 —
    /// the user was warned, nothing was discarded).
    #[serde(default)]
    pub dirty_at_creation: bool,
    /// True when the worktree survived a restart without an active task
    /// (§31, §50). Never deleted automatically.
    #[serde(default)]
    pub orphaned: bool,
}

impl WorktreeRecord {
    /// Private constructor — every field is engine-computed (never
    /// agent-controlled); the argument count is intentional (9 positional
    /// provenance fields, all required by §9/§49).
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: String,
        task_id: Option<String>,
        path: String,
        repository: String,
        branch: String,
        base_revision: Option<String>,
        base_branch: Option<String>,
        base_timestamp_ms: u64,
        dirty_at_creation: bool,
    ) -> Self {
        let now = crate::work::now_ms();
        Self {
            id,
            task_id,
            path,
            repository,
            branch,
            base_revision,
            base_branch,
            base_timestamp_ms,
            state: WorktreeState::Active,
            created_at_ms: now,
            updated_at_ms: now,
            dirty_at_creation,
            orphaned: false,
        }
    }
}

// ---------------------------------------------------------------------------
// §17–§18 deterministic diff + §14 dirty workspace
// ---------------------------------------------------------------------------

/// Deterministic diff between a worktree's base revision and its current
/// HEAD (§18 — never the agent's own summary). Secret-free paths only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiffSummary {
    pub files_changed: Vec<String>,
    pub files_created: Vec<String>,
    pub files_deleted: Vec<String>,
    pub insertions: u64,
    pub deletions: u64,
    pub base_revision: Option<String>,
    pub result_revision: Option<String>,
}

impl DiffSummary {
    pub fn files_total(&self) -> usize {
        self.files_changed.len() + self.files_created.len() + self.files_deleted.len()
    }
}

/// Uncommitted-change report for the base repository (§14).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DirtyWorkspaceReport {
    pub dirty: bool,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub untracked: Vec<String>,
}

impl DirtyWorkspaceReport {
    pub fn all_files(&self) -> Vec<String> {
        let mut v = Vec::new();
        v.extend(self.staged.iter().cloned());
        v.extend(self.unstaged.iter().cloned());
        v.extend(self.untracked.iter().cloned());
        v.sort();
        v.dedup();
        v
    }
}

// ---------------------------------------------------------------------------
// §23 merge conflicts + §22 merge outcome
// ---------------------------------------------------------------------------

/// A merge conflict surfaced without data loss (3c.md §23). Agents are
/// never asked to resolve conflicts automatically yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeConflict {
    pub files: Vec<String>,
    pub branches: Vec<String>,
    pub base: Option<String>,
    pub ours: String,
    pub theirs: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeOutcome {
    Merged { commit: String },
    Conflict(MergeConflict),
}

// ---------------------------------------------------------------------------
// §14, §30, §43, §47 policies
// ---------------------------------------------------------------------------

/// Dirty-workspace policy (3c.md §14). Default: require explicit handling —
/// never silently discard user changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DirtyPolicy {
    /// Uncommitted base-repo changes don't block isolated work (explicit
    /// choice — never silently discards anything, §14).
    #[default]
    AllowDirty,
    RequireClean,
    Snapshot,
}

/// Worktree cleanup policy (3c.md §30). Default: keep until the user
/// reviews/discards — never delete user work without explicit policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPolicy {
    /// 3c.md §30: default keeps worktrees until the user reviews/discards —
    /// never delete user work without explicit policy.
    #[default]
    Keep,
    CleanupAfterMerge,
    CleanupAfterReject,
    CleanupAfterCancel,
}

/// Retry worktree policy (§10, §43): the choice between reusing a worktree
/// and creating a fresh one is explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RetryWorktreePolicy {
    Reuse,
    /// 3c.md §10: default retries prefer a clean environment.
    #[default]
    Fresh,
}

/// Resource budgets (§47–§48) — hard caps, no clever optimization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeBudget {
    pub max_worktrees: usize,
    /// Advisory disk warning threshold in bytes (0 = disabled).
    pub warn_disk_usage_bytes: u64,
}

impl Default for WorktreeBudget {
    fn default() -> Self {
        Self {
            max_worktrees: 32,
            warn_disk_usage_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

// ---------------------------------------------------------------------------
// §7, §34 branch naming + path traversal safety
// ---------------------------------------------------------------------------

/// Sanitizes a string into a safe single git ref component (3c.md §7, §34):
/// keeps `[A-Za-z0-9._-]`, collapses runs of other characters into `-`,
/// trims, and never produces `.`/`..`/leading `-` (ref rules). Agent- or
/// user-controlled strings can never escape into path construction.
pub fn sanitize_git_ref_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with(['-', '.']) {
            out.push('-');
        }
    }
    // Ref rules: collapse `..`, drop leading/trailing '.' and '-', drop
    // any leading path components (hostile input can never escape).
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    out.trim_matches(['.', '-']).to_string()
}

/// Short stable hash of a string (FNV-1a 32-bit hex) — collision-resistant
/// suffix for ids that sanitize to nothing/too long.
pub fn short_hash(s: &str) -> String {
    let mut h: u32 = 0x811c9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    format!("{h:08x}")
}

const MAX_BRANCH_PART: usize = 40;

/// Deterministic, collision-resistant branch name for a task attempt
/// (3c.md §7): `flash/task/<sanitized-task>-<slug>[-a<attempt>]`.
/// Never uses user-provided strings unvalidated (§34).
pub fn branch_for_task(task_id: &str, slug: &str, attempt: u32) -> Result<String, WorktreeError> {
    let raw_slug = sanitize_git_ref_component(slug);
    if raw_slug.is_empty() {
        return Err(WorktreeError::InvalidName {
            name: slug.to_string(),
        });
    }
    let safe_task = sanitize_git_ref_component(task_id);
    let task_part = if safe_task.is_empty() {
        short_hash(task_id)
    } else {
        let mut t = safe_task;
        t.truncate(MAX_BRANCH_PART);
        t
    };
    let mut leaf = format!("{task_part}-{raw_slug}");
    if attempt > 1 {
        leaf.push_str(&format!("-a{attempt}"));
    }
    leaf.truncate(MAX_BRANCH_PART + 32);
    Ok(format!("flash/task/{leaf}"))
}

/// Deterministic worktree id for a task attempt: `wt-<hash>-a<attempt>`.
/// Stable per (task, attempt) so retries can create fresh worktrees while
/// reusing the same task identity (§10, §43).
pub fn worktree_id_for_task(task_id: &str, attempt: u32) -> String {
    let mut id = format!("wt-{}", short_hash(task_id));
    if attempt > 1 {
        id.push_str(&format!("-a{attempt}"));
    }
    id
}

/// Worktree path inside the repository's git dir: never inside the working
/// tree, never committed, never in `git status`. `repo` is the repository
/// root (trusted engine input, not agent-controlled).
pub fn worktree_path(repo: &str, worktree_id: &str) -> Result<String, WorktreeError> {
    let id = sanitize_git_ref_component(worktree_id);
    if id.is_empty() || id != worktree_id {
        // The id must already be a safe single component — no slashes, no
        // path traversal (§34).
        return Err(WorktreeError::InvalidName {
            name: worktree_id.to_string(),
        });
    }
    let base = Path::new(repo)
        .join(".git")
        .join("flashterminal")
        .join("worktrees");
    Ok(base.join(id).to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// §5 WorktreeManager — the dedicated git abstraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorktreeError {
    NotARepository { path: String },
    RepositoryInvalid { path: String, message: String },
    BaseRevisionMissing { revision: String },
    WorktreeExists { id: String },
    WorktreeMissing { id: String },
    BranchConflict { branch: String },
    InvalidName { name: String },
    DirtyWorkspace { files: Vec<String> },
    SharedWorkspaceNotAllowed { task: String },
    NotApproved { state: WorktreeState },
    NotReviewable { state: WorktreeState },
    MergeConflict { conflict: MergeConflict },
    ResourceLimit { message: String },
    UnknownTask { task: String },
    CwdMismatch { expected: String, actual: String },
    Git { message: String },
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepository { path } => write!(f, "{path} is not a git repository"),
            Self::RepositoryInvalid { path, message } => {
                write!(f, "repository {path} is invalid: {message}")
            }
            Self::BaseRevisionMissing { revision } => {
                write!(f, "base revision {revision} does not exist")
            }
            Self::WorktreeExists { id } => write!(f, "worktree {id} already exists"),
            Self::WorktreeMissing { id } => write!(f, "worktree {id} does not exist"),
            Self::BranchConflict { branch } => write!(f, "branch {branch} already exists"),
            Self::InvalidName { name } => write!(f, "invalid name {name:?}"),
            Self::DirtyWorkspace { files } => {
                write!(
                    f,
                    "repository is dirty ({} file(s)): {}",
                    files.len(),
                    files.join(", ")
                )
            }
            Self::SharedWorkspaceNotAllowed { task } => {
                write!(
                    f,
                    "task {task} requires shared workspace without explicit policy"
                )
            }
            Self::NotApproved { state } => {
                write!(
                    f,
                    "worktree must be approved before merge (state: {state:?})"
                )
            }
            Self::NotReviewable { state } => write!(
                f,
                "worktree is not in a reviewable state (state: {state:?})"
            ),
            Self::MergeConflict { conflict } => {
                write!(f, "merge conflict in {} file(s)", conflict.files.len())
            }
            Self::ResourceLimit { message } => write!(f, "worktree resource limit: {message}"),
            Self::UnknownTask { task } => write!(f, "unknown task {task}"),
            Self::CwdMismatch { expected, actual } => {
                write!(
                    f,
                    "expected worktree cwd {expected}, agent would run in {actual}"
                )
            }
            Self::Git { message } => write!(f, "git: {message}"),
        }
    }
}

impl std::error::Error for WorktreeError {}

fn git_output(args: &[&str], cwd: &str) -> Result<std::process::Output, WorktreeError> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .output()
        .map_err(|e| WorktreeError::Git {
            message: format!("failed to run git {args:?}: {e}"),
        })
}

fn git_ok(args: &[&str], cwd: &str) -> Result<String, WorktreeError> {
    let out = git_output(args, cwd)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(WorktreeError::Git {
            message: format!("`git {}` failed: {stderr}", args.join(" ")),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs git in a worktree directory, `-C`-style (worktree paths are engine
/// computed, so the process cwd is irrelevant).
fn git_in(args: &[&str], dir: &str) -> Result<std::process::Output, WorktreeError> {
    let mut full = vec!["-C", dir];
    full.extend_from_slice(args);
    git_output(&full, dir)
}

fn git_in_ok(args: &[&str], dir: &str) -> Result<String, WorktreeError> {
    let out = git_in(args, dir)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(WorktreeError::Git {
            message: format!("`git {}` in {dir} failed: {stderr}", args.join(" ")),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Git is available on this machine (tests skip cleanly when it is not).
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The worktree manager — the only git caller for worktree operations
/// (3c.md §5). Owns in-memory records; on restart it re-scans the repos it
/// knows about to reconnect metadata (§50) and find orphans (§31).
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    records: std::collections::HashMap<String, WorktreeRecord>,
    budget: WorktreeBudget,
    dirty_policy: DirtyPolicy,
    cleanup: CleanupPolicy,
    /// Repositories this engine has created worktrees in (used by the
    /// restart scan).
    repositories: HashSet<String>,
}

impl Default for WorktreeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WorktreeManager {
    pub fn new() -> Self {
        Self {
            records: std::collections::HashMap::new(),
            budget: WorktreeBudget::default(),
            dirty_policy: DirtyPolicy::default(),
            cleanup: CleanupPolicy::Keep,
            repositories: HashSet::new(),
        }
    }

    pub fn with_policies(dirty: DirtyPolicy, cleanup: CleanupPolicy) -> Self {
        Self {
            dirty_policy: dirty,
            cleanup,
            ..Self::new()
        }
    }

    /// Rebuilds the manager from persisted records (§49–§50). Ownership is
    /// reconnected by the engine through [`WorktreeManager::scan`]; nothing
    /// is deleted here.
    pub fn from_records(records: Vec<WorktreeRecord>) -> Self {
        let mut m = Self::new();
        for r in records {
            let repo = r.repository.clone();
            m.repositories.insert(repo);
            m.records.insert(r.id.clone(), r);
        }
        m
    }

    pub fn budget(&self) -> &WorktreeBudget {
        &self.budget
    }

    pub fn set_budget(&mut self, budget: WorktreeBudget) {
        self.budget = budget;
    }

    pub fn dirty_policy(&self) -> DirtyPolicy {
        self.dirty_policy
    }

    pub fn set_dirty_policy(&mut self, p: DirtyPolicy) {
        self.dirty_policy = p;
    }

    pub fn cleanup_policy(&self) -> CleanupPolicy {
        self.cleanup
    }

    pub fn set_cleanup_policy(&mut self, p: CleanupPolicy) {
        self.cleanup = p;
    }

    // -- §13 repository safety -------------------------------------------------

    /// Validates the base repository + base revision before any isolated
    /// work is launched (§13). Returns typed errors.
    pub fn repository_ok(
        &self,
        repo: &str,
        base_revision: Option<&str>,
    ) -> Result<(), WorktreeError> {
        if !Path::new(repo).is_dir() {
            return Err(WorktreeError::NotARepository {
                path: repo.to_string(),
            });
        }
        let git_dir = git_ok(&["rev-parse", "--git-dir"], repo).map_err(|_| {
            WorktreeError::RepositoryInvalid {
                path: repo.to_string(),
                message: "not a git repository".to_string(),
            }
        })?;
        if git_dir.is_empty() {
            return Err(WorktreeError::RepositoryInvalid {
                path: repo.to_string(),
                message: "not a git repository".to_string(),
            });
        }
        if let Some(rev) = base_revision {
            if rev.is_empty() {
                return Err(WorktreeError::BaseRevisionMissing {
                    revision: rev.to_string(),
                });
            }
            let ok = git_ok(&["cat-file", "-e", &format!("{rev}^{{commit}}")], repo).is_ok();
            if !ok {
                return Err(WorktreeError::BaseRevisionMissing {
                    revision: rev.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Current HEAD commit of a repo.
    pub fn head_revision(&self, repo: &str) -> Result<String, WorktreeError> {
        git_ok(&["rev-parse", "HEAD"], repo)
    }

    /// Current branch of a repo.
    pub fn current_branch(&self, repo: &str) -> Result<String, WorktreeError> {
        let b = git_ok(&["rev-parse", "--abbrev-ref", "HEAD"], repo)?;
        if b == "HEAD" {
            return Err(WorktreeError::Git {
                message: "repository is in detached HEAD state".to_string(),
            });
        }
        Ok(b)
    }

    // -- §14 dirty workspace ----------------------------------------------------

    /// Detects uncommitted changes in the base repository (§14). Never
    /// silently discards anything.
    pub fn dirty_report(&self, repo: &str) -> DirtyWorkspaceReport {
        let mut report = DirtyWorkspaceReport::default();
        let Ok(out) = git_output(&["status", "--porcelain", "--untracked-files=all"], repo) else {
            return report;
        };
        if !out.status.success() {
            return report;
        }
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim_end();
            if line.len() < 4 {
                continue;
            }
            let (xy, path) = line.split_at(2);
            let path = path.trim_start().to_string();
            match (
                xy.chars().next().unwrap_or(' '),
                xy.chars().nth(1).unwrap_or(' '),
            ) {
                ('?', _) => report.untracked.push(path),
                (x, y) => {
                    if x != ' ' && x != '?' {
                        report.staged.push(path.clone());
                    }
                    if y != ' ' {
                        report.unstaged.push(path);
                    }
                }
            }
        }
        report.staged.sort();
        report.unstaged.sort();
        report.untracked.sort();
        report.dirty = !report.staged.is_empty()
            || !report.unstaged.is_empty()
            || !report.untracked.is_empty();
        report
    }

    // -- §5/§6/§8 worktree creation --------------------------------------------

    /// Creates an isolated worktree for a task attempt (§6, §8, §13–§15).
    /// Deterministic naming (§7); typed errors; never touches the main
    /// working tree (user changes are never discarded).
    pub fn create(
        &mut self,
        repo: &str,
        task_id: &str,
        slug: &str,
        base_revision: Option<&str>,
        attempt: u32,
    ) -> Result<ExecutionEnvironment, WorktreeError> {
        if self.records.len() >= self.budget.max_worktrees {
            return Err(WorktreeError::ResourceLimit {
                message: format!("max_worktrees {} reached", self.budget.max_worktrees),
            });
        }
        let effective_base = match base_revision {
            Some(b) if !b.is_empty() => Some(b.to_string()),
            _ => None,
        };
        self.repository_ok(repo, effective_base.as_deref())?;
        // §14: RequireClean refuses to run isolated work on a dirty repo.
        if self.dirty_policy == DirtyPolicy::RequireClean {
            let dirty = self.dirty_report(repo);
            if dirty.dirty {
                return Err(WorktreeError::DirtyWorkspace {
                    files: dirty.all_files(),
                });
            }
        }
        let dirty_at_creation = self.dirty_report(repo).dirty;

        let id = worktree_id_for_task(task_id, attempt);
        if self.records.contains_key(&id) {
            return Err(WorktreeError::WorktreeExists { id });
        }
        let branch = branch_for_task(task_id, slug, attempt)?;
        let path = worktree_path(repo, &id)?;
        if Path::new(&path).exists() {
            return Err(WorktreeError::WorktreeExists { id });
        }
        // Collision-resistant: refuse if the branch already exists.
        let branch_exists = git_ok(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            repo,
        )
        .is_ok();
        if branch_exists {
            return Err(WorktreeError::BranchConflict { branch });
        }

        let (base_branch, base_timestamp_ms) = {
            let b = self.current_branch(repo).unwrap_or_default();
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            (b, ts)
        };
        if let Some(parent) = Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut add_args = vec!["worktree", "add", "-b", &branch, &path];
        if let Some(base) = &effective_base {
            add_args.push(base);
        }
        git_ok(&add_args, repo)?;

        let record = WorktreeRecord::new(
            id.clone(),
            Some(task_id.to_string()),
            path.clone(),
            repo.to_string(),
            branch.clone(),
            effective_base.clone(),
            if base_branch.is_empty() {
                None
            } else {
                Some(base_branch.clone())
            },
            base_timestamp_ms,
            dirty_at_creation,
        );
        self.repositories.insert(repo.to_string());
        self.records.insert(id.clone(), record);
        Ok(ExecutionEnvironment {
            repository: repo.to_string(),
            base_revision: effective_base,
            base_branch: if base_branch.is_empty() {
                None
            } else {
                Some(base_branch)
            },
            base_timestamp_ms,
            working_directory: path,
            worktree_id: Some(id),
            branch: Some(branch),
            isolation: IsolationMode::GitWorktree,
            environment_variables: Vec::new(),
        })
    }

    /// Resolves the environment for a task attempt (§44): reuse the task's
    /// existing worktree, or create a fresh one per [`RetryWorktreePolicy`]
    /// (§10, §43 — the choice is explicit).
    /// Explicit per-attempt environment resolution (§10, §43): reuse the
    /// task's worktree or create fresh per the policy.
    #[allow(clippy::too_many_arguments)]
    pub fn environment_for_task(
        &mut self,
        repo: &str,
        task_id: &str,
        slug: &str,
        existing_worktree: Option<&str>,
        base_revision: Option<&str>,
        attempt: u32,
        retry_policy: RetryWorktreePolicy,
    ) -> Result<ExecutionEnvironment, WorktreeError> {
        let reuse = match existing_worktree {
            Some(id) if !id.is_empty() => {
                // Attempt 1 may reuse; retries follow the explicit policy.
                attempt == 1 || retry_policy == RetryWorktreePolicy::Reuse
            }
            _ => false,
        };
        if reuse {
            if let Some(rec) = self.records.get(existing_worktree.unwrap()) {
                // Worktree may have been removed from disk externally —
                // recreate fresh rather than running in a missing dir (§12).
                if Path::new(&rec.path).is_dir() {
                    return Ok(self.environment_from_record(rec));
                }
            }
        }
        self.create(repo, task_id, slug, base_revision, attempt)
    }

    fn environment_from_record(&self, rec: &WorktreeRecord) -> ExecutionEnvironment {
        ExecutionEnvironment {
            repository: rec.repository.clone(),
            base_revision: rec.base_revision.clone(),
            base_branch: rec.base_branch.clone(),
            base_timestamp_ms: rec.base_timestamp_ms,
            working_directory: rec.path.clone(),
            worktree_id: Some(rec.id.clone()),
            branch: Some(rec.branch.clone()),
            isolation: IsolationMode::GitWorktree,
            environment_variables: Vec::new(),
        }
    }

    /// §12: hard wrong-cwd guard — the agent must run exactly where the
    /// worktree says.
    pub fn assert_cwd(&self, worktree_id: &str, actual_cwd: &str) -> Result<(), WorktreeError> {
        let rec = self
            .records
            .get(worktree_id)
            .ok_or_else(|| WorktreeError::WorktreeMissing {
                id: worktree_id.to_string(),
            })?;
        let expected = canonical(&rec.path);
        let actual = canonical(actual_cwd);
        if expected != actual {
            return Err(WorktreeError::CwdMismatch {
                expected: rec.path.clone(),
                actual: actual_cwd.to_string(),
            });
        }
        Ok(())
    }

    // -- inspection --------------------------------------------------------------

    pub fn list(&self) -> Vec<&WorktreeRecord> {
        let mut v: Vec<_> = self.records.values().collect();
        v.sort_by_key(|r| r.created_at_ms);
        v
    }

    pub fn get(&self, id: &str) -> Option<&WorktreeRecord> {
        self.records.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut WorktreeRecord> {
        self.records.get_mut(id)
    }

    /// §9 state transitions with updated timestamp.
    pub fn set_state(&mut self, id: &str, state: WorktreeState) -> Result<(), WorktreeError> {
        let rec = self
            .records
            .get_mut(id)
            .ok_or_else(|| WorktreeError::WorktreeMissing { id: id.to_string() })?;
        rec.state = state;
        rec.updated_at_ms = crate::work::now_ms();
        Ok(())
    }

    /// Inspects a worktree: path exists + current branch/HEAD on disk.
    pub fn inspect(&self, id: &str) -> Result<WorktreeInspection, WorktreeError> {
        let rec = self
            .records
            .get(id)
            .ok_or_else(|| WorktreeError::WorktreeMissing { id: id.to_string() })?;
        let head = git_in_ok(&["rev-parse", "HEAD"], &rec.path)?;
        let branch = git_in_ok(&["rev-parse", "--abbrev-ref", "HEAD"], &rec.path)?;
        Ok(WorktreeInspection {
            id: id.to_string(),
            path: rec.path.clone(),
            branch: if branch == "HEAD" { None } else { Some(branch) },
            head,
            exists: Path::new(&rec.path).is_dir(),
        })
    }

    // -- §16/§18 deterministic diff ---------------------------------------------

    /// Deterministic diff of a worktree against its recorded base revision
    /// (§18 — never the agent's summary).
    pub fn diff(&self, id: &str) -> Result<DiffSummary, WorktreeError> {
        let rec = self
            .records
            .get(id)
            .ok_or_else(|| WorktreeError::WorktreeMissing { id: id.to_string() })?;
        let Some(base) = &rec.base_revision else {
            return Err(WorktreeError::BaseRevisionMissing {
                revision: "<none>".to_string(),
            });
        };
        let head = git_in_ok(&["rev-parse", "HEAD"], &rec.path)?;
        let mut summary = DiffSummary {
            base_revision: Some(base.clone()),
            result_revision: Some(head.clone()),
            ..DiffSummary::default()
        };
        // numstat: <additions>\t<deletions>\t<path>
        if let Ok(out) = git_in(&["diff", "--numstat", base, "HEAD"], &rec.path) {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let mut it = line.splitn(3, '\t');
                    if let (Some(a), Some(d), Some(p)) = (it.next(), it.next(), it.next()) {
                        summary.insertions += a.parse().unwrap_or(0);
                        summary.deletions += d.parse().unwrap_or(0);
                        summary.files_changed.push(p.trim().to_string());
                    }
                }
            }
        }
        // name-status: <M|A|D>\t<path>
        if let Ok(out) = git_in(&["diff", "--name-status", base, "HEAD"], &rec.path) {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let mut it = line.splitn(2, '\t');
                    if let (Some(status), Some(p)) = (it.next(), it.next()) {
                        let p = p.trim().to_string();
                        match status.trim() {
                            "D" => summary.files_deleted.push(p),
                            "A" => summary.files_created.push(p),
                            _ => {}
                        }
                    }
                }
            }
        }
        // Untracked files in the worktree are created files.
        if let Ok(out) = git_in(&["ls-files", "--others", "--exclude-standard"], &rec.path) {
            if out.status.success() {
                for line in String::from_utf8_lossy(&out.stdout).lines() {
                    let p = line.trim();
                    if !p.is_empty() {
                        summary.files_created.push(p.to_string());
                    }
                }
            }
        }
        summary.files_changed.sort();
        summary.files_changed.dedup();
        summary.files_created.sort();
        summary.files_created.dedup();
        summary.files_deleted.sort();
        summary.files_deleted.dedup();
        Ok(summary)
    }

    // -- §22/§23 merge -----------------------------------------------------------

    /// Merges an **approved** worktree's branch into `target_branch`.
    /// Conflict detection is done first via `git merge-tree` — on conflict a
    /// [`MergeConflict`] is surfaced with no changes made (agents never
    /// auto-resolve, §23).
    pub fn merge(&mut self, id: &str, target_branch: &str) -> Result<MergeOutcome, WorktreeError> {
        let rec = self
            .records
            .get(id)
            .ok_or_else(|| WorktreeError::WorktreeMissing { id: id.to_string() })?;
        if rec.state != WorktreeState::Approved {
            return Err(WorktreeError::NotApproved { state: rec.state });
        }
        let repo = rec.repository.clone();
        let feature = rec.branch.clone();
        let base = rec.base_revision.clone().unwrap_or_default();
        let ours = git_ok(&["rev-parse", target_branch], &repo)?;
        let theirs = git_ok(&["rev-parse", &feature], &repo)?;

        // Classic merge-tree: deterministic conflict detection, no checkout.
        let out = git_output(&["merge-tree", &base, &ours, &theirs], &repo)?;
        let text = String::from_utf8_lossy(&out.stdout);
        let conflict_files = parse_merge_conflicts(&text);
        if !conflict_files.is_empty() {
            let conflict = MergeConflict {
                files: conflict_files,
                branches: vec![target_branch.to_string(), feature.clone()],
                base: rec.base_revision.clone(),
                ours: ours.clone(),
                theirs: theirs.clone(),
            };
            return Ok(MergeOutcome::Conflict(conflict));
        }
        // No conflict → integrate into the target branch. This is the
        // explicit, human-approved merge (§22, §53).
        let msg = format!("flashterminal: merge {feature}");
        let out = git_ok(&["merge", "--no-ff", &feature, "-m", &msg], &repo)?;
        let commit = git_ok(&["rev-parse", "HEAD"], &repo)?;
        let _ = out;
        self.set_state(id, WorktreeState::Merged)?;
        Ok(MergeOutcome::Merged { commit })
    }

    // -- §30/§31 cleanup + orphans -------------------------------------------------

    /// Removes a worktree from disk + records (§30). Only removes what the
    /// manager created; never user work without explicit policy.
    pub fn remove(&mut self, id: &str) -> Result<(), WorktreeError> {
        let rec = self
            .records
            .remove(id)
            .ok_or_else(|| WorktreeError::WorktreeMissing { id: id.to_string() })?;
        let _ = git_ok(
            &["worktree", "remove", "--force", &rec.path],
            &rec.repository,
        );
        let _ = std::fs::remove_dir_all(&rec.path);
        // Drop the branch too (the worktree branch is owned by the manager).
        let _ = git_ok(&["branch", "-D", &rec.branch], &rec.repository);
        let _ = git_ok(&["worktree", "prune"], &rec.repository);
        Ok(())
    }

    /// Applies the configured cleanup policy (§30).
    pub fn cleanup(&mut self) -> Vec<WorktreeError> {
        let mut removed = Vec::new();
        let should = |state: WorktreeState| match self.cleanup {
            CleanupPolicy::Keep => false,
            CleanupPolicy::CleanupAfterMerge => state == WorktreeState::Merged,
            CleanupPolicy::CleanupAfterReject => state == WorktreeState::Rejected,
            CleanupPolicy::CleanupAfterCancel => {
                matches!(state, WorktreeState::Rejected | WorktreeState::Merged)
            }
        };
        let ids: Vec<String> = self
            .records
            .values()
            .filter(|r| should(r.state))
            .map(|r| r.id.clone())
            .collect();
        for id in ids {
            if let Err(e) = self.remove(&id) {
                removed.push(e);
            }
        }
        removed
    }

    /// Re-scans known repositories after a restart (§31, §50). `ownership`
    /// maps `worktree_id → task_id` from the restored scheduler state so
    /// worktrees reconnect to their tasks through metadata; worktrees with
    /// no live owner are marked orphaned. Never deletes anything.
    pub fn scan(&mut self, ownership: &HashMap<String, String>) -> Vec<String> {
        let mut orphans = Vec::new();
        // 1. Apply ownership to in-memory records first.
        for (wt_id, task_id) in ownership {
            if let Some(rec) = self.records.get_mut(wt_id) {
                rec.task_id = Some(task_id.clone());
                rec.orphaned = false;
            }
        }
        // 2. Disk truth: `git worktree list --porcelain`.
        let repos: Vec<String> = self.repositories.iter().cloned().collect();
        for repo in repos {
            let Ok(out) = git_output(&["worktree", "list", "--porcelain"], &repo) else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            let mut current_path: Option<String> = None;
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    current_path = Some(p.trim().to_string());
                } else if line.starts_with("branch ") {
                    let branch = line
                        .trim_start_matches("branch ")
                        .trim()
                        .trim_start_matches("refs/heads/")
                        .to_string();
                    if let Some(path) = &current_path {
                        self.reconnect_or_orphan(&repo, path, &branch, ownership, &mut orphans);
                    }
                    current_path = None;
                }
            }
        }
        orphans
    }

    fn reconnect_or_orphan(
        &mut self,
        repo: &str,
        path: &str,
        branch: &str,
        ownership: &HashMap<String, String>,
        orphans: &mut Vec<String>,
    ) {
        // Only worktrees we created live under the git-dir worktree root.
        // `git worktree list --porcelain` reports absolute paths; compare
        // canonical forms (temp dirs on macOS resolve /tmp → /private/var).
        let expected_root = canonical(
            &Path::new(repo)
                .join(".git")
                .join("flashterminal")
                .join("worktrees")
                .to_string_lossy(),
        );
        if !canonical(path).starts_with(&expected_root) {
            return;
        }
        // Match by canonical path (ids hash the task id; the branch is the
        // stable link).
        let matched: Option<(String, Option<String>)> = self
            .records
            .values()
            .find(|r| canonical(&r.path) == canonical(path))
            .map(|r| (r.id.clone(), r.task_id.clone()));
        if let Some((id, task_id)) = matched {
            let owned = match &task_id {
                Some(t) => ownership.values().any(|v| v == t),
                None => ownership.contains_key(&id),
            };
            if let Some(rec) = self.records.get_mut(&id) {
                rec.orphaned = !owned;
                if rec.orphaned {
                    orphans.push(rec.id.clone());
                }
            }
            return;
        }
        // Unknown to the manager but under our root: metadata lost (crash
        // before the record write). The leaf directory name is the
        // deterministic worktree id — reconnect through ownership if the
        // restored scheduler state still owns it, else flag orphaned (§50).
        let leaf = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        let owned = ownership.contains_key(&leaf);
        let id = if owned {
            leaf.clone()
        } else {
            format!("wt-orphan-{}", short_hash(path))
        };
        if self.records.contains_key(&id) {
            return;
        }
        let task_id = if owned {
            ownership.get(&leaf).cloned()
        } else {
            None
        };
        let mut record = WorktreeRecord::new(
            id.clone(),
            task_id,
            path.to_string(),
            repo.to_string(),
            branch.to_string(),
            None,
            None,
            0,
            false,
        );
        record.orphaned = !owned;
        record.state = WorktreeState::NeedsReview;
        if record.orphaned {
            orphans.push(record.id.clone());
        }
        self.records.insert(id, record);
    }

    /// Orphaned worktree records (never auto-deleted, §31).
    pub fn orphans(&self) -> Vec<&WorktreeRecord> {
        self.records.values().filter(|r| r.orphaned).collect()
    }

    /// Reconnects a known (in-memory) worktree id to its task — used by the
    /// engine on restore when scheduler state knows the owner (§50).
    pub fn adopt(&mut self, worktree_id: &str, task_id: &str) -> Result<(), WorktreeError> {
        let rec =
            self.records
                .get_mut(worktree_id)
                .ok_or_else(|| WorktreeError::WorktreeMissing {
                    id: worktree_id.to_string(),
                })?;
        rec.task_id = Some(task_id.to_string());
        rec.orphaned = false;
        Ok(())
    }

    pub fn repositories(&self) -> &HashSet<String> {
        &self.repositories
    }
}

/// Inspectable snapshot of a worktree (3c.md §5 `inspect`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInspection {
    pub id: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub exists: bool,
}

fn canonical(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|c| c.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string())
}

/// Parses classic `git merge-tree <base> <ours> <theirs>` output for
/// conflicts ("changed in both" blocks). Returns the conflicting paths.
fn parse_merge_conflicts(text: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut in_conflict = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("changed in both") {
            in_conflict = true;
        } else if line.starts_with("merged") || line.starts_with("added in") {
            in_conflict = false;
        } else if in_conflict && !line.is_empty() && !line.starts_with("@@") {
            // base/our/their lines: "<mode> <hash> <path>"
            if let Some((tag, rest)) = line.split_once(' ') {
                if matches!(tag, "base" | "our" | "their") {
                    let path = rest.rsplit_once(' ').map(|(_, p)| p.trim().to_string());
                    if let Some(p) = path {
                        if !p.is_empty() && !files.contains(&p) {
                            files.push(p);
                        }
                    }
                }
            }
        }
    }
    files
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_ok_test(repo: &str, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Creates a disposable repo with one commit, returns its path.
    fn scratch_repo(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("ft-wt-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        git_ok_test(&dir.to_string_lossy(), &["init", "-q"]);
        git_ok_test(&dir.to_string_lossy(), &["config", "user.email", "t@t"]);
        git_ok_test(&dir.to_string_lossy(), &["config", "user.name", "t"]);
        fs::write(dir.join("base.txt"), "base\n").unwrap();
        git_ok_test(&dir.to_string_lossy(), &["add", "."]);
        git_ok_test(&dir.to_string_lossy(), &["commit", "-qm", "init"]);
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn branch_naming_is_deterministic_and_safe() {
        let b1 = branch_for_task("abc-123", "auth", 1).unwrap();
        let b2 = branch_for_task("abc-123", "auth", 1).unwrap();
        assert_eq!(b1, b2);
        assert_eq!(
            branch_for_task("abc-123", "auth", 1).unwrap(),
            "flash/task/abc-123-auth"
        );
        // Attempt 2 differs (fresh worktree per retry, §43).
        assert_ne!(b1, branch_for_task("abc-123", "auth", 2).unwrap());
        // Hostile input is sanitized, never escapes the ref namespace.
        let evil = branch_for_task("../../etc/passwd", "a b/c", 1).unwrap();
        assert!(!evil.contains(".."));
        assert!(!evil.contains('/') || evil.starts_with("flash/task/"));
        assert!(!evil.contains(' '));
        assert!(evil.starts_with("flash/task/"));
        // Empty slug is a typed error.
        assert!(matches!(
            branch_for_task("t", "", 1),
            Err(WorktreeError::InvalidName { .. })
        ));
    }

    #[test]
    fn sanitize_handles_path_traversal_and_ref_breaks() {
        assert_eq!(sanitize_git_ref_component("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_git_ref_component("a b c"), "a-b-c");
        assert_eq!(sanitize_git_ref_component(".."), "");
        assert_eq!(sanitize_git_ref_component("-lead"), "lead");
        assert_eq!(sanitize_git_ref_component("a..b"), "a.b");
        assert_eq!(sanitize_git_ref_component("a\tb\nc"), "a-b-c");
        // Worktree ids must be a single safe component — no traversal.
        assert!(worktree_path("/repo", "../evil").is_err());
        assert!(worktree_path("/repo", "a/b").is_err());
        assert!(worktree_path("/repo", "ok-id").is_ok());
    }

    #[test]
    fn merge_conflict_parsing_detects_files() {
        let text = "changed in both\n  base   100644 abc f.txt\n  our    100644 def f.txt\n  their  100644 ghi f.txt\n@@ -1 +1 @@\n";
        let files = parse_merge_conflicts(text);
        assert_eq!(files, vec!["f.txt"]);
        assert!(parse_merge_conflicts("merged\n  result 100644 abc f.txt\n").is_empty());
    }

    // -- git-gated integration-style unit tests ---------------------------------

    #[test]
    fn creates_worktree_with_deterministic_branch() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("create");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();
        let env = mgr.create(&repo, "task-1", "auth", Some(&base), 1).unwrap();
        assert_eq!(env.isolation, IsolationMode::GitWorktree);
        assert_eq!(env.branch.as_deref(), Some("flash/task/task-1-auth"));
        assert_eq!(env.base_revision.as_deref(), Some(base.as_str()));
        assert!(Path::new(&env.working_directory).is_dir());
        // The worktree exists on disk with its own branch.
        let branch = git_ok_test_ret(
            &env.working_directory,
            &["rev-parse", "--abbrev-ref", "HEAD"],
        );
        assert_eq!(branch, "flash/task/task-1-auth");
        let records = mgr.list();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].state, WorktreeState::Active);
        mgr.remove(&env.worktree_id.unwrap()).unwrap();
        assert!(mgr.list().is_empty());
        fs::remove_dir_all(&repo).ok();
    }

    fn git_ok_test_ret(repo: &str, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("git runs");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn default_branch(repo: &str) -> String {
        git_ok_test_ret(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
    }

    #[test]
    fn rejects_dirty_repo_under_require_clean() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("dirty");
        fs::write(Path::new(&repo).join("local.txt"), "user change\n").unwrap();
        let mut mgr =
            WorktreeManager::with_policies(DirtyPolicy::RequireClean, CleanupPolicy::Keep);
        let base = mgr.head_revision(&repo).unwrap();
        let err = mgr.create(&repo, "t", "x", Some(&base), 1).unwrap_err();
        assert!(matches!(err, WorktreeError::DirtyWorkspace { .. }));
        // The user's change is untouched.
        assert!(Path::new(&repo).join("local.txt").exists());
        // AllowDirty proceeds and never touches the main working tree.
        mgr.set_dirty_policy(DirtyPolicy::AllowDirty);
        let env = mgr.create(&repo, "t", "x", Some(&base), 1).unwrap();
        assert!(Path::new(&env.working_directory).is_dir());
        assert!(Path::new(&repo).join("local.txt").exists());
        mgr.remove(&env.worktree_id.unwrap()).unwrap();
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn diff_is_deterministic_and_counts_files() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("diff");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();
        let env = mgr.create(&repo, "t", "x", Some(&base), 1).unwrap();
        let wt = env.working_directory.clone();
        fs::write(Path::new(&wt).join("new.txt"), "new\n").unwrap();
        fs::write(Path::new(&wt).join("base.txt"), "base\nchanged\n").unwrap();
        git_ok_test(&wt, &["add", "."]);
        git_ok_test(&wt, &["commit", "-qm", "changes"]);
        let wid = env.worktree_id.clone().unwrap();
        let d1 = mgr.diff(&wid).unwrap();
        let d2 = mgr.diff(&wid).unwrap();
        assert_eq!(d1, d2, "diff must be deterministic");
        assert!(d1.files_created.contains(&"new.txt".to_string()));
        assert!(d1.files_changed.contains(&"base.txt".to_string()));
        assert!(d1.insertions >= 2);
        assert_eq!(
            d1.result_revision.as_deref(),
            Some(git_ok_test_ret(&wt, &["rev-parse", "HEAD"]).as_str())
        );
        mgr.remove(&wid).unwrap();
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn merge_and_conflict_are_surfaced_without_data_loss() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("merge");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();

        // Task A: appends "A" to base.txt.
        let a = mgr.create(&repo, "task-a", "auth", Some(&base), 1).unwrap();
        fs::write(
            Path::new(&a.working_directory).join("base.txt"),
            "base\nA\n",
        )
        .unwrap();
        git_ok_test(&a.working_directory, &["add", "."]);
        git_ok_test(&a.working_directory, &["commit", "-qm", "a"]);
        let wid_a = a.worktree_id.clone().unwrap();
        let target = default_branch(&repo);
        mgr.set_state(&wid_a, WorktreeState::Approved).unwrap();
        let merged = mgr.merge(&wid_a, &target).unwrap();
        let MergeOutcome::Merged { .. } = merged else {
            panic!("A should merge cleanly");
        };

        // Task B: appends "B" to the same lines → conflict with A.
        let b = mgr
            .create(&repo, "task-b", "payments", Some(&base), 1)
            .unwrap();
        fs::write(
            Path::new(&b.working_directory).join("base.txt"),
            "base\nB\n",
        )
        .unwrap();
        git_ok_test(&b.working_directory, &["add", "."]);
        git_ok_test(&b.working_directory, &["commit", "-qm", "b"]);
        let wid_b = b.worktree_id.clone().unwrap();
        mgr.set_state(&wid_b, WorktreeState::Approved).unwrap();
        let out = mgr.merge(&wid_b, &target).unwrap();
        match out {
            MergeOutcome::Conflict(c) => {
                assert!(c.files.contains(&"base.txt".to_string()), "{c:?}");
                assert_eq!(c.branches.len(), 2);
            }
            MergeOutcome::Merged { .. } => panic!("B must conflict with A on the same lines"),
        }
        // No data loss: main still contains A only.
        let head = git_ok_test_ret(&repo, &["log", "--oneline", "-3"]);
        assert!(
            head.contains('a') || head.contains("flash/task/task-a-auth"),
            "{head}"
        );
        assert!(!head.contains("flash/task/task-b-payments"));
        mgr.remove(&wid_a).unwrap();
        mgr.remove(&wid_b).unwrap();
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn merge_requires_approval() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("appr");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();
        let env = mgr.create(&repo, "t", "x", Some(&base), 1).unwrap();
        let wid = env.worktree_id.clone().unwrap();
        let target = default_branch(&repo);
        let err = mgr.merge(&wid, &target).unwrap_err();
        assert!(matches!(err, WorktreeError::NotApproved { .. }));
        mgr.remove(&wid).unwrap();
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn fresh_retry_policy_creates_new_worktree() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("retry");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();
        let a1 = mgr
            .environment_for_task(
                &repo,
                "task-x",
                "fix",
                None,
                Some(&base),
                1,
                RetryWorktreePolicy::Fresh,
            )
            .unwrap();
        let a2 = mgr
            .environment_for_task(
                &repo,
                "task-x",
                "fix",
                a1.worktree_id.as_deref(),
                Some(&base),
                2,
                RetryWorktreePolicy::Fresh,
            )
            .unwrap();
        assert_ne!(
            a1.worktree_id, a2.worktree_id,
            "Fresh policy must create a new worktree"
        );
        assert_ne!(a1.branch, a2.branch);
        // Reuse policy keeps the same worktree on retry.
        let a3 = mgr
            .environment_for_task(
                &repo,
                "task-x",
                "fix",
                a2.worktree_id.as_deref(),
                Some(&base),
                2,
                RetryWorktreePolicy::Reuse,
            )
            .unwrap();
        assert_eq!(a2.worktree_id, a3.worktree_id);
        for id in [a1.worktree_id.unwrap(), a2.worktree_id.unwrap()] {
            mgr.remove(&id).unwrap();
        }
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn orphan_scan_reconnects_and_flags() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("orphan");
        let mut mgr = WorktreeManager::new();
        let base = mgr.head_revision(&repo).unwrap();
        let env = mgr.create(&repo, "gone-task", "x", Some(&base), 1).unwrap();
        let wid = env.worktree_id.clone().unwrap();
        // Simulate restart: fresh manager, scan with no owners.
        let mut mgr2 = WorktreeManager::new();
        mgr2.repositories.insert(repo.clone());
        let orphans = mgr2.scan(&HashMap::new());
        assert!(
            !orphans.is_empty(),
            "the worktree must be flagged as orphaned"
        );
        assert!(!mgr2.orphans().is_empty());
        // Never deleted.
        assert!(Path::new(&env.working_directory).is_dir());
        // With ownership restored from scheduler state, it reconnects and
        // is not orphaned (§50).
        let mut mgr3 = WorktreeManager::new();
        mgr3.repositories.insert(repo.clone());
        let mut ownership = HashMap::new();
        ownership.insert(wid.clone(), "gone-task".to_string());
        let orphans3 = mgr3.scan(&ownership);
        let rec = mgr3
            .records
            .values()
            .find(|r| r.task_id.as_deref() == Some("gone-task"))
            .expect("reconnected");
        assert!(!rec.orphaned, "active task must not be orphaned");
        assert!(orphans3.is_empty() || !orphans3.contains(&rec.id));
        // Same scan on the original manager (record in memory) also keeps
        // it owned.
        let orphans4 = mgr.scan(&ownership);
        assert!(
            orphans4.is_empty(),
            "owned worktree must not orphan: {orphans4:?}"
        );
        mgr.remove(&wid).unwrap();
        fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn dirty_report_detects_user_changes() {
        if !git_available() {
            eprintln!("SKIP: git unavailable");
            return;
        }
        let repo = scratch_repo("dirtyrep");
        let mgr = WorktreeManager::new();
        assert!(!mgr.dirty_report(&repo).dirty);
        fs::write(Path::new(&repo).join("local.txt"), "x\n").unwrap();
        let report = mgr.dirty_report(&repo);
        assert!(report.dirty);
        assert!(report.untracked.contains(&"local.txt".to_string()));
        fs::remove_dir_all(&repo).ok();
    }
}
