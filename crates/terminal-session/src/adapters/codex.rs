//! OpenAI Codex CLI adapter (Phase 2B §9).
//!
//! Verifiable facts used here:
//! * binary: `codex` (npm: `@openai/codex`, also `~/.codex/bin`)
//! * credential: `OPENAI_API_KEY` environment variable
//! * model: `codex -m/--model <name>` (documented CLI flag)
//!
//! Activity hints are conservative: "Continue?"-style confirm prompts.
//! Structured event streams (`codex exec --json`) are NOT claimed until the
//! machine-readable protocol is verified end-to-end.

use super::{ActivityHint, AgentAdapterImpl, ChildSpec, EnvVarDecision};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;

#[derive(Debug)]
pub struct CodexAdapter {
    definition: AgentDefinition,
}

#[allow(clippy::new_without_default)]
impl CodexAdapter {
    pub fn new() -> Self {
        #[allow(clippy::new_without_default)]
        Self {
            definition: AgentDefinition {
                id: "codex".to_string(),
                name: "codex".to_string(),
                display_name: "OpenAI Codex".to_string(),
                command: "codex".to_string(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: Some("https://github.com/openai/codex".to_string()),
                install_hint: Some("npm install -g @openai/codex".to_string()),
            },
        }
    }
}

impl AgentAdapterImpl for CodexAdapter {
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
        }
    }

    fn candidate_binaries(&self) -> &[&'static str] {
        &["codex"]
    }

    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String> {
        Vec::new()
    }

    fn credential_env_var(&self, _provider_id: &str) -> EnvVarDecision {
        Some("OPENAI_API_KEY")
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
        // Observable confirm prompt in interactive mode.
        if l.contains("Continue?") || (l.contains("Approve") && l.contains("?")) {
            return Some(ActivityHint::NeedsApproval);
        }
        if l.starts_with("●") || (l.contains("codex") && l.contains("thinking")) {
            return Some(ActivityHint::Working);
        }
        None
    }
}
