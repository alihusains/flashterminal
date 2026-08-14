//! Fake agent adapter (Phase 2B §6).
//!
//! Runs the deterministic `fake-agent` binary through the *same* real PTY
//! path as production adapters, so the normalized lifecycle test suite
//! (interactive input, large output, slow output, long-running sessions,
//! approval, failure, restart) exercises exactly what real agents exercise.

use super::{
    default_prepare_task, find_fake_agent_bin, ActivityHint, AgentAdapterImpl, ChildSpec,
    EnvVarDecision,
};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;
use crate::orchestration::{PreparedTask, TaskContext};
use anyhow::{Context, Result};

#[derive(Debug)]
pub struct FakeAgentAdapter {
    definition: AgentDefinition,
}

#[allow(clippy::new_without_default)]
impl FakeAgentAdapter {
    pub fn new() -> Self {
        #[allow(clippy::new_without_default)]
        Self {
            definition: AgentDefinition {
                id: "fake-agent".to_string(),
                name: "fake-agent".to_string(),
                display_name: "Fake Agent".to_string(),
                command: "fake-agent".to_string(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: None,
                install_hint: Some("cargo build -p fake-agent".to_string()),
            },
        }
    }

    /// Resolves the fake-agent binary or returns a helpful error.
    pub fn resolve_binary() -> Result<std::path::PathBuf> {
        find_fake_agent_bin().context(
            "fake-agent binary not found — build it with `cargo build -p fake-agent`, \
             or point FLASHTERMINAL_FAKE_AGENT_BIN at the executable",
        )
    }
}

impl AgentAdapterImpl for FakeAgentAdapter {
    fn definition(&self) -> &AgentDefinition {
        &self.definition
    }

    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            spawn: true,
            interactive: true,
            stop: true,
            restart: true,
            resize: true,
            resume: false,
            pause: false,
            // The `approval` scenario prints an observable prompt.
            approval_detection: true,
            structured_events: false,
            usage: false,
            cost: false,
            files_tracked: true,
            commands_tracked: true,
        }
    }

    fn candidate_binaries(&self) -> &[&'static str] {
        &["fake-agent"]
    }

    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String> {
        Vec::new()
    }

    fn credential_env_var(&self, _provider_id: &str) -> EnvVarDecision {
        None
    }

    fn apply_resume(&self, _spec: &mut ChildSpec, _launch: &AgentLaunchConfig) {}

    fn detect_activity(&self, line: &str) -> Option<ActivityHint> {
        let l = line.trim();
        if l.is_empty() {
            return None;
        }
        if l.contains("APPROVAL REQUIRED") {
            return Some(ActivityHint::NeedsApproval);
        }
        if l.contains("waiting for input") || l.contains("Exiting") {
            return Some(ActivityHint::Waiting);
        }
        if l.contains("working") || l.starts_with("Working step") {
            return Some(ActivityHint::Working);
        }
        None
    }

    fn prepare_task(&self, ctx: &TaskContext) -> PreparedTask {
        // Deterministic fixture control: the scenario is picked from the
        // task context environment (set by the engine's task API), not
        // from free text.
        let scenario = ctx
            .environment
            .iter()
            .find(|(k, _)| k == "FAKE_AGENT_SCENARIO")
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| "completion".to_string());
        let mut arguments = vec!["--scenario".to_string(), scenario];
        // The scheduler stamps the attempt number so `flaky` can fail the
        // first attempt deterministically (§33).
        if let Some((_, attempt)) = ctx
            .environment
            .iter()
            .find(|(k, _)| k == "FAKE_AGENT_ATTEMPT")
        {
            arguments.push("--attempt".to_string());
            arguments.push(attempt.clone());
        }
        let mut prepared = default_prepare_task(ctx);
        prepared.arguments = arguments;
        prepared
    }
}
