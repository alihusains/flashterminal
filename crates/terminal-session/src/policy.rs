//! Central policy engine (phases/4.md §1–§16).
//!
//! ```text
//! User
//!   ↓
//! Policy Engine
//!   ↓
//! Planner
//!   ↓
//! Plan Validator
//!   ↓
//! Approval
//!   ↓
//! Execution
//! ```
//!
//! The planner is never the final authority. Every executable action is
//! evaluated against policy domains:
//!
//! ```text
//! Filesystem   Network   Process   Secrets   Shell
//! Workspace    Agent     Budget    Autonomy
//! ```
//!
//! Design rules:
//!
//! - **Deterministic rules come first.** Dangerous-command protection
//!   (§4) and path validation (§8) are pure, tested rules — never LLM
//!   classification.
//! - **Conservative by default.** When uncertain, the decision is
//!   [`PolicyDecision::RequireApproval`] — never an optimistic Allow.
//! - **Never silently downgrade** a Deny to an Allow. Deny only ever comes
//!   from deterministic rules, and the decision record carries the source.
//! - **Structured execution** (§5): process actions carry
//!   `executable` + `arguments[]`, never shell strings. Values destined
//!   for shell interpretation are gated by [`ShellInterpolationGuard`]
//!   (§6).
//! - **Approval integrity** (§15–§16): approvals are bound to a workflow,
//!   agent, action hash and expiry — replay, stale reuse, wrong-workflow
//!   and post-approval action changes are all rejected.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::planning::now_ms;

// ---------------------------------------------------------------------------
// §2 policy decisions
// ---------------------------------------------------------------------------

/// Outcome of evaluating one action against policy (§2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyDecision {
    Allow,
    Deny,
    RequireApproval,
}

impl PolicyDecision {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::RequireApproval => "require_approval",
        }
    }

    /// Follow-on for an evaluation: what the caller must do.
    pub fn resolved_by(&self) -> &'static str {
        match self {
            Self::Allow => "execute",
            Self::Deny => "blocked (deterministic policy)",
            Self::RequireApproval => "approval required",
        }
    }
}

/// Where a decision came from — auditability for "why did this happen".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicySource {
    /// A deterministic dangerous-command / filesystem / secret rule.
    DeterministicRule,
    /// Explicit policy configuration (filesystem scope, network policy…).
    PolicyConfig,
    /// The autonomy matrix run with the current autonomy level.
    AutonomyMatrix,
    /// Conservative default because the action was off-model.
    DefaultConservative,
    /// The action was approved by a human.
    HumanApproval,
    /// The engine denies because the system is paused (PAUSE ALL).
    Paused,
}

impl PolicySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DeterministicRule => "deterministic_rule",
            Self::PolicyConfig => "policy_config",
            Self::AutonomyMatrix => "autonomy_matrix",
            Self::DefaultConservative => "default_conservative",
            Self::HumanApproval => "human_approval",
            Self::Paused => "workflow_paused",
        }
    }
}

/// Full evaluation record (§2: action, decision, risk, reasons, source).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyEvaluation {
    pub action: String,
    pub risk: RiskLevel,
    pub decision: PolicyDecision,
    pub reasons: Vec<String>,
    pub policy_source: PolicySource,
}

impl PolicyEvaluation {
    pub fn allow(action: impl Into<String>, risk: RiskLevel, reason: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            risk,
            decision: PolicyDecision::Allow,
            reasons: vec![reason.into()],
            policy_source: PolicySource::AutonomyMatrix,
        }
    }

    pub fn deny(
        action: impl Into<String>,
        risk: RiskLevel,
        reason: impl Into<String>,
        source: PolicySource,
    ) -> Self {
        Self {
            action: action.into(),
            risk,
            decision: PolicyDecision::Deny,
            reasons: vec![reason.into()],
            policy_source: source,
        }
    }

    pub fn require_approval(
        action: impl Into<String>,
        risk: RiskLevel,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            action: action.into(),
            risk,
            decision: PolicyDecision::RequireApproval,
            reasons: vec![reason.into()],
            policy_source: PolicySource::DefaultConservative,
        }
    }
}

// ---------------------------------------------------------------------------
// §3 risk classification
// ---------------------------------------------------------------------------

/// Normalized risk model (§3). Ord: Low < Medium < High < Critical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Critical => "Critical",
        }
    }
}

// ---------------------------------------------------------------------------
// §5 structured process execution
// ---------------------------------------------------------------------------

/// A structured process action: executable + arguments, never a shell
/// string (§5). The engine executes these with no shell interpolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub executable: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    pub fn new(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// The one place a shell string may enter the system — explicitly
    /// marked, raised to High risk, and only ever permitted via approval.
    pub fn from_shell(command: impl Into<String>) -> Self {
        Self {
            executable: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), command.into()],
        }
    }

    pub fn is_shell(&self) -> bool {
        self.executable == "/bin/sh" || self.executable == "sh"
    }
}

// ---------------------------------------------------------------------------
// §6 shell interpolation guard
// ---------------------------------------------------------------------------

/// §6 shell interpolation guard.
///
/// Values originating from agents, planners, file paths, workspace paths
/// or environment values must never be silently concatenated into shell
/// commands. This guard classifies a value as safe *only* when it contains
/// no shell metacharacter in a position that could escape a single literal
/// argument. When unsure, the caller must reject (`unsafe`) — never pass
/// the value through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellInterpolationGuard;

const SHELL_METACHARS: &[char] = &[
    ';', '&', '|', '`', '$', '(', ')', '>', '<', '\n', '\'', '"', '\\',
];

impl ShellInterpolationGuard {
    /// True when `value` contains any shell metacharacter. Conservative:
    /// even characters that are technically safe inside quotes are flagged,
    /// because the guard's job is to stop *silent interpolation*, not to
    /// approve quoted contexts.
    pub fn contains_metachar(value: &str) -> bool {
        value.chars().any(|c| SHELL_METACHARS.contains(&c))
    }

    /// True when the value contains no shell metacharacter, no control
    /// character, and does not start with `-` (a value that could be
    /// mistaken for a flag).
    pub fn is_safe_literal(value: &str) -> bool {
        !value.is_empty()
            && !Self::contains_metachar(value)
            && !value.chars().any(|c| c.is_control())
            && !value.starts_with('-')
    }

    /// Full check for a value that will be passed as one argv element.
    /// NULs are always rejected; newlines make the value unsafe for any
    /// line-based transport; shell metacharacters make the value
    /// *shell-unsafe* but argv-safe (reported distinctly so callers can
    /// still use argv form).
    pub fn check(value: &str) -> ShellValueClass {
        if value.contains('\0') {
            return ShellValueClass::InvalidNul;
        }
        if value.contains('\n') {
            return ShellValueClass::Unsafe;
        }
        if SHELL_METACHARS.iter().any(|c| value.contains(*c)) {
            return ShellValueClass::Unsafe;
        }
        ShellValueClass::SafeLiteral
    }
}

/// Classification of a value destined for a command line (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellValueClass {
    /// Safe to pass as a literal argument in any form.
    SafeLiteral,
    /// Contains shell metacharacters — argv-safe, never shell-interpolated.
    Unsafe,
    /// NUL bytes — rejected outright.
    InvalidNul,
}

impl ShellValueClass {
    pub fn is_safe(self) -> bool {
        matches!(self, Self::SafeLiteral)
    }
}

// ---------------------------------------------------------------------------
// §4 dangerous command protection
// ---------------------------------------------------------------------------

/// Whether a rule's argument needles match bare arguments or anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgMatch {
    /// Needles match dash-arguments by substring, bare arguments exactly
    /// (`rm -rf …`, `git push --force …`).
    DashOrExact,
    /// Needles match anywhere in any argument (credential paths, keychain
    /// lookups — `cat ~/.ssh/id_ed25519`).
    AnyContains,
}

/// How multiple needles of one rule combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSetMatch {
    /// Any single needle matching is enough (`rm -rf` needs only one
    /// destructive flag).
    AnyOf,
    /// Every needle must match, in argument order (`git branch -D` needs
    /// both).
    AllOf,
}

/// One deterministic dangerous-command rule (§4, §3).
#[derive(Debug, Clone, Copy)]
pub struct DangerousRule {
    pub name: &'static str,
    /// Risk applied when the rule matches.
    pub risk: RiskLevel,
    /// A Deny verdict stops execution outright; otherwise the action
    /// enters the approval path.
    pub verdict: RuleVerdict,
    pub executables: &'static [&'static str],
    pub args: &'static [&'static str],
    pub arg_match: ArgMatch,
    pub set_match: ArgSetMatch,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleVerdict {
    Deny,
    DenyUnlessExplicitlyAuthorized,
}

fn arg_matches_needle(m: ArgMatch, a: &str, n: &str) -> bool {
    match m {
        ArgMatch::AnyContains => a.contains(n),
        ArgMatch::DashOrExact => {
            if a.starts_with('-') {
                a.contains(n)
            } else {
                a == n
            }
        }
    }
}

fn args_match(rule: &DangerousRule, args: &[String]) -> bool {
    if rule.args.is_empty() {
        return true;
    }
    match rule.set_match {
        ArgSetMatch::AnyOf => args.iter().any(|a| {
            rule.args
                .iter()
                .any(|n| arg_matches_needle(rule.arg_match, a, n))
        }),
        ArgSetMatch::AllOf => {
            // Every needle must match, in argument order.
            let mut cursor = 0;
            for needle in rule.args {
                let mut found = false;
                while cursor < args.len() {
                    if arg_matches_needle(rule.arg_match, &args[cursor], needle) {
                        found = true;
                        cursor += 1;
                        break;
                    }
                    cursor += 1;
                }
                if !found {
                    return false;
                }
            }
            true
        }
    }
}

/// Deterministic protections against clearly dangerous operations (§4).
/// The planner/LLM is never consulted for these.
pub const DANGEROUS_COMMANDS: &[DangerousRule] = &[
    // -- destructive filesystem -------------------------------------------------
    DangerousRule {
        name: "recursive_delete",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["rm", "/bin/rm", "/usr/bin/rm"],
        args: &[
            "-r",
            "-f",
            "rf",
            "-rf",
            "-fr",
            "--recursive",
            "--force",
            "-R",
            "-rfv",
        ],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "recursive/forced deletion — destructive and irreversible",
    },
    DangerousRule {
        name: "mass_delete",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &[
            "rm",
            "rmdir",
            "/bin/rm",
            "/usr/bin/rm",
            "/bin/rmdir",
            "/usr/bin/rmdir",
        ],
        args: &["*"],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "mass deletion pattern on the current directory tree",
    },
    DangerousRule {
        name: "disk_wipe",
        risk: RiskLevel::Critical,
        verdict: RuleVerdict::Deny,
        executables: &[
            "mkfs",
            "mkfs.ext2",
            "mkfs.ext4",
            "mkfs.btrfs",
            "mkfs.xfs",
            "mkfs.fat",
            "dd",
            "fdisk",
            "diskutil",
            "sfdisk",
        ],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "disk/filesystem operations can destroy data devices",
    },
    // -- privilege & ownership ---------------------------------------------------
    DangerousRule {
        name: "privilege_escalation",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["sudo", "su", "doas", "pkexec"],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "privilege escalation — requires explicit approval",
    },
    DangerousRule {
        name: "ownership_change",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["chown", "chgrp"],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "ownership change is system-level configuration",
    },
    DangerousRule {
        name: "permission_change",
        risk: RiskLevel::Medium,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["chmod"],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "permission change — authorization recommended",
    },
    // -- system configuration ----------------------------------------------------
    DangerousRule {
        name: "system_config",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["sysctl", "launchctl", "systemctl", "defaults", "osascript"],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "system configuration changes affect the whole machine",
    },
    // -- credential access --------------------------------------------------------
    DangerousRule {
        name: "credential_access",
        risk: RiskLevel::Critical,
        verdict: RuleVerdict::Deny,
        executables: &[
            "cat", "less", "more", "head", "tail", "cp", "scp", "rsync", "nano", "vim", "code",
            "open", "type", "sed",
        ],
        args: &[
            "id_rsa",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            "id_ecdsa_sk",
            "id_ed25519_sk",
            ".ssh",
            "authorized_keys",
            ".aws/credentials",
            ".env",
            ".netrc",
        ],
        arg_match: ArgMatch::AnyContains,
        set_match: ArgSetMatch::AnyOf,
        reason: "SSH private key / credential file access",
    },
    DangerousRule {
        name: "keychain_access",
        risk: RiskLevel::Critical,
        verdict: RuleVerdict::Deny,
        executables: &["security", "keychain", "pass", "gopass", "op", "bw"],
        args: &[
            "find-generic-password",
            "find-internet-password",
            "show",
            "dump",
            "export",
            ".password-store",
        ],
        arg_match: ArgMatch::AnyContains,
        set_match: ArgSetMatch::AnyOf,
        reason: "keychain / password-store access",
    },
    // -- destructive git ---------------------------------------------------------
    DangerousRule {
        name: "force_push",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["git"],
        args: &["--force", "-f"],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "force push overwrites shared history",
    },
    DangerousRule {
        name: "hard_reset",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["git"],
        args: &["reset", "--hard"],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AllOf,
        reason: "git reset --hard discards working-tree changes",
    },
    DangerousRule {
        name: "git_clean_fdx",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["git"],
        args: &["clean", "-fdx", "-fd"],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "git clean -fdx deletes untracked files (incl. ignored)",
    },
    DangerousRule {
        name: "force_delete_branch",
        risk: RiskLevel::High,
        verdict: RuleVerdict::DenyUnlessExplicitlyAuthorized,
        executables: &["git"],
        args: &["branch", "-D"],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AllOf,
        reason: "force-deletes a branch with unmerged work",
    },
    // -- secret environments -----------------------------------------------------
    DangerousRule {
        name: "env_dump",
        risk: RiskLevel::Critical,
        verdict: RuleVerdict::Deny,
        executables: &["env", "printenv"],
        args: &[],
        arg_match: ArgMatch::DashOrExact,
        set_match: ArgSetMatch::AnyOf,
        reason: "environment dump can expose injected credentials",
    },
];

/// Deterministic risk classification for a process action (§3, §4).
/// Returns the highest-risk rule that matches, plus the matched rule (if
/// any). This classifier is deliberately *not* a perfect command
/// classifier — it errs on the side of `High`/`Critical` when a rule
/// matches, and unknown executables are handled by the engine's
/// conservative default (approval required).
pub fn classify_process(
    executable: &str,
    args: &[String],
) -> (RiskLevel, Option<&'static DangerousRule>) {
    let base = executable
        .rsplit('/')
        .next()
        .unwrap_or(executable)
        .to_lowercase();
    let mut worst: Option<(RiskLevel, &'static DangerousRule)> = None;
    for rule in DANGEROUS_COMMANDS {
        let exe_matches = rule
            .executables
            .iter()
            .any(|e| e == &base || *e == executable);
        if !exe_matches {
            continue;
        }
        if !args_match(rule, args) {
            continue;
        }
        if worst.map(|(r, _)| rule.risk > r).unwrap_or(true) {
            worst = Some((rule.risk, rule));
        }
    }
    worst
        .map(|(r, rule)| (r, Some(rule)))
        .unwrap_or((RiskLevel::Low, None))
}

/// §4 verdict for a matched rule: deny outright, or deny unless the
/// engine has an explicit authorization for it.
pub fn rule_verdict(rule: &DangerousRule) -> RuleVerdict {
    rule.verdict
}

// ---------------------------------------------------------------------------
// §7–§8 filesystem policy
// ---------------------------------------------------------------------------

/// Filesystem scopes an agent/workflow may touch (§7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FilesystemScope {
    /// Only the project root directory itself.
    ProjectOnly,
    /// Only the task's isolated worktree — the default for autonomous
    /// coding tasks (§7).
    #[default]
    WorktreeOnly,
    /// The workspace directory tree.
    Workspace,
    /// An explicit allowlist of absolute paths.
    CustomPaths(Vec<String>),
    /// No filesystem access.
    NoFilesystem,
}

impl FilesystemScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ProjectOnly => "project_only",
            Self::WorktreeOnly => "worktree_only",
            Self::Workspace => "workspace",
            Self::CustomPaths(_) => "custom_paths",
            Self::NoFilesystem => "no_filesystem",
        }
    }
}

/// Why a path was rejected (§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathViolation {
    /// Outside every allowed root.
    OutsideScope,
    /// Contains a `..` traversal component.
    Traversal,
    /// Canonicalization escapes the allowed root (symlink/mount escape).
    SymlinkEscape,
    /// The path does not exist / cannot be canonicalized.
    NotCanonicalizable,
    /// The scope denies filesystem access outright.
    Denied,
}

impl PathViolation {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::OutsideScope => "path is outside the allowed scope",
            Self::Traversal => "path contains `..` traversal",
            Self::SymlinkEscape => "path resolves outside the allowed scope (symlink/mount escape)",
            Self::NotCanonicalizable => "path cannot be canonicalized",
            Self::Denied => "filesystem access denied by scope",
        }
    }
}

/// Deterministic path validation (§8). Canonicalization alone is not
/// enough: traversal components are rejected before canonicalization, and
/// the canonical result must stay inside the allowed root.
#[derive(Debug, Clone)]
pub struct PathValidator {
    scope: FilesystemScope,
    /// Canonical allowed roots (workspace root + custom paths). For
    /// `WorktreeOnly` this stays empty — only the per-task worktree root
    /// (passed at validation time) is allowed.
    roots: Vec<PathBuf>,
}

impl PathValidator {
    /// Builds a validator for `scope`; `workspace_root` is canonicalized
    /// at build time (a non-canonicalizable root narrows the scope).
    pub fn new(scope: FilesystemScope, workspace_root: &Path) -> Self {
        let ws = workspace_root.canonicalize().unwrap_or_default();
        let mut roots: Vec<PathBuf> = Vec::new();
        match &scope {
            FilesystemScope::NoFilesystem | FilesystemScope::WorktreeOnly => {}
            FilesystemScope::ProjectOnly | FilesystemScope::Workspace => {
                if !ws.as_os_str().is_empty() {
                    roots.push(ws);
                }
            }
            FilesystemScope::CustomPaths(paths) => {
                for p in paths {
                    if let Ok(c) = Path::new(p).canonicalize() {
                        roots.push(c);
                    }
                }
                if !ws.as_os_str().is_empty() {
                    roots.push(ws);
                }
            }
        }
        Self { scope, roots }
    }

    pub fn scope(&self) -> &FilesystemScope {
        &self.scope
    }

    /// Pre-check without touching the disk: rejects traversal components
    /// and scope denials. Canonicalization still happens in
    /// [`Self::validate`].
    pub fn check_components(path: &Path) -> Result<(), PathViolation> {
        for c in path.components() {
            if matches!(c, Component::ParentDir) {
                return Err(PathViolation::Traversal);
            }
        }
        Ok(())
    }

    /// Validates `candidate`. For `WorktreeOnly` scope, `scope_root` is
    /// the task worktree root and is the *only* allowed root; for other
    /// scopes it adds to the configured roots.
    #[allow(clippy::result_large_err)]
    pub fn validate(
        &self,
        candidate: &Path,
        scope_root: Option<&Path>,
    ) -> Result<PathBuf, PathViolation> {
        if matches!(self.scope, FilesystemScope::NoFilesystem) {
            return Err(PathViolation::Denied);
        }
        Self::check_components(candidate)?;
        // Canonicalization: prefer the real path; for not-yet-existing
        // candidates (write targets) resolve the nearest existing
        // ancestor so scope checks still apply.
        let canonical = match candidate.canonicalize() {
            Ok(c) => c,
            Err(_) => {
                let mut abs = if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("/"))
                        .join(candidate)
                };
                while !abs.exists() {
                    if !abs.pop() {
                        break;
                    }
                }
                let ancestor = abs
                    .canonicalize()
                    .map_err(|_| PathViolation::NotCanonicalizable)?;
                // Re-attach the unresolved tail so the caller can still
                // reason about the full path.
                ancestor
            }
        };
        let mut allowed_roots = self.roots.clone();
        if let Some(sr) = scope_root {
            if matches!(self.scope, FilesystemScope::WorktreeOnly) {
                // WorktreeOnly: the worktree root is the ONLY root.
                allowed_roots.clear();
            }
            if let Ok(c) = sr.canonicalize() {
                allowed_roots.push(c);
            }
        }
        if allowed_roots.is_empty() {
            return Err(PathViolation::OutsideScope);
        }
        for root in &allowed_roots {
            if Self::starts_with(&canonical, root) {
                return Ok(canonical);
            }
        }
        Err(PathViolation::SymlinkEscape)
    }

    fn starts_with(candidate: &Path, root: &Path) -> bool {
        candidate.starts_with(root)
            && (candidate == root
                || candidate
                    .strip_prefix(root)
                    .map(|rest| !rest.as_os_str().is_empty())
                    .unwrap_or(false))
    }
}

// ---------------------------------------------------------------------------
// §10 network policy
// ---------------------------------------------------------------------------

/// One allowlisted network target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAllowance {
    pub host: String,
    pub port: Option<u16>,
    pub description: String,
}

/// Network policy modes (§10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NetworkPolicy {
    /// All network access denied.
    #[default]
    Blocked,
    /// All network access allowed (still audited).
    Allowed,
    /// Only the listed hosts/ports.
    Allowlist(Vec<NetworkAllowance>),
    /// Every request asks the user (per-action approval).
    Prompt,
}

impl NetworkPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Allowed => "allowed",
            Self::Allowlist(_) => "allowlist",
            Self::Prompt => "prompt",
        }
    }

    /// Decision for a host+port request. Never Allow on `Prompt`.
    #[allow(clippy::result_large_err)]
    pub fn evaluate(&self, host: &str, port: Option<u16>) -> Result<(), String> {
        match self {
            Self::Blocked => Err(format!("network is blocked by policy (requested {host})")),
            Self::Allowed => Ok(()),
            Self::Allowlist(list) => {
                for a in list {
                    if a.host == host && (a.port.is_none() || a.port == port || port.is_none()) {
                        return Ok(());
                    }
                }
                Err(format!(
                    "host {host} is not on the network allowlist ({} entries)",
                    list.len()
                ))
            }
            Self::Prompt => {
                // Prompt is *allowed to proceed pending approval*: the
                // policy layer above turns this into RequireApproval
                // (never a silent Deny and never a silent Allow).
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §11 secret policy
// ---------------------------------------------------------------------------

/// Secret categories (§11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SecretCategory {
    /// Non-secret regular project data.
    Safe,
    /// Credentials in development configuration (API tokens in local rc
    /// files, cloud CLI config).
    Sensitive,
    /// Private keys, password stores, env files with secrets, browser
    /// credential stores.
    Critical,
}

impl SecretCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Safe => "Safe",
            Self::Sensitive => "Sensitive",
            Self::Critical => "Critical",
        }
    }
}

/// Deterministic classification of a path against secret locations (§11).
/// Paths that cannot be classified become Safe; the *allowance* model is
/// what keeps access conservative.
pub fn classify_secret_path(path: &str) -> SecretCategory {
    let p = path.replace('\\', "/");
    let base = p.rsplit('/').next().unwrap_or(&p).to_lowercase();
    let is_ssh_key = matches!(
        base.as_str(),
        "id_rsa" | "id_dsa" | "id_ecdsa" | "id_ed25519" | "id_ed25519_sk" | "id_ecdsa_sk"
    ) || base.starts_with("id_rsa.")
        || base.starts_with("id_ed25519");
    let looks_env = base.starts_with(".env") || base.ends_with(".env") || base == ".envrc";
    let is_password_store = p.contains("/.password-store/")
        || p.contains("/.gnupg/")
        || p.contains("/.config/pass/")
        || p.contains("/Library/Keychains/")
        || p.contains("/.config/chromium/")
        || p.contains("/.config/google-chrome/")
        || p.contains("/.aws/credentials")
        || p.contains("/.azure/")
        || p.contains("/.config/gcloud/")
        || p.contains("/.kube/")
        || base == "credentials"
        || base == "credentials.json"
        || base == "auth.json";
    let is_sensitive = p.contains("/.ssh/")
        || p.contains("/.config/gh/")
        || p.contains("/.git-credentials")
        || p.contains("/.netrc")
        || base == "npmrc"
        || base == ".npmrc"
        || base == ".pypirc"
        || base.ends_with(".pem")
        || base.ends_with(".key")
        || base.ends_with(".p12")
        || base.ends_with(".pfx");
    if is_ssh_key || is_password_store || looks_env || is_sensitive {
        SecretCategory::Critical
    } else if p.contains("/.ssh/config") || base == "config" && p.contains("/.ssh") {
        SecretCategory::Sensitive
    } else {
        SecretCategory::Safe
    }
}

/// One explicit secret authorization (the only way agents touch secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAllowance {
    /// Scope: workflow id, `*` for all workflows.
    pub workflow_id: String,
    /// Path prefix that is authorized.
    pub path_prefix: String,
    /// Whether the allowance was granted by a human (required for
    /// Critical categories).
    pub human_granted: bool,
    pub granted_at: u64,
}

impl SecretAllowance {
    pub fn new(
        workflow_id: impl Into<String>,
        path_prefix: impl Into<String>,
        human_granted: bool,
    ) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            path_prefix: path_prefix.into(),
            human_granted,
            granted_at: now_ms(),
        }
    }

    pub fn covers(&self, workflow_id: &str, path: &str) -> bool {
        (self.workflow_id == "*" || self.workflow_id == workflow_id)
            && path.starts_with(&self.path_prefix)
    }
}

/// Secret policy (§11): agents never automatically access secrets. Every
/// access must match an explicit allowance; Critical categories always
/// require a human-granted allowance plus approval at the decision layer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretPolicy {
    pub allowances: Vec<SecretAllowance>,
}

impl SecretPolicy {
    #[allow(clippy::result_large_err)]
    pub fn evaluate(
        &self,
        workflow_id: &str,
        path: &str,
        category: SecretCategory,
    ) -> Result<(), String> {
        match category {
            SecretCategory::Safe => Ok(()),
            SecretCategory::Sensitive | SecretCategory::Critical => {
                let allowance = self.allowances.iter().find(|a| a.covers(workflow_id, path));
                match (allowance, category) {
                    (Some(a), SecretCategory::Sensitive) if !a.human_granted => Ok(()),
                    (_, SecretCategory::Critical) => match allowance {
                        Some(a) if a.human_granted => Ok(()),
                        _ => Err(format!(
                            "critical secret access to {path} requires a human-granted allowance for workflow {workflow_id}"
                        )),
                    },
                    (None, _) => Err(format!(
                        "secret access to {path} is not authorized for workflow {workflow_id}"
                    )),
                    _ => Ok(()),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §13 budget policy
// ---------------------------------------------------------------------------

/// Budget dimensions (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BudgetDimension {
    AgentCount,
    Tokens,
    CostCents,
    RuntimeMs,
    ReplanCount,
    CommandCount,
    NetworkRequests,
}

impl BudgetDimension {
    pub fn label(self) -> &'static str {
        match self {
            Self::AgentCount => "agent count",
            Self::Tokens => "token usage",
            Self::CostCents => "estimated cost",
            Self::RuntimeMs => "runtime duration",
            Self::ReplanCount => "replan count",
            Self::CommandCount => "command count",
            Self::NetworkRequests => "network requests",
        }
    }
}

pub const ALL_BUDGET_DIMENSIONS: [BudgetDimension; 7] = [
    BudgetDimension::AgentCount,
    BudgetDimension::Tokens,
    BudgetDimension::CostCents,
    BudgetDimension::RuntimeMs,
    BudgetDimension::ReplanCount,
    BudgetDimension::CommandCount,
    BudgetDimension::NetworkRequests,
];

/// Centralized budget enforcement (§13). Each dimension is an optional
/// cap; `None` = unlimited. The planner can never increase its own
/// budget — increases flow through [`BudgetLedger::authorize_increase`],
/// which requires policy configuration *or* human approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BudgetPolicy {
    pub max_agents: Option<u64>,
    pub max_tokens: Option<u64>,
    pub max_cost_cents: Option<u64>,
    pub max_runtime_ms: Option<u64>,
    pub max_replans: Option<u64>,
    pub max_commands: Option<u64>,
    pub max_network_requests: Option<u64>,
}

impl BudgetPolicy {
    pub fn cap(&self, dim: BudgetDimension) -> Option<u64> {
        match dim {
            BudgetDimension::AgentCount => self.max_agents,
            BudgetDimension::Tokens => self.max_tokens,
            BudgetDimension::CostCents => self.max_cost_cents,
            BudgetDimension::RuntimeMs => self.max_runtime_ms,
            BudgetDimension::ReplanCount => self.max_replans,
            BudgetDimension::CommandCount => self.max_commands,
            BudgetDimension::NetworkRequests => self.max_network_requests,
        }
    }
}

/// Live consumption counters (bounded u64 saturating arithmetic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetCounters {
    pub agents_spawned: u64,
    pub tokens_used: u64,
    pub cost_cents: u64,
    pub runtime_ms: u64,
    pub replans: u64,
    pub commands: u64,
    pub network_requests: u64,
}

impl BudgetCounters {
    pub fn value(&self, dim: BudgetDimension) -> u64 {
        match dim {
            BudgetDimension::AgentCount => self.agents_spawned,
            BudgetDimension::Tokens => self.tokens_used,
            BudgetDimension::CostCents => self.cost_cents,
            BudgetDimension::RuntimeMs => self.runtime_ms,
            BudgetDimension::ReplanCount => self.replans,
            BudgetDimension::CommandCount => self.commands,
            BudgetDimension::NetworkRequests => self.network_requests,
        }
    }
}

/// One recorded budget event (auditable, never a secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetEvent {
    pub dimension: BudgetDimension,
    pub delta: u64,
    pub running: u64,
    pub cap: Option<u64>,
    pub at_ms: u64,
}

/// Budget ledger: counters + recent events (bounded ring). Engines record
/// consumption here and check enforcement before spending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetLedger {
    pub counters: BudgetCounters,
    pub started_at_ms: u64,
    pub events: Vec<BudgetEvent>,
}

impl Default for BudgetLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl BudgetLedger {
    pub fn new() -> Self {
        Self {
            counters: BudgetCounters::default(),
            started_at_ms: now_ms(),
            events: Vec::new(),
        }
    }

    /// Records consumption; the engine calls this before/after spending.
    /// The event ring is bounded at 256 entries.
    pub fn record(&mut self, dim: BudgetDimension, delta: u64) {
        let running = self.counters.value(dim).saturating_add(delta);
        match dim {
            BudgetDimension::AgentCount => self.counters.agents_spawned = running,
            BudgetDimension::Tokens => self.counters.tokens_used = running,
            BudgetDimension::CostCents => self.counters.cost_cents = running,
            BudgetDimension::RuntimeMs => self.counters.runtime_ms = running,
            BudgetDimension::ReplanCount => self.counters.replans = running,
            BudgetDimension::CommandCount => self.counters.commands = running,
            BudgetDimension::NetworkRequests => self.counters.network_requests = running,
        }
        if delta > 0 && self.events.len() < 256 {
            self.events.push(BudgetEvent {
                dimension: dim,
                delta,
                running,
                cap: None,
                at_ms: now_ms(),
            });
        }
    }

    pub fn effective_runtime_ms(&self) -> u64 {
        now_ms().saturating_sub(self.started_at_ms)
    }

    pub fn value(&self, dim: BudgetDimension) -> u64 {
        if dim == BudgetDimension::RuntimeMs {
            self.effective_runtime_ms()
        } else {
            self.counters.value(dim)
        }
    }

    /// Dimensions already at/over their cap, with (dim, value, cap).
    pub fn check(&self, policy: &BudgetPolicy) -> Vec<(BudgetDimension, u64, u64)> {
        let mut over = Vec::new();
        for dim in ALL_BUDGET_DIMENSIONS {
            if let Some(cap) = policy.cap(dim) {
                let value = self.value(dim);
                if value >= cap {
                    over.push((dim, value, cap));
                }
            }
        }
        over
    }

    pub fn can_afford(&self, dim: BudgetDimension, delta: u64, policy: &BudgetPolicy) -> bool {
        match policy.cap(dim) {
            Some(cap) => self.value(dim).saturating_add(delta) <= cap,
            None => true,
        }
    }

    /// §13: the planner cannot increase its own budget. An increase is a
    /// *policy change* and must be explicitly authorized (human approval
    /// recorded by the engine) — this method only applies an increase once
    /// `authorized == true`.
    pub fn authorize_increase(
        &mut self,
        policy: &mut BudgetPolicy,
        dim: BudgetDimension,
        new_cap: u64,
        authorized: bool,
    ) -> Result<(), String> {
        if !authorized {
            return Err(format!(
                "budget increase on {} requires authorization — the planner cannot raise its own budget",
                dim.label()
            ));
        }
        let slot = match dim {
            BudgetDimension::AgentCount => &mut policy.max_agents,
            BudgetDimension::Tokens => &mut policy.max_tokens,
            BudgetDimension::CostCents => &mut policy.max_cost_cents,
            BudgetDimension::RuntimeMs => &mut policy.max_runtime_ms,
            BudgetDimension::ReplanCount => &mut policy.max_replans,
            BudgetDimension::CommandCount => &mut policy.max_commands,
            BudgetDimension::NetworkRequests => &mut policy.max_network_requests,
        };
        *slot = Some(new_cap);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// §14 autonomy levels
// ---------------------------------------------------------------------------

/// Explicit autonomy modes (§14). Do not equate Autonomous with
/// unrestricted: every level still requires approval for Critical risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyLevel {
    /// Every risky action requires approval.
    #[default]
    Manual,
    /// Low-risk actions automatic.
    Assisted,
    /// Medium-risk actions automatic inside policy scope.
    Supervised,
    /// High automation within a strict sandbox and budget. Critical
    /// actions still require explicit user approval.
    Autonomous,
}

impl AutonomyLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Assisted => "Assisted",
            Self::Supervised => "Supervised",
            Self::Autonomous => "Autonomous",
        }
    }

    /// The maximum risk that may execute automatically at this level
    /// (Critical is never automatic at any level).
    pub fn auto_threshold(self) -> RiskLevel {
        match self {
            Self::Manual => RiskLevel::Low,
            Self::Assisted => RiskLevel::Low,
            Self::Supervised => RiskLevel::Medium,
            Self::Autonomous => RiskLevel::High,
        }
    }
}

/// §14: what each autonomy level allows, mapped to one word per level.
pub fn autonomy_description(level: AutonomyLevel) -> &'static str {
    match level {
        AutonomyLevel::Manual => "every risky action requires approval",
        AutonomyLevel::Assisted => "low-risk actions run automatically",
        AutonomyLevel::Supervised => "medium-risk actions run automatically inside policy scope",
        AutonomyLevel::Autonomous => {
            "high automation inside a strict sandbox and budget; critical actions always require approval"
        }
    }
}

/// The autonomy matrix: decision(level, risk, in_policy_scope).
/// - above the auto threshold → RequireApproval
/// - Critical → RequireApproval at every level
/// - out-of-scope → RequireApproval even when Low
pub fn autonomy_decision(level: AutonomyLevel, risk: RiskLevel, in_scope: bool) -> PolicyDecision {
    if !in_scope {
        return PolicyDecision::RequireApproval;
    }
    if risk == RiskLevel::Critical {
        return PolicyDecision::RequireApproval;
    }
    if risk > level.auto_threshold() {
        return PolicyDecision::RequireApproval;
    }
    PolicyDecision::Allow
}

// ---------------------------------------------------------------------------
// evaluation context
// ---------------------------------------------------------------------------

/// Everything the engine needs to evaluate one action.
#[derive(Debug, Clone, Default)]
pub struct PolicyContext {
    /// Owning workflow (scoped approvals + secret allowances).
    pub workflow_id: String,
    /// Owning task (approval binding).
    pub task_id: Option<String>,
    /// Owning agent (approval binding).
    pub agent_id: Option<String>,
    /// Worktree root for WorktreeOnly scope validation.
    pub worktree_root: Option<PathBuf>,
    /// Workspace project root (default scope root).
    pub project_root: Option<PathBuf>,
}

impl PolicyContext {
    pub fn new(workflow_id: impl Into<String>) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// §15–§16 approvals
// ---------------------------------------------------------------------------

/// Stable approval id.
pub type ApprovalId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApprovalStatus {
    Pending,
    Granted,
    Rejected,
    Consumed,
    Expired,
    Invalidated,
}

impl ApprovalStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Rejected => "rejected",
            Self::Consumed => "consumed",
            Self::Expired => "expired",
            Self::Invalidated => "invalidated",
        }
    }
}

/// An approval (§15): workflow + task + agent + action bound, with a
/// policy-reasons record, creation/expiry timestamps and a stable id.
/// Default TTL is 10 minutes; a request may override.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub workflow_id: String,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    /// Canonical fingerprint of the approved action (change detection §16).
    pub action_hash: String,
    /// Human-readable description of the approved action.
    pub action: String,
    pub risk: RiskLevel,
    pub policy_reasons: Vec<String>,
    pub created_at: u64,
    pub expires_at: u64,
    pub status: ApprovalStatus,
    /// Who approved (user handle), set on grant.
    pub granted_by: Option<String>,
    pub granted_at: Option<u64>,
}

impl Approval {
    pub const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;

    pub fn is_pending(&self) -> bool {
        self.status == ApprovalStatus::Pending
    }
}

/// Why an approval was not honored (§15–§16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    Unknown,
    WrongWorkflow,
    WrongAgent,
    Expired,
    Replay,
    ActionMismatch,
    AlreadyDecided(ApprovalStatus),
    NotPending,
}

impl ApprovalError {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unknown => "unknown approval id",
            Self::WrongWorkflow => "approval belongs to a different workflow",
            Self::WrongAgent => "approval belongs to a different agent",
            Self::Expired => "approval has expired",
            Self::Replay => "approval replay rejected — already consumed",
            Self::ActionMismatch => {
                "the action changed after approval — the new action must be approved again"
            }
            Self::AlreadyDecided(s) => s.label(),
            Self::NotPending => "approval is not pending (must be granted first)",
        }
    }
}

/// Approval store with integrity guarantees (§15–§16):
/// - no stale reuse (expiry)
/// - no wrong-workflow / wrong-agent reuse
/// - no approval replay (consume on first use)
/// - action changes invalidate the old approval (hash binding)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApprovalStore {
    approvals: Vec<Approval>,
    /// Hard bound on retained records; pending approvals are never pruned.
    max_records: usize,
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self {
            approvals: Vec::new(),
            max_records: 512,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &mut self,
        workflow_id: impl Into<String>,
        task_id: Option<String>,
        agent_id: Option<String>,
        action: &str,
        action_hash: String,
        risk: RiskLevel,
        policy_reasons: Vec<String>,
        ttl_ms: u64,
    ) -> ApprovalId {
        let now = now_ms();
        let id = format!("approval:{}", uuid::Uuid::new_v4());
        self.approvals.push(Approval {
            id: id.clone(),
            workflow_id: workflow_id.into(),
            task_id,
            agent_id,
            action_hash,
            action: action.to_string(),
            risk,
            policy_reasons,
            created_at: now,
            expires_at: now.saturating_add(ttl_ms),
            status: ApprovalStatus::Pending,
            granted_by: None,
            granted_at: None,
        });
        self.prune(now);
        id
    }

    fn prune(&mut self, now: u64) {
        // Expired *pending* approvals are dead weight.
        self.approvals
            .retain(|a| !(a.status == ApprovalStatus::Pending && now > a.expires_at));
        // Bounded memory: drop oldest decided records beyond the cap,
        // never pending or granted-this-session records.
        if self.approvals.len() > self.max_records {
            let excess = self.approvals.len() - self.max_records;
            let mut removed = 0;
            self.approvals.retain(|a| {
                if removed >= excess
                    || matches!(a.status, ApprovalStatus::Pending | ApprovalStatus::Granted)
                {
                    return true;
                }
                removed += 1;
                false
            });
        }
    }

    pub fn get(&self, id: &str) -> Option<&Approval> {
        self.approvals.iter().find(|a| a.id == id)
    }

    pub fn pending(&self, workflow_id: &str) -> Vec<&Approval> {
        self.approvals
            .iter()
            .filter(|a| a.workflow_id == workflow_id && a.status == ApprovalStatus::Pending)
            .collect()
    }

    pub fn all(&self) -> &[Approval] {
        &self.approvals
    }

    pub fn grant(&mut self, id: &str, actor: &str) -> Result<(), ApprovalError> {
        let now = now_ms();
        let a = self
            .approvals
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(ApprovalError::Unknown)?;
        match a.status {
            ApprovalStatus::Pending => {}
            other => return Err(ApprovalError::AlreadyDecided(other)),
        }
        if now > a.expires_at {
            a.status = ApprovalStatus::Expired;
            return Err(ApprovalError::Expired);
        }
        a.status = ApprovalStatus::Granted;
        a.granted_by = Some(actor.to_string());
        a.granted_at = Some(now);
        Ok(())
    }

    pub fn reject(&mut self, id: &str) -> Result<(), ApprovalError> {
        let a = self
            .approvals
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(ApprovalError::Unknown)?;
        if a.status != ApprovalStatus::Pending {
            return Err(ApprovalError::AlreadyDecided(a.status));
        }
        a.status = ApprovalStatus::Rejected;
        Ok(())
    }

    /// The single honor path: verifies identity, expiry, hash and
    /// replay-protection, then consumes.
    pub fn honor(
        &mut self,
        id: &str,
        workflow_id: &str,
        agent_id: Option<&str>,
        action_hash: &str,
    ) -> Result<(), ApprovalError> {
        let now = now_ms();
        let a = self
            .approvals
            .iter_mut()
            .find(|a| a.id == id)
            .ok_or(ApprovalError::Unknown)?;
        if a.workflow_id != workflow_id {
            return Err(ApprovalError::WrongWorkflow);
        }
        if let Some(agent) = agent_id {
            if let Some(expected) = a.agent_id.as_deref() {
                if expected != agent {
                    return Err(ApprovalError::WrongAgent);
                }
            }
        }
        if now > a.expires_at {
            a.status = ApprovalStatus::Expired;
            return Err(ApprovalError::Expired);
        }
        match a.status {
            ApprovalStatus::Granted => {}
            ApprovalStatus::Pending => return Err(ApprovalError::NotPending),
            ApprovalStatus::Rejected => {
                return Err(ApprovalError::AlreadyDecided(ApprovalStatus::Rejected))
            }
            ApprovalStatus::Consumed => return Err(ApprovalError::Replay),
            ApprovalStatus::Expired => return Err(ApprovalError::Expired),
            ApprovalStatus::Invalidated => {
                return Err(ApprovalError::AlreadyDecided(ApprovalStatus::Invalidated))
            }
        }
        if a.action_hash != action_hash {
            // §16: the action materially changed after approval.
            a.status = ApprovalStatus::Invalidated;
            return Err(ApprovalError::ActionMismatch);
        }
        a.status = ApprovalStatus::Consumed;
        Ok(())
    }

    pub fn pending_count(&self) -> usize {
        self.approvals
            .iter()
            .filter(|a| a.status == ApprovalStatus::Pending)
            .count()
    }
}

/// Canonical action fingerprint for approvals (§16). Any material change
/// to the action (command, args, path, target) changes the hash and
/// invalidates the old approval.
pub fn action_hash(action: &str, extra: &[&str]) -> String {
    let mut s: u64 = 0xcbf29ce484222325;
    for part in std::iter::once(action).chain(extra.iter().copied()) {
        for b in part.as_bytes() {
            s ^= u64::from(*b);
            s = s.wrapping_mul(0x100000001b3);
        }
    }
    format!("{s:016x}")
}

// ---------------------------------------------------------------------------
// §1–§2 the engine
// ---------------------------------------------------------------------------

/// The processed-action view handed to the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Structured process execution (§5).
    Process(CommandSpec),
    /// Unstructured shell string — treated with maximum conservatism.
    Shell(String),
    /// Filesystem operation.
    Filesystem {
        path: String,
        operation: FsOperation,
    },
    /// Outbound network request.
    Network { host: String, port: Option<u16> },
    /// Secret material access.
    Secret { path: String },
    /// Agent process spawn.
    AgentSpawn { definition_id: String, cwd: String },
    /// Budget increase on a dimension.
    Budget {
        dimension: BudgetDimension,
        new_cap: u64,
    },
    /// Autonomy level change.
    AutonomyChange { requested: AutonomyLevel },
    /// Workspace-wide control operations (pause/stop/revert) — always
    /// human-facing, never agent-initiated.
    WorkspaceControl { operation: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsOperation {
    Read,
    Write,
    Delete,
    RecursiveDelete,
}

/// The central policy engine. Owned by the workspace engine; evaluated
/// per action with a workflow context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyEngine {
    pub autonomy: AutonomyLevel,
    pub filesystem: FilesystemScope,
    pub network: NetworkPolicy,
    pub secrets: SecretPolicy,
    pub budget_policy: BudgetPolicy,
    pub budget: BudgetLedger,
    pub approvals: ApprovalStore,
    /// Explicit process allowlist (executable basenames). Entered by the
    /// user, never by the planner.
    pub process_allowlist: Vec<String>,
    pub version: u32,
}

impl Default for PolicyEngine {
    fn default() -> Self {
        Self {
            autonomy: AutonomyLevel::Supervised,
            filesystem: FilesystemScope::default(),
            network: NetworkPolicy::Blocked,
            secrets: SecretPolicy::default(),
            budget_policy: BudgetPolicy::default(),
            budget: BudgetLedger::new(),
            approvals: ApprovalStore::new(),
            process_allowlist: Vec::new(),
            version: 1,
        }
    }
}

impl PolicyEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates one action (§2). High-level flow:
    /// 1. deterministic dangerous-command rules (Process/Shell) → Deny or
    ///    approval-gated override,
    /// 2. category gates (filesystem scope, network, secrets) → Deny or
    ///    scope waiver, and the autonomy matrix with the current level,
    /// 3. conservative default: unknown executables require approval.
    pub fn evaluate(&self, action: &Action, ctx: &PolicyContext) -> PolicyEvaluation {
        match action {
            Action::Process(spec) => self.evaluate_process(spec, ctx),
            Action::Shell(cmd) => self.evaluate_shell(cmd, ctx),
            Action::Filesystem { path, operation } => {
                self.evaluate_filesystem(path, *operation, ctx)
            }
            Action::Network { host, port } => self.evaluate_network(host, *port),
            Action::Secret { path } => self.evaluate_secret(path, ctx),
            Action::AgentSpawn { definition_id, cwd } => {
                self.evaluate_agent_spawn(definition_id, cwd, ctx)
            }
            Action::Budget { dimension, new_cap } => PolicyEvaluation {
                action: format!("budget increase: {} → {new_cap}", dimension.label()),
                risk: RiskLevel::High,
                decision: PolicyDecision::RequireApproval,
                reasons: vec![
                    "a planner cannot increase its own budget".into(),
                    "budget increases require policy and/or human approval".into(),
                ],
                policy_source: PolicySource::PolicyConfig,
            },
            Action::AutonomyChange { requested } => {
                let _ = requested;
                PolicyEvaluation {
                    action: "autonomy level change".into(),
                    risk: RiskLevel::High,
                    decision: PolicyDecision::RequireApproval,
                    reasons: vec![
                        "autonomy changes are human-only decisions".into(),
                        "agents never change their own autonomy level".into(),
                    ],
                    policy_source: PolicySource::PolicyConfig,
                }
            }
            Action::WorkspaceControl { operation } => PolicyEvaluation {
                action: format!("workspace control: {operation}"),
                risk: RiskLevel::Low,
                decision: PolicyDecision::Allow,
                reasons: vec!["workspace controls are user-issued only".into()],
                policy_source: PolicySource::PolicyConfig,
            },
        }
    }

    fn evaluate_process(&self, spec: &CommandSpec, ctx: &PolicyContext) -> PolicyEvaluation {
        let action_desc = if spec.args.is_empty() {
            spec.executable.clone()
        } else {
            format!("{} {}", spec.executable, spec.args.join(" "))
        };
        let in_scope = self.filesystem_in_scope(ctx);
        // 1. deterministic dangerous-command rules take precedence.
        let (rule_risk, rule) = classify_process(&spec.executable, &spec.args);
        if let Some(rule) = rule {
            let source = PolicySource::DeterministicRule;
            return PolicyEvaluation {
                action: action_desc,
                risk: rule_risk,
                decision: match rule.verdict {
                    RuleVerdict::Deny => PolicyDecision::Deny,
                    RuleVerdict::DenyUnlessExplicitlyAuthorized => PolicyDecision::RequireApproval,
                },
                reasons: vec![rule.reason.to_string()],
                policy_source: source,
            };
        }
        // 2. explicit process allowlist (user-configured, not planner).
        let base = spec
            .executable
            .rsplit('/')
            .next()
            .map(|b| self.process_allowlist.contains(&b.to_string()))
            .unwrap_or(false);
        if base {
            return PolicyEvaluation {
                action: action_desc,
                risk: RiskLevel::Low,
                decision: PolicyDecision::Allow,
                reasons: vec![format!(
                    "{} is on the explicit process allowlist",
                    spec.executable
                )],
                policy_source: PolicySource::PolicyConfig,
            };
        }
        // 3. known-command risk map.
        let risk = base_risk_for(&spec.executable, &spec.args);
        let unknown = risk == RiskLevel::Medium && is_unknown_executable(&spec.executable);
        if unknown {
            // When uncertain: require approval.
            return PolicyEvaluation {
                action: action_desc,
                risk,
                decision: PolicyDecision::RequireApproval,
                reasons: vec![
                    format!(
                        "unknown executable `{}` — conservative default",
                        spec.executable
                    ),
                    "when uncertain, require approval".into(),
                ],
                policy_source: PolicySource::DefaultConservative,
            };
        }
        PolicyEvaluation {
            action: action_desc,
            risk,
            decision: autonomy_decision(self.autonomy, risk, in_scope),
            reasons: vec![
                format!(
                    "autonomy level {} allows {} risk automatically",
                    self.autonomy.label(),
                    risk.label()
                ),
                format!("risk classification: {risk:?}"),
            ],
            policy_source: PolicySource::AutonomyMatrix,
        }
    }

    fn evaluate_shell(&self, cmd: &str, ctx: &PolicyContext) -> PolicyEvaluation {
        let in_scope = self.filesystem_in_scope(ctx);
        let words: Vec<&str> = cmd.split_whitespace().collect();
        let trivial = words.len() == 1 && ShellInterpolationGuard::is_safe_literal(words[0]);
        let risk = if cmd.contains("rm ") || cmd.contains("sudo") || cmd.contains("dd ") {
            RiskLevel::High
        } else {
            RiskLevel::Medium
        };
        let decision = if trivial && in_scope {
            PolicyDecision::Allow
        } else {
            PolicyDecision::RequireApproval
        };
        PolicyEvaluation {
            action: cmd.to_string(),
            risk,
            decision,
            reasons: vec![
                "unstructured shell strings are evaluated conservatively".into(),
                "prefer structured executable + arguments execution".into(),
            ],
            policy_source: PolicySource::DefaultConservative,
        }
    }

    fn evaluate_filesystem(
        &self,
        path: &str,
        operation: FsOperation,
        ctx: &PolicyContext,
    ) -> PolicyEvaluation {
        let root = ctx
            .project_root
            .as_deref()
            .map(Path::new)
            .unwrap_or_else(|| Path::new("/"));
        let validator = PathValidator::new(self.filesystem.clone(), root);
        let scope_root = ctx.worktree_root.clone();
        let action_desc = format!("{operation:?} {path}");
        match validator.validate(Path::new(path), scope_root.as_deref()) {
            Err(violation) => PolicyEvaluation {
                action: action_desc,
                risk: match operation {
                    FsOperation::Read => RiskLevel::Medium,
                    _ => RiskLevel::High,
                },
                decision: PolicyDecision::Deny,
                reasons: vec![violation.reason().to_string()],
                policy_source: PolicySource::DeterministicRule,
            },
            Ok(_) => {
                let risk = match operation {
                    FsOperation::Read | FsOperation::Write => RiskLevel::Low,
                    FsOperation::Delete => RiskLevel::High,
                    FsOperation::RecursiveDelete => RiskLevel::Critical,
                };
                PolicyEvaluation {
                    action: action_desc,
                    risk,
                    decision: autonomy_decision(self.autonomy, risk, true),
                    reasons: vec![
                        format!("path validated within scope {}", self.filesystem.as_str()),
                        format!("operation risk: {risk:?}"),
                    ],
                    policy_source: PolicySource::AutonomyMatrix,
                }
            }
        }
    }

    fn evaluate_network(&self, host: &str, port: Option<u16>) -> PolicyEvaluation {
        let action_desc = match port {
            Some(p) => format!("network request to {host}:{p}"),
            None => format!("network request to {host}"),
        };
        let decision = match self.network.evaluate(host, port) {
            Ok(()) => {
                // Allowed by network policy configuration, but still
                // governed by autonomy: Manual/Assisted levels need
                // approval for network traffic.
                let auto_ok = matches!(
                    self.network,
                    NetworkPolicy::Allowed | NetworkPolicy::Allowlist(_)
                );
                if !auto_ok {
                    PolicyDecision::RequireApproval
                } else {
                    autonomy_decision(self.autonomy, RiskLevel::Medium, true)
                }
            }
            Err(_) => PolicyDecision::Deny,
        };
        PolicyEvaluation {
            action: action_desc,
            risk: RiskLevel::Medium,
            decision,
            reasons: vec![format!("network policy: {}", self.network.as_str())],
            policy_source: PolicySource::PolicyConfig,
        }
    }

    fn evaluate_secret(&self, path: &str, ctx: &PolicyContext) -> PolicyEvaluation {
        let category = classify_secret_path(path);
        let action_desc = format!("secret access: {path}");
        let (decision, reasons) = match self.secrets.evaluate(&ctx.workflow_id, path, category) {
            Ok(()) => {
                let decision = if category == SecretCategory::Critical {
                    PolicyDecision::RequireApproval
                } else {
                    autonomy_decision(self.autonomy, RiskLevel::Medium, true)
                };
                (
                    decision,
                    vec![
                        "explicit allowance matched".into(),
                        "critical secrets require explicit approval".into(),
                    ],
                )
            }
            Err(reason) => (PolicyDecision::Deny, vec![reason]),
        };
        PolicyEvaluation {
            action: action_desc,
            risk: match category {
                SecretCategory::Safe => RiskLevel::Low,
                SecretCategory::Sensitive => RiskLevel::Medium,
                SecretCategory::Critical => RiskLevel::Critical,
            },
            decision,
            reasons,
            policy_source: PolicySource::DeterministicRule,
        }
    }

    fn evaluate_agent_spawn(
        &self,
        definition_id: &str,
        cwd: &str,
        ctx: &PolicyContext,
    ) -> PolicyEvaluation {
        let action_desc = format!("agent spawn: {definition_id} in {cwd}");
        let agent_budget_ok =
            self.budget
                .can_afford(BudgetDimension::AgentCount, 1, &self.budget_policy);
        let in_scope = self.filesystem_in_scope(ctx);
        if !agent_budget_ok {
            return PolicyEvaluation {
                action: action_desc,
                risk: RiskLevel::Medium,
                decision: PolicyDecision::Deny,
                reasons: vec!["agent count budget exceeded".into()],
                policy_source: PolicySource::PolicyConfig,
            };
        }
        PolicyEvaluation {
            action: action_desc,
            risk: RiskLevel::Low,
            decision: autonomy_decision(self.autonomy, RiskLevel::Medium, in_scope),
            reasons: vec![
                format!("agent spawn in scope {}", self.filesystem.as_str()),
                "agent count budget checked".into(),
            ],
            policy_source: PolicySource::AutonomyMatrix,
        }
    }

    /// True when the context has enough scope info for [`Self::evaluate`]
    /// to consider filesystem access in-scope.
    fn filesystem_in_scope(&self, ctx: &PolicyContext) -> bool {
        match self.filesystem {
            FilesystemScope::NoFilesystem => false,
            FilesystemScope::WorktreeOnly => {
                ctx.worktree_root.is_some() || ctx.project_root.is_some()
            }
            FilesystemScope::ProjectOnly | FilesystemScope::Workspace => ctx.project_root.is_some(),
            FilesystemScope::CustomPaths(_) => true,
        }
    }

    /// Requests an approval for a RequireApproval evaluation. Returns the
    /// stable approval id.
    pub fn request_approval(
        &mut self,
        evaluation: &PolicyEvaluation,
        ctx: &PolicyContext,
        action_hash: &str,
        ttl_ms: u64,
    ) -> ApprovalId {
        // §12/§40: never persist raw secret values — the action description
        // and reasons are defensively redacted before they enter the store
        // (which survives restarts).
        let action = crate::redact::Redactor::redact(&evaluation.action);
        let reasons: Vec<String> = evaluation
            .reasons
            .iter()
            .map(|r| crate::redact::Redactor::redact(r))
            .collect();
        self.approvals.request(
            &ctx.workflow_id,
            ctx.task_id.clone(),
            ctx.agent_id.clone(),
            &action,
            action_hash.to_string(),
            evaluation.risk,
            reasons,
            ttl_ms,
        )
    }

    pub fn record_budget(&mut self, dim: BudgetDimension, delta: u64) {
        self.budget.record(dim, delta);
    }

    pub fn budget_exceeded(&self) -> Vec<(BudgetDimension, u64, u64)> {
        self.budget.check(&self.budget_policy)
    }
}

/// Base risk for well-known executables (spec §3 examples: `ls` Low,
/// `npm install` Medium, network clients Medium). Unknown executables
/// return Medium — the engine turns those into RequireApproval.
fn base_risk_for(executable: &str, args: &[String]) -> RiskLevel {
    let base = executable
        .rsplit('/')
        .next()
        .unwrap_or(executable)
        .to_lowercase();
    match base.as_str() {
        "ls" | "cat" | "pwd" | "echo" | "grep" | "find" | "head" | "tail" | "sed" | "awk"
        | "diff" | "wc" | "sort" | "uniq" | "cut" | "tr" | "basename" | "dirname" | "test"
        | "true" | "false" | "which" | "printf" | "env" => RiskLevel::Low,
        "cargo" => {
            let first = args.first().map(String::as_str).unwrap_or("");
            if matches!(
                first,
                "check" | "test" | "clippy" | "build" | "doc" | "fmt" | "metadata"
            ) {
                RiskLevel::Low
            } else {
                RiskLevel::Medium
            }
        }
        "git" => {
            let first = args.first().map(String::as_str).unwrap_or("");
            if matches!(
                first,
                "status"
                    | "diff"
                    | "log"
                    | "branch"
                    | "rev-parse"
                    | "show"
                    | "blame"
                    | "ls-files"
                    | "ls-tree"
                    | "merge-base"
                    | "describe"
            ) {
                RiskLevel::Low
            } else {
                RiskLevel::Medium
            }
        }
        _ => RiskLevel::Medium,
    }
}

/// True when the executable is not in any known-safe or known list — the
/// engine treats it as "uncertain → require approval".
fn is_unknown_executable(executable: &str) -> bool {
    let base = executable
        .rsplit('/')
        .next()
        .unwrap_or(executable)
        .to_lowercase();
    const KNOWN: &[&str] = &[
        "ls",
        "cat",
        "pwd",
        "echo",
        "grep",
        "find",
        "head",
        "tail",
        "sed",
        "awk",
        "diff",
        "wc",
        "sort",
        "uniq",
        "cut",
        "tr",
        "basename",
        "dirname",
        "test",
        "true",
        "false",
        "which",
        "printf",
        "env",
        "cargo",
        "git",
        "npm",
        "npx",
        "pip",
        "pip3",
        "pipx",
        "curl",
        "wget",
        "yarn",
        "pnpm",
        "bun",
        "go",
        "rustup",
        "brew",
        "apt",
        "apt-get",
        "node",
        "python",
        "python3",
        "make",
        "cmake",
        "gh",
        "docker",
        "cp",
        "mv",
        "mkdir",
        "touch",
        "file",
        "ranlib",
        "ar",
        "ld",
        "cc",
        "gcc",
        "clang",
        "swift",
        "ruby",
        "perl",
        "ruby",
        "sh",
        "bash",
        "zsh",
        "open",
        "xcodebuild",
        "codesign",
        "carthage",
        "bundle",
        "ruby",
    ];
    !KNOWN.contains(&base.as_str())
}

/// The persisted slice of policy state (never contains credentials).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedPolicyState {
    pub version: u32,
    pub autonomy: AutonomyLevel,
    pub filesystem: FilesystemScope,
    pub network: NetworkPolicy,
    pub secrets: SecretPolicy,
    pub budget_policy: BudgetPolicy,
    pub budget: BudgetLedger,
    /// Pending/decided approvals survive restarts (§25): the store is
    /// serializable and its integrity checks (expiry, hash binding,
    /// consume-on-use) are re-verified at honor time — persisted approvals
    /// are not trusted blindly. Never contains credentials.
    #[serde(default)]
    pub approvals: ApprovalStore,
    /// User-entered process allowlist (never planner-entered).
    #[serde(default)]
    pub process_allowlist: Vec<String>,
}

impl From<&PolicyEngine> for PersistedPolicyState {
    fn from(e: &PolicyEngine) -> Self {
        Self {
            version: e.version,
            autonomy: e.autonomy,
            filesystem: e.filesystem.clone(),
            network: e.network.clone(),
            secrets: e.secrets.clone(),
            budget_policy: e.budget_policy.clone(),
            budget: e.budget.clone(),
            approvals: e.approvals.clone(),
            process_allowlist: e.process_allowlist.clone(),
        }
    }
}

impl From<PersistedPolicyState> for PolicyEngine {
    fn from(p: PersistedPolicyState) -> Self {
        Self {
            version: p.version,
            autonomy: p.autonomy,
            filesystem: p.filesystem,
            network: p.network,
            secrets: p.secrets,
            budget_policy: p.budget_policy,
            budget: p.budget,
            approvals: p.approvals,
            process_allowlist: p.process_allowlist,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(exe: &str, args: &[&str]) -> CommandSpec {
        let mut s = CommandSpec::new(exe);
        for a in args {
            s = s.with_arg(*a);
        }
        s
    }

    // -- §3 risk classification ------------------------------------------------

    #[test]
    fn risk_levels_are_ordered() {
        assert!(RiskLevel::Low < RiskLevel::Medium);
        assert!(RiskLevel::Medium < RiskLevel::High);
        assert!(RiskLevel::High < RiskLevel::Critical);
    }

    // -- §4 dangerous commands -------------------------------------------------

    #[test]
    fn ls_is_low_risk() {
        let (risk, rule) = classify_process("ls", &[]);
        assert_eq!(risk, RiskLevel::Low);
        assert!(rule.is_none());
    }

    #[test]
    fn rm_rf_is_high_risk_and_needs_authorization() {
        for args in [
            vec!["-rf", "/tmp/project"],
            vec!["-fr", "/tmp/project"],
            vec!["-r", "-f", "."],
        ] {
            let (risk, rule) = classify_process(
                "rm",
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            assert_eq!(risk, RiskLevel::High, "{args:?}");
            let rule = rule.expect("recursive delete rule");
            assert_eq!(rule.name, "recursive_delete");
            assert_eq!(
                rule_verdict(rule),
                RuleVerdict::DenyUnlessExplicitlyAuthorized
            );
        }
    }

    #[test]
    fn plain_rm_single_file_is_not_flagged() {
        let (risk, rule) = classify_process("rm", &["file.tmp".to_string()]);
        assert_eq!(risk, RiskLevel::Low);
        assert!(rule.is_none());
    }

    #[test]
    fn mkfs_and_dd_are_critical_and_denied() {
        for exe in ["mkfs", "mkfs.ext4", "dd", "fdisk"] {
            let (risk, rule) = classify_process(exe, &[]);
            assert_eq!(risk, RiskLevel::Critical, "{exe} risk");
            assert_eq!(
                rule_verdict(rule.unwrap()),
                RuleVerdict::Deny,
                "{exe} verdict"
            );
        }
    }

    #[test]
    fn sudo_is_high_and_approval_gated() {
        let (risk, rule) = classify_process(
            "sudo",
            &["rm".to_string(), "-rf".to_string(), "/".to_string()],
        );
        assert_eq!(risk, RiskLevel::High);
        assert_eq!(
            rule_verdict(rule.unwrap()),
            RuleVerdict::DenyUnlessExplicitlyAuthorized
        );
    }

    #[test]
    fn chmod_chown_gated() {
        let (risk, rule) = classify_process("chmod", &["+x".into(), "script.sh".into()]);
        assert_eq!(risk, RiskLevel::Medium);
        assert!(rule.is_some());
        let (risk2, _) = classify_process("chown", &["root:root".into(), "f".into()]);
        assert_eq!(risk2, RiskLevel::High);
    }

    #[test]
    fn ssh_key_reading_is_critical_deny() {
        let (risk, rule) = classify_process("cat", &["~/.ssh/id_ed25519".into()]);
        assert_eq!(risk, RiskLevel::Critical);
        assert_eq!(rule_verdict(rule.unwrap()), RuleVerdict::Deny);
        let (risk2, _) = classify_process("cat", &["Cargo.toml".into()]);
        assert_eq!(risk2, RiskLevel::Low);
    }

    #[test]
    fn env_file_read_is_critical_deny() {
        let (risk, rule) = classify_process("head", &["-n".into(), "5".into(), ".env".into()]);
        assert_eq!(risk, RiskLevel::Critical);
        assert_eq!(rule_verdict(rule.unwrap()), RuleVerdict::Deny);
    }

    #[test]
    fn git_force_push_is_approval_gated() {
        let (risk, rule) = classify_process(
            "git",
            &[
                "push".to_string(),
                "--force".to_string(),
                "origin".to_string(),
                "main".to_string(),
            ],
        );
        assert_eq!(risk, RiskLevel::High);
        assert_eq!(rule.unwrap().name, "force_push");
        // Plain push is not flagged.
        let (risk2, rule2) = classify_process(
            "git",
            &["push".to_string(), "origin".to_string(), "main".to_string()],
        );
        assert_eq!(risk2, RiskLevel::Low);
        assert!(rule2.is_none());
        // --force-with-lease is a conservative match (approval-gated, not denied).
        let (risk3, rule3) = classify_process(
            "git",
            &["push".to_string(), "--force-with-lease".to_string()],
        );
        assert_eq!(risk3, RiskLevel::High);
        assert!(rule3.is_some());
    }

    #[test]
    fn git_clean_fdx_and_hard_reset_gated() {
        for args in [
            vec!["clean".to_string(), "-fdx".to_string()],
            vec!["reset".to_string(), "--hard".to_string()],
            vec![
                "branch".to_string(),
                "-D".to_string(),
                "feature".to_string(),
            ],
        ] {
            let (risk, _) = classify_process("git", &args);
            assert_eq!(risk, RiskLevel::High, "{args:?}");
        }
        let (risk4, _) = classify_process(
            "git",
            &[
                "branch".to_string(),
                "-d".to_string(),
                "feature".to_string(),
            ],
        );
        assert_eq!(risk4, RiskLevel::Low);
    }

    #[test]
    fn env_dump_denied() {
        let (risk, rule) = classify_process("env", &[]);
        assert_eq!(risk, RiskLevel::Critical);
        assert_eq!(rule_verdict(rule.unwrap()), RuleVerdict::Deny);
    }

    // -- §5/§6 structured execution + shell guard -------------------------------

    #[test]
    fn command_spec_is_argv_only_by_default() {
        let s = spec("cargo", &["test"]);
        assert!(!s.is_shell());
        let shell = CommandSpec::from_shell("rm -rf /");
        assert!(shell.is_shell());
        assert_eq!(shell.args.len(), 2);
    }

    #[test]
    fn shell_metachars_all_detected() {
        for c in [
            ";", "&&", "||", "|", "$", "`", "$(ls)", ">", ">>", "<", "\n", "'", "\"", "\\a", "&",
        ] {
            assert!(
                ShellInterpolationGuard::contains_metachar(c),
                "missed {:?}",
                c
            );
        }
    }

    #[test]
    fn safe_values_pass_guard() {
        assert!(ShellInterpolationGuard::is_safe_literal("Cargo.toml"));
        assert!(ShellInterpolationGuard::is_safe_literal("src/main.rs"));
        assert!(ShellInterpolationGuard::is_safe_literal("naïve-文件-æ.rs"));
        assert!(!ShellInterpolationGuard::is_safe_literal("-flag.txt"));
        assert_eq!(
            ShellInterpolationGuard::check("weird; name.txt"),
            ShellValueClass::Unsafe
        );
        assert_eq!(
            ShellInterpolationGuard::check("safe.txt"),
            ShellValueClass::SafeLiteral
        );
        assert_eq!(
            ShellInterpolationGuard::check("bad\0name"),
            ShellValueClass::InvalidNul
        );
    }

    // -- §7/§8 filesystem scope + path validation -------------------------------

    #[test]
    fn traversal_components_rejected_without_disk_access() {
        assert_eq!(
            PathValidator::check_components(Path::new("../../etc/passwd")),
            Err(PathViolation::Traversal)
        );
        assert_eq!(
            PathValidator::check_components(Path::new("a/../../b")),
            Err(PathViolation::Traversal)
        );
        assert_eq!(
            PathValidator::check_components(Path::new("..")),
            Err(PathViolation::Traversal)
        );
        assert!(PathValidator::check_components(Path::new("a/b/c")).is_ok());
    }

    #[test]
    fn unicode_and_space_paths_validate_within_scope() {
        let dir = std::env::temp_dir().join(format!("ft-policy-space-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("naïve folder 文件")).unwrap();
        std::fs::write(dir.join("naïve folder 文件").join("file.rs"), "x").unwrap();
        let v = PathValidator::new(FilesystemScope::Workspace, &dir);
        let ok = v
            .validate(&dir.join("naïve folder 文件").join("file.rs"), None)
            .expect("unicode+space path inside scope");
        assert!(ok.ends_with("file.rs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_escape_rejected() {
        let dir = std::env::temp_dir().join(format!("ft-policy-sym-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("ft-policy-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        #[cfg(unix)]
        {
            let v = PathValidator::new(FilesystemScope::Workspace, &dir);
            let r = v.validate(&dir.join("link/secret.txt"), None);
            assert!(r.is_err(), "symlink escape must be rejected (got {:?})", r);
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn case_differences_are_distinct_until_canonicalization() {
        let dir = std::env::temp_dir().join(format!("ft-policy-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("File.RS"), "x").unwrap();
        let v = PathValidator::new(FilesystemScope::Workspace, &dir);
        let r = v.validate(&dir.join("file.rs"), None);
        if cfg!(target_os = "macos") {
            // macOS default APFS is case-insensitive: `file.rs` canonicalizes
            // to the existing `File.RS`. The check below only applies on
            // case-sensitive filesystems.
        } else {
            assert!(r.is_err(), "wrong-case path must not resolve");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_filesystem_scope_denies_everything() {
        let v = PathValidator::new(FilesystemScope::NoFilesystem, Path::new("/tmp"));
        assert_eq!(
            v.validate(Path::new("/tmp/x"), None),
            Err(PathViolation::Denied)
        );
    }

    #[test]
    fn worktree_root_is_the_only_allowed_root_under_worktree_only() {
        let ws = std::env::temp_dir().join(format!("ft-policy-ws-{}", std::process::id()));
        let wt = std::env::temp_dir().join(format!("ft-policy-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&wt);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let v = PathValidator::new(FilesystemScope::WorktreeOnly, &ws);
        // Inside the worktree root → ok.
        assert!(v.validate(&wt.join("src/main.rs"), Some(&wt)).is_ok());
        // Inside the workspace but outside the worktree → rejected.
        assert!(v.validate(&ws.join("other/file.rs"), Some(&wt)).is_err());
        // No worktree root supplied → rejected.
        assert!(v.validate(&wt.join("src/main.rs"), None).is_err());
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&wt);
    }

    // -- §10 network --------------------------------------------------------------

    #[test]
    fn network_policy_modes() {
        assert!(NetworkPolicy::Blocked
            .evaluate("api.openai.com", None)
            .is_err());
        assert!(NetworkPolicy::Allowed
            .evaluate("api.openai.com", None)
            .is_ok());
        let allow = NetworkPolicy::Allowlist(vec![NetworkAllowance {
            host: "api.openai.com".into(),
            port: Some(443),
            description: "llm".into(),
        }]);
        assert!(allow.evaluate("api.openai.com", Some(443)).is_ok());
        assert!(allow.evaluate("api.openai.com", Some(80)).is_err());
        assert!(allow.evaluate("evil.example", None).is_err());
        // Prompt is *allowed to proceed pending approval* at the raw
        // evaluate() layer; the policy layer above turns it into
        // RequireApproval — never a silent Allow, never a silent Deny.
        assert!(NetworkPolicy::Prompt
            .evaluate("api.openai.com", None)
            .is_ok());
        let eng = PolicyEngine {
            network: NetworkPolicy::Prompt,
            ..Default::default()
        };
        let ev = eng.evaluate(
            &Action::Network {
                host: "api.openai.com".into(),
                port: None,
            },
            &PolicyContext::new("wf"),
        );
        assert_eq!(ev.decision, PolicyDecision::RequireApproval);
    }

    // -- §11 secrets ---------------------------------------------------------------

    #[test]
    fn secret_path_classification() {
        assert_eq!(
            classify_secret_path("~/.ssh/id_ed25519"),
            SecretCategory::Critical
        );
        assert_eq!(classify_secret_path("/repo/.env"), SecretCategory::Critical);
        assert_eq!(
            classify_secret_path("/home/u/.aws/credentials"),
            SecretCategory::Critical
        );
        assert_eq!(
            classify_secret_path("/Library/Keychains/login.keychain-db"),
            SecretCategory::Critical
        );
        assert_eq!(classify_secret_path("src/main.rs"), SecretCategory::Safe);
        assert_eq!(
            classify_secret_path("/repo/.env.example"),
            SecretCategory::Critical
        );
    }

    #[test]
    fn secrets_denied_without_allowance() {
        let p = SecretPolicy::default();
        assert!(p
            .evaluate("wf", "/repo/.env", SecretCategory::Critical)
            .is_err());
        assert!(p
            .evaluate("wf", "src/main.rs", SecretCategory::Safe)
            .is_ok());
    }

    #[test]
    fn critical_secret_requires_human_granted_allowance() {
        let mut p = SecretPolicy::default();
        p.allowances
            .push(SecretAllowance::new("wf", "/repo/", true));
        assert!(p
            .evaluate("wf", "/repo/.env", SecretCategory::Critical)
            .is_ok());
        assert!(p
            .evaluate("wf", "/repo/src/main.rs", SecretCategory::Safe)
            .is_ok());
        // Uncovered workflow cannot use another workflow's allowance.
        assert!(p
            .evaluate("other-wf", "/repo/.env", SecretCategory::Critical)
            .is_err());
        // Machine-granted allowance is not enough for Critical.
        let mut p2 = SecretPolicy::default();
        p2.allowances
            .push(SecretAllowance::new("wf", "/repo/.env", false));
        assert!(p2
            .evaluate("wf", "/repo/.env", SecretCategory::Critical)
            .is_err());
        assert!(p2
            .evaluate("wf", "/repo/.env", SecretCategory::Sensitive)
            .is_ok());
    }

    // -- §13 budget ------------------------------------------------------------------

    #[test]
    fn budget_ledger_enforces_caps() {
        let policy = BudgetPolicy {
            max_agents: Some(4),
            max_cost_cents: Some(1000),
            ..Default::default()
        };
        let mut ledger = BudgetLedger::new();
        for _ in 0..4 {
            assert!(ledger.can_afford(BudgetDimension::AgentCount, 1, &policy));
            ledger.record(BudgetDimension::AgentCount, 1);
        }
        assert!(!ledger.can_afford(BudgetDimension::AgentCount, 1, &policy));
        assert!(ledger
            .check(&policy)
            .iter()
            .any(|(d, _, _)| *d == BudgetDimension::AgentCount));
        ledger.record(BudgetDimension::CostCents, 1500);
        assert!(ledger
            .check(&policy)
            .iter()
            .any(|(d, _, _)| *d == BudgetDimension::CostCents));
    }

    #[test]
    fn budget_increase_requires_authorization() {
        let mut policy = BudgetPolicy {
            max_cost_cents: Some(500),
            ..Default::default()
        };
        let mut ledger = BudgetLedger::new();
        assert!(ledger
            .authorize_increase(&mut policy, BudgetDimension::CostCents, 100_000, false)
            .is_err());
        assert_eq!(policy.max_cost_cents, Some(500));
        assert!(ledger
            .authorize_increase(&mut policy, BudgetDimension::CostCents, 100_000, true)
            .is_ok());
        assert_eq!(policy.max_cost_cents, Some(100_000));
    }

    // -- §14 autonomy -------------------------------------------------------------------

    #[test]
    fn autonomy_matrix_levels() {
        for lvl in [
            AutonomyLevel::Manual,
            AutonomyLevel::Assisted,
            AutonomyLevel::Supervised,
            AutonomyLevel::Autonomous,
        ] {
            assert_eq!(
                autonomy_decision(lvl, RiskLevel::Critical, true),
                PolicyDecision::RequireApproval,
                "{lvl:?} + Critical must require approval"
            );
        }
        assert_eq!(
            autonomy_decision(AutonomyLevel::Manual, RiskLevel::Medium, true),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            autonomy_decision(AutonomyLevel::Assisted, RiskLevel::Medium, true),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            autonomy_decision(AutonomyLevel::Supervised, RiskLevel::Medium, true),
            PolicyDecision::Allow
        );
        assert_eq!(
            autonomy_decision(AutonomyLevel::Autonomous, RiskLevel::High, true),
            PolicyDecision::Allow
        );
        assert_eq!(
            autonomy_decision(AutonomyLevel::Autonomous, RiskLevel::Low, false),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn autonomy_level_ordering_and_labels() {
        assert!(AutonomyLevel::Manual < AutonomyLevel::Autonomous);
        assert_eq!(AutonomyLevel::default(), AutonomyLevel::Manual);
        assert_eq!(
            AutonomyLevel::Supervised.auto_threshold(),
            RiskLevel::Medium
        );
        assert_eq!(AutonomyLevel::Autonomous.auto_threshold(), RiskLevel::High);
        assert!(!autonomy_description(AutonomyLevel::Autonomous).is_empty());
    }

    // -- §15–§16 approvals ---------------------------------------------------------------

    fn approval_store_with_granted() -> (ApprovalStore, ApprovalId) {
        let mut store = ApprovalStore::new();
        let id = store.request(
            "wf-1",
            Some("t1".into()),
            Some("agent-a".into()),
            "npm install",
            action_hash("npm install", &[]),
            RiskLevel::Medium,
            vec!["network requires approval".into()],
            600_000,
        );
        store.grant(&id, "Ali").unwrap();
        (store, id)
    }

    #[test]
    fn approval_honored_once_and_replay_rejected() {
        let (mut store, id) = approval_store_with_granted();
        assert!(store
            .honor(
                &id,
                "wf-1",
                Some("agent-a"),
                &action_hash("npm install", &[])
            )
            .is_ok());
        assert_eq!(
            store.honor(
                &id,
                "wf-1",
                Some("agent-a"),
                &action_hash("npm install", &[])
            ),
            Err(ApprovalError::Replay)
        );
    }

    #[test]
    fn approval_wrong_workflow_and_wrong_agent_rejected() {
        let (mut store, id) = approval_store_with_granted();
        assert_eq!(
            store.honor(
                &id,
                "other-wf",
                Some("agent-a"),
                &action_hash("npm install", &[])
            ),
            Err(ApprovalError::WrongWorkflow)
        );
        let (mut store, id) = approval_store_with_granted();
        assert_eq!(
            store.honor(
                &id,
                "wf-1",
                Some("agent-evil"),
                &action_hash("npm install", &[])
            ),
            Err(ApprovalError::WrongAgent)
        );
    }

    #[test]
    fn approval_expiry_blocks_stale_reuse() {
        let mut store = ApprovalStore::new();
        let id = store.request(
            "wf",
            None,
            None,
            "rm -rf /tmp/x",
            action_hash("rm -rf /tmp/x", &[]),
            RiskLevel::High,
            vec![],
            1,
        );
        store.grant(&id, "Ali").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(
            store.honor(&id, "wf", None, &action_hash("rm -rf /tmp/x", &[])),
            Err(ApprovalError::Expired)
        );
    }

    #[test]
    fn approval_action_change_invalidates_old_approval() {
        let mut store = ApprovalStore::new();
        let id = store.request(
            "wf",
            None,
            None,
            "npm install lodash",
            action_hash("npm install lodash", &[]),
            RiskLevel::Medium,
            vec![],
            600_000,
        );
        store.grant(&id, "Ali").unwrap();
        let changed = store.honor(&id, "wf", None, &action_hash("npm install malware", &[]));
        assert_eq!(changed, Err(ApprovalError::ActionMismatch));
        assert_eq!(
            store.get(&id).map(|a| a.status),
            Some(ApprovalStatus::Invalidated)
        );
    }

    #[test]
    fn approval_pending_cannot_be_honored_before_grant() {
        let mut store = ApprovalStore::new();
        let id = store.request(
            "wf",
            None,
            None,
            "action",
            action_hash("action", &[]),
            RiskLevel::Medium,
            vec![],
            600_000,
        );
        assert_eq!(
            store.honor(&id, "wf", None, &action_hash("action", &[])),
            Err(ApprovalError::NotPending)
        );
        store.reject(&id).unwrap();
        assert_eq!(
            store.honor(&id, "wf", None, &action_hash("action", &[])),
            Err(ApprovalError::AlreadyDecided(ApprovalStatus::Rejected))
        );
    }

    #[test]
    fn approval_store_bounded_memory() {
        let mut store = ApprovalStore::new();
        for i in 0..600 {
            let id = store.request(
                "wf",
                None,
                None,
                &format!("action {i}"),
                action_hash(&format!("action {i}"), &[]),
                RiskLevel::Medium,
                vec![],
                600_000,
            );
            // Decide most of them so pruning has something to drop.
            if i % 2 == 0 {
                store.reject(&id).unwrap();
            }
        }
        assert!(
            store.all().len() <= 512,
            "approval memory must stay bounded"
        );
        // All pending approvals survive pruning.
        assert_eq!(
            store.pending_count(),
            300,
            "pending approvals must never be pruned"
        );
    }

    // -- §1–§2 engine evaluation ---------------------------------------------------------

    fn ctx(wf: &str) -> PolicyContext {
        PolicyContext {
            workflow_id: wf.into(),
            project_root: Some(PathBuf::from("/")),
            ..Default::default()
        }
    }

    #[test]
    fn engine_allows_low_risk_at_supervised() {
        let e = PolicyEngine::default();
        let d = e.evaluate(&Action::Process(spec("cargo", &["check"])), &ctx("wf"));
        assert_eq!(d.decision, PolicyDecision::Allow);
        assert_eq!(d.risk, RiskLevel::Low);
        let d2 = e.evaluate(&Action::Process(spec("git", &["status"])), &ctx("wf"));
        assert_eq!(d2.decision, PolicyDecision::Allow);
    }

    #[test]
    fn engine_denies_disk_wipe_outright() {
        let e = PolicyEngine::default();
        let d = e.evaluate(
            &Action::Process(spec("dd", &["if=/dev/zero", "of=/dev/sda"])),
            &ctx("wf"),
        );
        assert_eq!(d.decision, PolicyDecision::Deny);
        assert_eq!(d.risk, RiskLevel::Critical);
        assert_eq!(d.policy_source, PolicySource::DeterministicRule);
    }

    #[test]
    fn engine_requires_approval_for_npm_install_at_supervised() {
        // §3 example: `npm install` → Medium. At Supervised, Medium runs
        // automatically inside policy scope.
        let e = PolicyEngine::default();
        let d = e.evaluate(&Action::Process(spec("npm", &["install"])), &ctx("wf"));
        assert_eq!(d.risk, RiskLevel::Medium);
        assert_eq!(d.decision, PolicyDecision::Allow);
        // At Manual, Medium requires approval.
        let e_manual = PolicyEngine {
            autonomy: AutonomyLevel::Manual,
            ..Default::default()
        };
        let d_manual = e_manual.evaluate(&Action::Process(spec("npm", &["install"])), &ctx("wf"));
        assert_eq!(d_manual.decision, PolicyDecision::RequireApproval);
        // Network requests at Supervised with a Blocked network policy
        // are denied outright.
        let d_net = e.evaluate(
            &Action::Network {
                host: "registry.npmjs.org".into(),
                port: Some(443),
            },
            &ctx("wf"),
        );
        assert_eq!(d_net.decision, PolicyDecision::Deny);
    }

    #[test]
    fn engine_unknown_executables_require_approval() {
        let e = PolicyEngine::default();
        let d = e.evaluate(
            &Action::Process(spec("/tmp/hidden_tool", &["--do"])),
            &ctx("wf"),
        );
        assert_eq!(d.decision, PolicyDecision::RequireApproval);
        assert_eq!(d.policy_source, PolicySource::DefaultConservative);
        // Allowlisted → automatic.
        let e2 = PolicyEngine {
            process_allowlist: vec!["hidden_tool".into()],
            ..Default::default()
        };
        let d2 = e2.evaluate(
            &Action::Process(spec("/tmp/hidden_tool", &["--do"])),
            &ctx("wf"),
        );
        assert_eq!(d2.decision, PolicyDecision::Allow);
    }

    #[test]
    fn engine_autonomy_escalation_and_budget_increase_never_automatic() {
        let e = PolicyEngine::default();
        let d = e.evaluate(
            &Action::AutonomyChange {
                requested: AutonomyLevel::Autonomous,
            },
            &ctx("wf"),
        );
        assert_eq!(d.decision, PolicyDecision::RequireApproval);
        let b = e.evaluate(
            &Action::Budget {
                dimension: BudgetDimension::CostCents,
                new_cap: 999_999,
            },
            &ctx("wf"),
        );
        assert_eq!(b.decision, PolicyDecision::RequireApproval);
    }

    #[test]
    fn engine_denies_secret_without_allowance() {
        let e = PolicyEngine::default();
        let d = e.evaluate(
            &Action::Secret {
                path: "/repo/.env".into(),
            },
            &ctx("wf"),
        );
        assert_eq!(d.decision, PolicyDecision::Deny);
        assert_eq!(d.risk, RiskLevel::Critical);
    }

    #[test]
    fn engine_network_blocked_denies() {
        let e = PolicyEngine::default();
        let d = e.evaluate(
            &Action::Network {
                host: "evil.example".into(),
                port: Some(443),
            },
            &ctx("wf"),
        );
        assert_eq!(d.decision, PolicyDecision::Deny);
        assert_eq!(d.policy_source, PolicySource::PolicyConfig);
    }

    #[test]
    fn engine_filesystem_scope_enforced() {
        let dir = std::env::temp_dir().join(format!("ft-policy-fs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.rs"), "x").unwrap();
        let e = PolicyEngine {
            filesystem: FilesystemScope::Workspace,
            ..Default::default()
        };
        let mut c = ctx("wf");
        c.project_root = Some(dir.clone());
        let d = e.evaluate(
            &Action::Filesystem {
                path: dir.join("file.rs").to_string_lossy().into(),
                operation: FsOperation::Write,
            },
            &c,
        );
        assert_eq!(d.decision, PolicyDecision::Allow, "{:?}", d);
        let d2 = e.evaluate(
            &Action::Filesystem {
                path: "/etc/passwd".into(),
                operation: FsOperation::Read,
            },
            &c,
        );
        assert_eq!(d2.decision, PolicyDecision::Deny, "{:?}", d2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_roundtrip_via_engine() {
        let mut e = PolicyEngine {
            autonomy: AutonomyLevel::Manual,
            ..Default::default()
        };
        let ev = e.evaluate(&Action::Process(spec("npm", &["install"])), &ctx("wf-1"));
        assert_eq!(ev.decision, PolicyDecision::RequireApproval);
        let id = e.request_approval(&ev, &ctx("wf-1"), &action_hash("npm install", &[]), 600_000);
        e.approvals.grant(&id, "Ali").unwrap();
        assert!(e
            .approvals
            .honor(&id, "wf-1", None, &action_hash("npm install", &[]))
            .is_ok());
    }

    #[test]
    fn persisted_policy_state_roundtrip_without_secrets() {
        let mut e = PolicyEngine {
            autonomy: AutonomyLevel::Autonomous,
            network: NetworkPolicy::Allowlist(vec![NetworkAllowance {
                host: "api.openai.com".into(),
                port: None,
                description: "llm".into(),
            }]),
            ..Default::default()
        };
        e.budget.record(BudgetDimension::CostCents, 42);
        let p: PersistedPolicyState = (&e).into();
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains("sk-ant-"),
            "no secret shapes in persisted policy"
        );
        let back: PolicyEngine = p.into();
        assert_eq!(back.autonomy, AutonomyLevel::Autonomous);
        assert_eq!(back.budget.counters.cost_cents, 42);
    }

    #[test]
    fn approval_request_redacts_secrets_before_persist() {
        // §12/§40 regression: an action string that contains a secret
        // value must never reach the persisted policy slice.
        let mut e = PolicyEngine::default();
        let secret = "sk-ant-SENTINEL_APPROVAL_REDACT_0001234567";
        let ev = e.evaluate(
            &Action::Shell(format!("echo {secret} > /tmp/leak")),
            &ctx("wf-redact"),
        );
        assert_eq!(ev.decision, PolicyDecision::RequireApproval);
        let id = e.request_approval(
            &ev,
            &ctx("wf-redact"),
            &action_hash(&ev.action, &[]),
            600_000,
        );
        let stored = e.approvals.get(&id).expect("approval stored");
        assert!(
            !stored.action.contains(secret),
            "approval action must be redacted: {}",
            stored.action
        );
        let p: PersistedPolicyState = (&e).into();
        let json = serde_json::to_string(&p).unwrap();
        assert!(
            !json.contains(secret),
            "persisted policy must never contain the raw secret"
        );
        assert!(
            json.contains("approval:"),
            "approval identities survive redaction"
        );
    }
}
