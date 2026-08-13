//! Agent launch configuration (Phase 2B §23, §36).
//!
//! [`AgentLaunchConfig`] is the *declarative, persistable* description of
//! how an agent should run: which definition, where, with what arguments,
//! provider/model/credential references. It never contains raw secrets —
//! credentials are referenced via `keychain://…` URIs.
//!
//! [`AgentLaunchContext`] is the *ephemeral, resolved* form built by the
//! runtime at spawn time: executable path, full argument list, working
//! directory, and the environment that will be handed to the child process
//! (including the secret once it is resolved from the store). It lives only
//! for the duration of the spawn call and is never persisted, logged, or
//! transmitted over IPC.

use crate::credential::CredentialRef;
use serde::{Deserialize, Serialize};

/// Persistable agent launch configuration. No secrets, ever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLaunchConfig {
    /// Registry id of the `AgentDefinition` to run.
    pub definition_id: String,
    /// Working directory for the agent process.
    #[serde(default)]
    pub cwd: String,
    /// Extra command-line arguments (appended after the definition's own
    /// default arguments). For `generic-cli` the first entry, if present,
    /// is the absolute command to execute.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Provider (e.g. `anthropic`, `openrouter`). Optional for local/deterministic
    /// agents; required when a credential must be injected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Model selected for this launch (e.g. a Claude model through OpenRouter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Credential reference URI (`keychain://flashterminal/<provider>`).
    /// A reference, never the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
    /// Resume target (e.g. a Claude Code session id) — only meaningful for
    /// agents whose adapter advertises the `resume` capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_id: Option<String>,
    /// Extra environment variables for the child. MUST NOT contain secrets —
    /// credentials are injected by the runtime from the credential store.
    /// These are persisted with the workspace, so callers must never put
    /// API keys here (the engine redacts them defensively on save anyway).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<(String, String)>,
}

impl AgentLaunchConfig {
    pub fn new(definition_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            definition_id: definition_id.into(),
            cwd: cwd.into(),
            arguments: Vec::new(),
            provider_id: None,
            model_id: None,
            credential_ref: None,
            resume_id: None,
            environment: Vec::new(),
        }
    }

    /// Validates the config without touching the network or the store.
    /// Returns a human-readable error when something is missing.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.definition_id.is_empty(),
            "no agent definition selected"
        );
        if let Some(ref_uri) = &self.credential_ref {
            anyhow::ensure!(
                CredentialRef::parse(ref_uri).is_some(),
                "invalid credential reference `{}` — expected keychain://flashterminal/<provider>",
                ref_uri
            );
        }
        Ok(())
    }

    /// Defensive redaction applied before persistence: any value that looks
    /// like a secret is stripped or masked from persisted state. Arguments
    /// are masked in place (positions preserved so restore/restart still
    /// launches); environment values that carry a secret are removed.
    pub fn redact(&mut self) {
        use crate::redact::Redactor;
        self.arguments = self.arguments.iter().map(|a| Redactor::redact(a)).collect();
        let mut env = Vec::with_capacity(self.environment.len());
        for (k, v) in self.environment.drain(..) {
            if Redactor::is_secret_free(&v) && !is_secret_like_value(&v) {
                env.push((k, v));
            }
        }
        self.environment = env;
    }
}

/// Heuristic guard used by `AgentLaunchConfig::redact`: values starting
/// with a known key prefix are never persisted.
fn is_secret_like_value(v: &str) -> bool {
    let v = v.trim_start();
    ["sk-ant-", "sk-proj-", "sk-", "AIza", "xai-", "ghp_"]
        .iter()
        .any(|p| v.starts_with(p))
}

/// The resolved, ephemeral launch description handed to the PTY.
/// Dropped immediately after spawn — never persisted or logged.
#[derive(Debug, Clone)]
pub struct AgentLaunchContext {
    /// Definition id (for diagnostics).
    pub definition_id: String,
    /// Resolved executable path.
    pub command: String,
    /// Full argument list.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
    /// Environment additions (includes the resolved credential).
    pub env: Vec<(String, String)>,
    /// Provider id (for diagnostics).
    pub provider_id: Option<String>,
    /// Model id (for diagnostics).
    pub model_id: Option<String>,
    /// The credential reference that was resolved (never the secret itself).
    pub credential_ref: Option<String>,
}

impl AgentLaunchContext {
    /// Human-readable descriptor without any secret material.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} {} in {}",
            self.definition_id,
            if self.args.is_empty() {
                String::new()
            } else {
                self.args.join(" ")
            },
            self.cwd
        );
        if let (Some(p), Some(m)) = (&self.provider_id, &self.model_id) {
            s.push_str(&format!(" [{}/{}]", p, m));
        }
        s
    }

    /// The env value for a key, if present (tests).
    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_config_roundtrip_via_json() {
        let cfg = AgentLaunchConfig {
            definition_id: "claude-code".into(),
            cwd: "/tmp".into(),
            arguments: vec!["--dangerously-skip-permissions".into()],
            provider_id: Some("anthropic".into()),
            model_id: Some("claude-sonnet-4-5".into()),
            credential_ref: Some("keychain://flashterminal/anthropic".into()),
            resume_id: None,
            environment: vec![("LANG".into(), "en_US.UTF-8".into())],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(!json.contains("api_key"));
        let back: AgentLaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn config_rejects_plaintext_credential_ref() {
        let cfg = AgentLaunchConfig {
            credential_ref: Some("sk-ant-SOME_PLAINTEXT_KEY_123456789".into()),
            ..AgentLaunchConfig::new("claude-code", "/tmp")
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn redact_strips_secret_like_env() {
        let mut cfg = AgentLaunchConfig {
            environment: vec![
                ("LANG".into(), "en_US.UTF-8".into()),
                ("ANTHROPIC_API_KEY".into(), "sk-ant-1234567890abcdef".into()),
            ],
            ..AgentLaunchConfig::new("claude-code", "/tmp")
        };
        cfg.redact();
        assert_eq!(cfg.environment.len(), 1);
        assert_eq!(cfg.environment[0].0, "LANG");
    }

    #[test]
    fn redact_masks_registered_secret_in_arguments() {
        const SENTINEL: &str = "SUPER_SECRET_TEST_VALUE_2B1";
        crate::redact::Redactor::register_secret(SENTINEL);
        let mut cfg = AgentLaunchConfig {
            arguments: vec![
                "--echo".into(),
                SENTINEL.into(),
                "--keep".into(),
                "plain-value".into(),
            ],
            ..AgentLaunchConfig::new("fake-agent", "/tmp")
        };
        cfg.redact();
        assert_eq!(cfg.arguments.len(), 4, "positions must be preserved");
        assert!(!cfg.arguments[1].contains(SENTINEL));
        assert_eq!(cfg.arguments[2], "--keep");
        assert_eq!(cfg.arguments[3], "plain-value");
        crate::redact::Redactor::unregister_secret(SENTINEL);
    }

    #[test]
    fn context_carries_env_and_describe_is_secret_free() {
        let ctx = AgentLaunchContext {
            definition_id: "claude-code".into(),
            command: "/usr/local/bin/claude".into(),
            args: vec!["--no-session-persistence".into()],
            cwd: "/tmp".into(),
            env: vec![(
                "ANTHROPIC_API_KEY".into(),
                "sk-ant-SECRET_1234567890".into(),
            )],
            provider_id: Some("anthropic".into()),
            model_id: Some("claude-sonnet-4-5".into()),
            credential_ref: None,
        };
        assert_eq!(
            ctx.env_value("ANTHROPIC_API_KEY"),
            Some("sk-ant-SECRET_1234567890")
        );
        let d = ctx.describe();
        assert!(!d.contains("SECRET"));
        assert!(d.contains("anthropic"));
    }
}
