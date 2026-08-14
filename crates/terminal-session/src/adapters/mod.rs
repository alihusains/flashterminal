//! Agent adapters (Phase 2B §3, §7–§11).
//!
//! One shared PTY implementation (the crate's `Session`/`PtyManager`);
//! adapters are pure policy — executable detection, arguments, environment
//! naming, and conservative activity heuristics. Nothing here spawns a
//! second PTY; nothing here calls provider APIs.
//!
//! Adapters deliberately do NOT invent behavior. Capabilities are marked
//! verified-only (see `docs/agent-compatibility.md`); activity hints are
//! documented heuristics that merely refine the UI status line — the raw
//! terminal stream is always the source of truth.

pub mod claude;
pub mod codex;
pub mod fake;
pub mod generic;
pub mod opencode;
pub mod pi;

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::agent::AgentDefinition;
use crate::execution::AgentState;
use crate::launch::AgentLaunchConfig;

/// A refinement hint produced by per-agent activity heuristics (§26).
/// `Completed`/`Failed` are NOT produced here — they come from the process
/// exit state, which is authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityHint {
    Working,
    Waiting,
    NeedsApproval,
}

impl ActivityHint {
    pub fn to_state(&self) -> AgentState {
        match self {
            ActivityHint::Working => AgentState::Working,
            ActivityHint::Waiting => AgentState::Waiting,
            ActivityHint::NeedsApproval => AgentState::NeedsApproval,
        }
    }
}

/// What the adapter wants the runtime to spawn: arguments, environment
/// additions, and working directory. The runtime owns the PTY spawn.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
}

impl ChildSpec {
    pub fn new(cwd: impl Into<String>) -> Self {
        Self {
            args: Vec::new(),
            env: Vec::new(),
            cwd: cwd.into(),
        }
    }
}

/// Checks `path` for executable permission (best-effort on all platforms).
pub fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Finds an executable by name: PATH first, then the common per-user bin
/// directories (Homebrew, ~/.local/bin, agent-specific install dirs).
pub fn find_executable(candidates: &[&str]) -> Option<PathBuf> {
    // Explicit env override (tests and odd installs).
    if let Ok(override_path) = std::env::var("FLASHTERMINAL_AGENT_BIN") {
        let p = PathBuf::from(&override_path);
        if is_executable(&p) {
            return Some(p);
        }
    }
    for c in candidates {
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let p = dir.join(c);
                if is_executable(&p) {
                    return Some(p);
                }
            }
        }
    }
    // Common home install locations.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let subdirs: &[&str] = &[
        ".local/bin",
        "bin",
        ".cargo/bin",
        ".codex/bin",
        ".opencode/bin",
        ".claude/local",
        ".pi/bin",
    ];
    if let Some(home) = home {
        for c in candidates {
            for sub in subdirs {
                let p = home.join(sub).join(c);
                if is_executable(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Resolves the fake-agent test binary: explicit env, then the workspace
/// target dirs (debug/release), then PATH.
pub fn find_fake_agent_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FLASHTERMINAL_FAKE_AGENT_BIN") {
        let p = PathBuf::from(p);
        if is_executable(&p) {
            return Some(p);
        }
    }
    // `cargo build` writes binaries next to the workspace target dir.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for dir in [
        manifest.join("../../target/debug"),
        manifest.join("../../target/release"),
    ] {
        let p = dir.join("fake-agent");
        if is_executable(&p) {
            return Some(p);
        }
    }
    for c in ["fake-agent"] {
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let p = dir.join(c);
                if is_executable(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Shared helper: human-readable error when an agent binary is missing.
pub fn not_found_error(display_name: &str, install_hint: Option<&str>) -> anyhow::Error {
    let hint = install_hint
        .map(|h| format!(" Install it with: {h}"))
        .unwrap_or_default();
    anyhow::anyhow!(
        "{display_name} executable not found on PATH{hint}. \
         Set FLASHTERMINAL_AGENT_BIN to its absolute path if it lives somewhere unusual."
    )
}

/// Builds the credential environment variable name for a provider, giving
/// the adapter the first word. `None` means "fall back to the provider's
/// default env var".
pub type EnvVarDecision = Option<&'static str>;

use crate::orchestration::{PreparedTask, TaskContext};

/// Trait shared by all adapters: definition, capabilities, spawn spec.
pub trait AgentAdapterImpl: Send + Sync {
    fn definition(&self) -> &AgentDefinition;
    fn capabilities(&self) -> crate::agent::AgentCapabilities;
    /// Candidate binary names, most specific first.
    fn candidate_binaries(&self) -> &[&'static str];
    /// Extra static arguments for this agent (e.g. resume flags).
    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String>;
    /// Environment additions this adapter always wants (never secrets).
    fn base_env(&self, _launch: &AgentLaunchConfig) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Env var this adapter injects the provider credential through
    /// (defaults to the provider's own env var).
    fn credential_env_var(&self, _provider_id: &str) -> EnvVarDecision {
        None
    }
    /// Adds `--model <id>` when the adapter supports it for the definition.
    fn supports_model_flag(&self) -> bool {
        false
    }
    /// Modifies `spec` when resuming (e.g. `claude --resume <id>`).
    fn apply_resume(&self, spec: &mut ChildSpec, launch: &AgentLaunchConfig);
    /// Per-line activity heuristic (raw terminal output is authoritative).
    fn detect_activity(&self, line: &str) -> Option<ActivityHint>;
    /// Translates a normalized permission decision into the keystrokes the
    /// agent's approval prompt expects (Phase 2B.1 §18). The default matches
    /// the common `y`/`n` prompt; adapters override for their own wording.
    fn permission_response(&self, decision: crate::agent::PermissionDecision) -> Vec<u8> {
        match decision {
            crate::agent::PermissionDecision::Deny => b"n\n".to_vec(),
            crate::agent::PermissionDecision::AllowOnce
            | crate::agent::PermissionDecision::Allow => b"y\n".to_vec(),
        }
    }
    /// Phase 3A §20: the ONLY place task instructions for a vendor are
    /// constructed. The scheduler never builds vendor prompts — it hands the
    /// structured [`TaskContext`] here and launches what this returns.
    /// The default is vendor-neutral text of the task contract.
    fn prepare_task(&self, ctx: &TaskContext) -> PreparedTask {
        default_prepare_task(ctx)
    }
}

/// The vendor-neutral default task instruction (adapter boundary §20).
/// Extracted so adapters can call it without recursively invoking their own
/// override.
pub fn default_prepare_task(ctx: &TaskContext) -> PreparedTask {
    let mut instructions = String::new();
    instructions.push_str(&format!(
        "TASK: {}\n{}\n",
        ctx.task_title, ctx.task_description
    ));
    if !ctx.dependencies.is_empty() {
        instructions.push_str("DEPENDENCIES (completed):\n");
        for d in &ctx.dependencies {
            instructions.push_str(&format!(
                "  - {} ({}){}\n",
                d.title,
                d.status,
                if d.artifacts.is_empty() {
                    String::new()
                } else {
                    format!(": {}", d.artifacts.join(", "))
                }
            ));
        }
    }
    if !ctx.artifact_paths.is_empty() {
        instructions.push_str(&format!(
            "INPUT ARTIFACTS: {}\n",
            ctx.artifact_paths.join(", ")
        ));
    }
    if !ctx.relevant_files.is_empty() {
        instructions.push_str(&format!(
            "RELEVANT FILES: {}\n",
            ctx.relevant_files.join(", ")
        ));
    }
    instructions.push_str(&format!("WORKSPACE: {}\n", ctx.project_root));
    PreparedTask {
        instructions,
        arguments: Vec::new(),
        environment: Vec::new(),
    }
}

/// Resolves the executable for a definition + launch.
pub fn resolve_binary(adapter: &dyn AgentAdapterImpl, def: &AgentDefinition) -> Result<PathBuf> {
    let mut candidates: Vec<&str> = adapter.candidate_binaries().to_vec();
    if !def.command.is_empty() {
        candidates.insert(0, def.command.as_str());
    }
    if let Some(bin) = find_executable(&candidates) {
        return Ok(bin);
    }
    // Absolute command override (generic CLI) — already a path.
    if !def.command.is_empty() && std::path::Path::new(&def.command).is_file() {
        return Ok(PathBuf::from(&def.command));
    }
    Err(not_found_error(
        &def.display_name,
        def.install_hint.as_deref(),
    ))
    .with_context(|| format!("resolve `{}`", def.display_name))
}

/// Applies the model flag + arguments + env for a launch.
pub fn build_spec(
    adapter: &dyn AgentAdapterImpl,
    def: &AgentDefinition,
    launch: &AgentLaunchConfig,
) -> ChildSpec {
    let mut spec = ChildSpec::new(launch.cwd.clone());
    spec.args.extend(def.args.iter().cloned());
    spec.args.extend(adapter.base_args(launch));
    if let Some(model) = &launch.model_id {
        if adapter.supports_model_flag() {
            spec.args.push("--model".to_string());
            spec.args.push(model.clone());
        }
    }
    spec.args.extend(launch.arguments.iter().cloned());
    spec.env.extend(adapter.base_env(launch));
    spec.env.extend(launch.environment.iter().cloned());
    adapter.apply_resume(&mut spec, launch);
    spec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_found_in_path() {
        // `sh` (or `ls`) is guaranteed present on the test machines used by
        // the CI and practically every dev machine.
        let found = find_executable(&["sh", "ls"]);
        assert!(found.is_some(), "PATH lookup failed for `sh`");
        let p = found.unwrap();
        assert!(p.is_absolute());
    }

    #[test]
    fn missing_executable_returns_none() {
        assert!(find_executable(&["definitely-not-a-real-binary-xyz-42"]).is_none());
    }

    #[test]
    fn not_found_error_is_human_readable() {
        let e = not_found_error(
            "Claude Code",
            Some("npm install -g @anthropic-ai/claude-code"),
        );
        let msg = format!("{e:#}");
        assert!(msg.contains("Claude Code executable not found"));
        assert!(msg.contains("npm install"));
    }
}
