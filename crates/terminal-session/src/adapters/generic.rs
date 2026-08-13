//! Generic CLI adapter (Phase 2B §3).
//!
//! The production-grade baseline: spawns any CLI command in a real PTY
//! with stdin/stdout/stderr, resize, stop, restart, environment and cwd.
//! It has no agent-specific heuristics — activity stays in `Starting` /
//! exit-derived states.

use super::{ActivityHint, AgentAdapterImpl, ChildSpec, EnvVarDecision};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;

#[derive(Debug)]
pub struct GenericCliAdapter {
    definition: AgentDefinition,
}

impl GenericCliAdapter {
    pub fn new() -> Self {
        Self {
            definition: AgentDefinition {
                id: "generic-cli".to_string(),
                name: "generic-cli".to_string(),
                display_name: "Generic CLI".to_string(),
                command: String::new(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: None,
                install_hint: None,
            },
        }
    }

    /// A generic adapter for a caller-supplied definition.
    pub fn for_definition(definition: AgentDefinition) -> Self {
        Self { definition }
    }
}

impl Default for GenericCliAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentAdapterImpl for GenericCliAdapter {
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
            approval_detection: false,
            structured_events: false,
            usage: false,
            cost: false,
            files_tracked: false,
            commands_tracked: false,
        }
    }

    fn candidate_binaries(&self) -> &[&'static str] {
        &[]
    }

    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String> {
        Vec::new()
    }

    fn credential_env_var(&self, provider_id: &str) -> EnvVarDecision {
        // Generic CLI: fall back to the provider's default env var.
        let _ = provider_id;
        None
    }

    fn apply_resume(&self, _spec: &mut ChildSpec, _launch: &AgentLaunchConfig) {}

    fn detect_activity(&self, _line: &str) -> Option<ActivityHint> {
        None
    }
}
