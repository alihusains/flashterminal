//! Claude Code adapter (Phase 2B §8).
//!
//! Verifiable facts used here:
//! * binary: `claude` (installs to `~/.local/bin` via the official
//!   installer, or whatever `npm i -g @anthropic-ai/claude-code` selects)
//! * credential: `ANTHROPIC_API_KEY` environment variable
//! * resume: `claude --resume [session-id]` (documented CLI flags)
//! * model: `claude --model <name>` (documented CLI flag)
//!
//! Activity hints are heuristic refinements of observable output
//! (permission prompts, the working spinner). The raw terminal stream stays
//! authoritative — this never replaces output with a summary.

use super::{ActivityHint, AgentAdapterImpl, ChildSpec, EnvVarDecision};
use crate::agent::{AgentCapabilities, AgentDefinition};
use crate::launch::AgentLaunchConfig;

#[derive(Debug)]
pub struct ClaudeCodeAdapter {
    definition: AgentDefinition,
}

#[allow(clippy::new_without_default)]
impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        #[allow(clippy::new_without_default)]
        Self {
            definition: AgentDefinition {
                id: "claude-code".to_string(),
                name: "claude-code".to_string(),
                display_name: "Claude Code".to_string(),
                command: "claude".to_string(),
                args: Vec::new(),
                protocol: "cli".to_string(),
                documentation_url: Some(
                    "https://docs.anthropic.com/en/docs/claude-code/overview".to_string(),
                ),
                install_hint: Some(
                    "curl -fsSL https://claude.ai/install.sh | bash (or npm install -g @anthropic-ai/claude-code)"
                        .to_string(),
                ),
            },
        }
    }
}

impl AgentAdapterImpl for ClaudeCodeAdapter {
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
            resume: true,
            pause: false,
            // Permission prompts are observable in the TUI; detection is a
            // heuristic refinement only.
            approval_detection: true,
            structured_events: false,
            usage: false,
            cost: false,
            files_tracked: true,
            commands_tracked: true,
        }
    }

    fn candidate_binaries(&self) -> &[&'static str] {
        &["claude"]
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

    fn apply_resume(&self, spec: &mut ChildSpec, launch: &AgentLaunchConfig) {
        // `claude --resume` re-attaches to the most recent session;
        // `claude --resume <id>` resumes a specific one. Only used when the
        // runtime was asked to resume (launch.resume_id set).
        if launch.resume_id.is_some() {
            spec.args.push("--resume".to_string());
            if let Some(id) = &launch.resume_id {
                spec.args.push(id.clone());
            }
        }
    }

    fn detect_activity(&self, line: &str) -> Option<ActivityHint> {
        let l = line.trim();
        if l.is_empty() {
            return None;
        }
        // Observable permission prompt (TUI prints it to the stream).
        if l.contains("Do you want to continue")
            || (l.contains("Allow") && l.contains("y/n"))
            || l.contains("Permission to execute")
        {
            return Some(ActivityHint::NeedsApproval);
        }
        // The spinner/task marker means Claude is actively working.
        if l.contains("✦")
            || l.contains("Thinking")
            || l.starts_with("●")
            || (l.contains("working") && l.contains("Claude"))
        {
            return Some(ActivityHint::Working);
        }
        None
    }
}
