//! OpenCode adapter (Phase 2B §10).
//!
//! Verifiable facts used here:
//! * binary: `opencode` (npm: `opencode-ai`, or the official installer)
//! * credentials: provider-driven — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`
//!   or `OPENROUTER_API_KEY` depending on the configured provider
//! * model: `opencode --model <name>` (documented CLI flag)
//!
//! OpenCode exposes a client SDK / server protocol; no *stable* CLI
//! machine-readable event stream is claimed, so structured events stay
//! unverified and the adapter isolates everything inside this file — a
//! future `StructuredEvents` implementation would land here, not in the
//! runtime.

use super::{ActivityHint, AgentAdapterImpl, ChildSpec, EnvVarDecision};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;

#[derive(Debug)]
pub struct OpenCodeAdapter {
    definition: AgentDefinition,
}

#[allow(clippy::new_without_default)]
impl OpenCodeAdapter {
    pub fn new() -> Self {
        #[allow(clippy::new_without_default)]
        Self {
            definition: AgentDefinition {
                id: "opencode".to_string(),
                name: "opencode".to_string(),
                display_name: "OpenCode".to_string(),
                command: "opencode".to_string(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: Some("https://opencode.ai".to_string()),
                install_hint: Some(
                    "npm install -g opencode-ai (or curl -fsSL https://opencode.ai/install | bash)"
                        .to_string(),
                ),
            },
        }
    }
}

impl AgentAdapterImpl for OpenCodeAdapter {
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
            approval_detection: true,
            structured_events: false,
            usage: false,
            cost: false,
            files_tracked: true,
            commands_tracked: true,
        }
    }

    fn candidate_binaries(&self) -> &[&'static str] {
        &["opencode"]
    }

    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String> {
        Vec::new()
    }

    fn credential_env_var(&self, provider_id: &str) -> EnvVarDecision {
        // OpenCode reads the provider's canonical env var.
        match provider_id {
            "openrouter" => Some("OPENROUTER_API_KEY"),
            "openai" => Some("OPENAI_API_KEY"),
            "anthropic" => Some("ANTHROPIC_API_KEY"),
            _ => None,
        }
    }

    fn supports_model_flag(&self) -> bool {
        true
    }

    fn apply_resume(&self, _spec: &mut ChildSpec, _launch: &AgentLaunchConfig) {}

    fn detect_activity(&self, line: &str) -> Option<ActivityHint> {
        let l = line.trim();
        if l.is_empty() {
            return None;
        }
        // Observable permission prompt.
        if l.contains("Allow?") || (l.contains("Allow") && l.contains("No")) {
            return Some(ActivityHint::NeedsApproval);
        }
        if l.contains("✻") || l.starts_with("●") {
            return Some(ActivityHint::Working);
        }
        None
    }
}
