//! Pi adapter (Phase 2B §11).
//!
//! Verifiable facts used here:
//! * binary: `pi` (npm: `@mariozechner/pi`)
//! * credential: `ANTHROPIC_API_KEY` environment variable
//! * model: `pi --model <name>` (documented CLI flag)
//!
//! Activity hints are conservative: confirm prompts. No undocumented flags
//! are passed; no structured protocol is claimed.

use super::{ActivityHint, AgentAdapterImpl, ChildSpec, EnvVarDecision};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;

#[derive(Debug)]
pub struct PiAdapter {
    definition: AgentDefinition,
}

#[allow(clippy::new_without_default)]
impl PiAdapter {
    pub fn new() -> Self {
        #[allow(clippy::new_without_default)]
        Self {
            definition: AgentDefinition {
                id: "pi".to_string(),
                name: "pi".to_string(),
                display_name: "Pi".to_string(),
                command: "pi".to_string(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: Some("https://pi.software".to_string()),
                install_hint: Some("npm install -g @mariozechner/pi".to_string()),
            },
        }
    }
}

impl AgentAdapterImpl for PiAdapter {
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
        &["pi"]
    }

    fn base_args(&self, _launch: &AgentLaunchConfig) -> Vec<String> {
        Vec::new()
    }

    fn credential_env_var(&self, _provider_id: &str) -> EnvVarDecision {
        Some("ANTHROPIC_API_KEY")
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
        if l.contains("Continue?")
            || l.contains("Run command?")
            || (l.contains("Allow") && l.contains("?"))
        {
            return Some(ActivityHint::NeedsApproval);
        }
        if l.starts_with("●") || l.contains("pi is working") {
            return Some(ActivityHint::Working);
        }
        None
    }
}
