//! Provider and Model abstraction (Phase 2B §13–§15, §19, §30).
//!
//! Strict separation of concerns:
//!
//! ```text
//! Agent      – a CLI agent executable (Claude Code, Codex, …)
//! Provider   – who serves the model (Anthropic, OpenRouter, Ollama, …)
//! Model      – a specific model id within a provider
//! Credential – a stored API key *reference* (keychain://…)
//! ```
//!
//! These are deliberately separate types/registries. The UI selects one of
//! each; the runtime never cares how the credential is stored.
//!
//! Network isolation (§30): provider API calls (`test_connection`) MUST run
//! on a background thread, never the UI/rendering thread. The provided
//! [`HttpProviderConnection`] is blocking — callers (IPC server threads,
//! background test jobs) are responsible for invoking it off the UI thread.
//! Unit tests use [`MockProviderConnection`] or a function-based stub and
//! never touch the network.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

/// A provider (e.g., Anthropic, OpenAI, OpenRouter, local endpoints).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderDefinition {
    pub id: String,
    pub display_name: String,
    /// Endpoint. `None` means "use the provider's default endpoint".
    pub base_url: Option<String>,
    /// True for OpenAI-compatible endpoints (`/v1/chat/completions` etc.).
    pub is_openai_compatible: bool,
    /// Environment variable used to inject this provider's credential into
    /// agent processes (e.g. `ANTHROPIC_API_KEY`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_env_var: Option<String>,
    /// Extra headers for OpenAI-compatible endpoints (e.g. provider keys).
    /// Header *values* must never contain secrets (use the keychain).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    /// Whether this provider is user-configurable (e.g. custom OpenAI-compatible).
    #[serde(default)]
    pub is_custom: bool,
}

impl ProviderDefinition {
    pub fn new(id: &str, display_name: &str, credential_env_var: &str) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            base_url: None,
            is_openai_compatible: false,
            credential_env_var: Some(credential_env_var.to_string()),
            headers: Vec::new(),
            documentation_url: None,
            is_custom: false,
        }
    }

    pub fn openai_compatible(
        id: &str,
        display_name: &str,
        base_url: &str,
        credential_env_var: &str,
    ) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            base_url: Some(base_url.to_string()),
            is_openai_compatible: true,
            credential_env_var: Some(credential_env_var.to_string()),
            headers: Vec::new(),
            documentation_url: None,
            is_custom: false,
        }
    }

    /// A user-configured OpenAI-compatible provider (OpenRouter, LM Studio,
    /// local gateways, Ollama, …) without a fixed endpoint.
    pub fn custom_openai_compatible(
        id: &str,
        display_name: &str,
        credential_env_var: &str,
    ) -> Self {
        Self {
            id: id.to_string(),
            display_name: display_name.to_string(),
            base_url: None,
            is_openai_compatible: true,
            credential_env_var: Some(credential_env_var.to_string()),
            headers: Vec::new(),
            documentation_url: None,
            is_custom: true,
        }
    }

    /// The effective endpoint for this provider.
    pub fn endpoint(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| default_endpoint(&self.id).to_string())
    }

    /// The environment variable agents should receive this provider's
    /// credential through. Adapters may override per-agents.
    pub fn env_var(&self) -> Option<&str> {
        self.credential_env_var.as_deref()
    }
}

/// Capabilities of a model definition (used by the UI to filter choices).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    pub vision: bool,
    pub function_calling: bool,
    pub reasoning: bool,
}

/// A specific model offered by a provider. Provider and model stay separate
/// (`model.provider_id` is a reference, never an inline provider object),
/// and model ids are never hard-coded in the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelDefinition {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

impl ModelDefinition {
    pub fn new(provider_id: &str, model_id: &str, display_name: &str) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            display_name: display_name.to_string(),
            context_window: None,
            capabilities: ModelCapabilities::default(),
        }
    }
}

/// Registry of providers and their model catalogs.
#[derive(Debug, Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, ProviderDefinition>,
    models: HashMap<String, Vec<ModelDefinition>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        let mut registry = Self::default();
        registry.register_builtins();
        registry
    }

    fn register_builtins(&mut self) {
        for p in builtin_providers() {
            self.providers.insert(p.id.clone(), p);
        }
        for (provider_id, models) in builtin_models() {
            self.models.insert(provider_id.to_string(), models);
        }
    }

    /// Registers a custom OpenAI-compatible provider (or replaces one).
    pub fn register_provider(&mut self, def: ProviderDefinition) {
        self.providers.insert(def.id.clone(), def);
    }

    /// Establishes a custom OpenAI-compatible endpoint (base URL + optional
    /// headers). Stored in-memory only; persists nowhere in Phase 2B.
    pub fn configure_openai_compatible(
        &mut self,
        base_url: &str,
        headers: &[(String, String)],
    ) -> Result<()> {
        anyhow::ensure!(
            base_url.starts_with("http://") || base_url.starts_with("https://"),
            "base URL must start with http:// or https://"
        );
        if let Some(p) = self.providers.get_mut("custom-openai") {
            p.base_url = Some(base_url.to_string());
            p.headers = headers.to_vec();
        }
        Ok(())
    }

    pub fn get_provider(&self, id: &str) -> Option<&ProviderDefinition> {
        self.providers.get(id)
    }

    pub fn list_providers(&self) -> Vec<&ProviderDefinition> {
        let mut v: Vec<_> = self.providers.values().collect();
        v.sort_by(|a, b| a.display_name.cmp(&b.display_name));
        v
    }

    pub fn get_models(&self, provider_id: &str) -> Vec<&ModelDefinition> {
        self.models
            .get(provider_id)
            .map(|m| m.iter().collect())
            .unwrap_or_default()
    }

    pub fn get_model(&self, provider_id: &str, model_id: &str) -> Option<&ModelDefinition> {
        self.models
            .get(provider_id)?
            .iter()
            .find(|m| m.model_id == model_id)
    }

    /// True when a model can be chosen freely (OpenAI-compatible providers
    /// accept arbitrary model ids from the endpoint).
    pub fn accepts_arbitrary_models(&self, provider_id: &str) -> bool {
        self.providers
            .get(provider_id)
            .map(|p| p.is_openai_compatible)
            .unwrap_or(false)
    }

    /// The credential env var an agent process should receive for this
    /// provider (provider-specific; adapters may override).
    pub fn credential_env_var(&self, provider_id: &str) -> Option<&str> {
        self.providers
            .get(provider_id)
            .and_then(|p| p.credential_env_var.as_deref())
    }

    /// The number of known models for a provider.
    pub fn model_count(&self, provider_id: &str) -> usize {
        self.models.get(provider_id).map(|m| m.len()).unwrap_or(0)
    }
}

/// Built-in provider catalog (Phase 2B §13). Not every provider ships with
/// a curated model list — model names for OpenAI-compatible endpoints are
/// fetched live in a "Test Connection" and are *not* hard-coded.
fn builtin_providers() -> Vec<ProviderDefinition> {
    vec![
        ProviderDefinition::new("anthropic", "Anthropic", "ANTHROPIC_API_KEY"),
        ProviderDefinition::new("openai", "OpenAI", "OPENAI_API_KEY"),
        ProviderDefinition::new("google", "Google", "GOOGLE_API_KEY"),
        ProviderDefinition::openai_compatible(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
        ),
        ProviderDefinition::openai_compatible(
            "mistral",
            "Mistral",
            "https://api.mistral.ai/v1",
            "MISTRAL_API_KEY",
        ),
        ProviderDefinition::openai_compatible(
            "groq",
            "Groq",
            "https://api.groq.com/openai/v1",
            "GROQ_API_KEY",
        ),
        ProviderDefinition::openai_compatible(
            "together",
            "Together",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
        ),
        ProviderDefinition::openai_compatible(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
        ),
        ProviderDefinition::openai_compatible("xai", "xAI", "https://api.x.ai/v1", "XAI_API_KEY"),
        // Local endpoints — no credential needed, but the env var slot is
        // kept for parity.
        ProviderDefinition::openai_compatible(
            "ollama",
            "Ollama (Local)",
            "http://localhost:11434/v1",
            "OLLAMA_API_KEY",
        ),
        ProviderDefinition::openai_compatible(
            "lm-studio",
            "LM Studio (Local)",
            "http://localhost:1234/v1",
            "LM_STUDIO_API_KEY",
        ),
        ProviderDefinition::custom_openai_compatible(
            "custom-openai",
            "Custom OpenAI-compatible",
            "CUSTOM_OPENAI_API_KEY",
        ),
    ]
}

/// Curated model catalog for providers with stable model ids. This is a
/// *documentation-grade* list: the runtime never hard-codes model ids, the
/// UI can pick from these or type its own for OpenAI-compatible providers.
fn builtin_models() -> Vec<(&'static str, Vec<ModelDefinition>)> {
    let model =
        |provider: &str, id: &str, name: &str, ctx: Option<u64>, caps: ModelCapabilities| {
            ModelDefinition {
                provider_id: provider.to_string(),
                model_id: id.to_string(),
                display_name: name.to_string(),
                context_window: ctx,
                capabilities: caps,
            }
        };
    let sonnet = |p, id, name, ctx| {
        model(
            p,
            id,
            name,
            context_window(ctx),
            ModelCapabilities {
                vision: true,
                function_calling: true,
                reasoning: true,
            },
        )
    };
    vec![
        (
            "anthropic",
            vec![
                sonnet(
                    "anthropic",
                    "claude-sonnet-4-5",
                    "Claude Sonnet 4.5",
                    200_000,
                ),
                model(
                    "anthropic",
                    "claude-opus-4-1",
                    "Claude Opus 4.1",
                    Some(200_000),
                    ModelCapabilities {
                        vision: true,
                        function_calling: true,
                        reasoning: true,
                    },
                ),
                sonnet("anthropic", "claude-haiku-4-5", "Claude Haiku 4.5", 200_000),
            ],
        ),
        (
            "openai",
            vec![
                model(
                    "openai",
                    "gpt-4o",
                    "GPT-4o",
                    None,
                    ModelCapabilities {
                        vision: true,
                        function_calling: true,
                        reasoning: false,
                    },
                ),
                model(
                    "openai",
                    "gpt-4.1",
                    "GPT-4.1",
                    None,
                    ModelCapabilities {
                        vision: true,
                        function_calling: true,
                        reasoning: true,
                    },
                ),
                model(
                    "openai",
                    "o3",
                    "o3",
                    None,
                    ModelCapabilities {
                        vision: false,
                        function_calling: true,
                        reasoning: true,
                    },
                ),
            ],
        ),
        (
            "google",
            vec![
                model(
                    "google",
                    "gemini-2.5-pro",
                    "Gemini 2.5 Pro",
                    None,
                    ModelCapabilities {
                        vision: true,
                        function_calling: true,
                        reasoning: true,
                    },
                ),
                model(
                    "google",
                    "gemini-2.5-flash",
                    "Gemini 2.5 Flash",
                    None,
                    ModelCapabilities {
                        vision: true,
                        function_calling: true,
                        reasoning: false,
                    },
                ),
            ],
        ),
        (
            "xai",
            vec![model(
                "xai",
                "grok-4",
                "Grok 4",
                None,
                ModelCapabilities {
                    vision: true,
                    function_calling: true,
                    reasoning: true,
                },
            )],
        ),
        (
            "mistral",
            vec![model(
                "mistral",
                "mistral-large-latest",
                "Mistral Large",
                Some(128_000),
                ModelCapabilities {
                    vision: true,
                    function_calling: true,
                    reasoning: false,
                },
            )],
        ),
        (
            "groq",
            vec![model(
                "groq",
                "llama-3.3-70b-versatile",
                "Llama 3.3 70B",
                Some(131_072),
                ModelCapabilities {
                    vision: false,
                    function_calling: true,
                    reasoning: false,
                },
            )],
        ),
    ]
}

fn context_window(v: u64) -> Option<u64> {
    Some(v)
}

/// Default endpoints for providers (matches `builtin_providers`).
fn default_endpoint(provider_id: &str) -> String {
    match provider_id {
        "anthropic" => "https://api.anthropic.com/v1".to_string(),
        "openai" => "https://api.openai.com/v1".to_string(),
        "google" => "https://generativelanguage.googleapis.com/v1beta".to_string(),
        "openrouter" => "https://openrouter.ai/api/v1".to_string(),
        "mistral" => "https://api.mistral.ai/v1".to_string(),
        "groq" => "https://api.groq.com/openai/v1".to_string(),
        "together" => "https://api.together.xyz/v1".to_string(),
        "deepseek" => "https://api.deepseek.com/v1".to_string(),
        "xai" => "https://api.x.ai/v1".to_string(),
        "ollama" => "http://localhost:11434/v1".to_string(),
        "lm-studio" => "http://localhost:1234/v1".to_string(),
        _ => "https://localhost/v1".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Provider connection ("Test Connection", §19)
// ---------------------------------------------------------------------------

/// Human-readable result of a provider test. Never contains secret material.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderTestReport {
    pub ok: bool,
    pub provider_id: String,
    pub credential_configured: bool,
    pub endpoint_reachable: bool,
    /// `Some(true)` = credentials accepted; `Some(false)` = rejected;
    /// `None` = not verifiable (no credential, or endpoint without auth).
    pub auth_valid: Option<bool>,
    /// `Some(true)` = the model id is offered by the endpoint.
    pub model_valid: Option<bool>,
    /// Human-readable message (redacted).
    pub message: String,
    pub latency_ms: u64,
}

/// The network boundary. Implementations MUST be invoked from a background
/// thread (never the UI thread). Unit tests use the mock implementation.
pub trait ProviderConnection: Send + Sync {
    /// Tests a provider with an optional credential and optional model id.
    fn test(
        &self,
        provider: &ProviderDefinition,
        credential: Option<&str>,
        model_id: Option<&str>,
    ) -> ProviderTestReport;
}

/// Real HTTP implementation (blocking; background threads only).
#[derive(Debug, Default)]
pub struct HttpProviderConnection {
    timeout_secs: u64,
}

impl HttpProviderConnection {
    pub fn new() -> Self {
        Self { timeout_secs: 10 }
    }
}

fn endpoint_scheme_ok(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

impl ProviderConnection for HttpProviderConnection {
    fn test(
        &self,
        provider: &ProviderDefinition,
        credential: Option<&str>,
        model_id: Option<&str>,
    ) -> ProviderTestReport {
        let t0 = Instant::now();
        let latency = || t0.elapsed().as_millis() as u64;
        let base = provider.endpoint();
        let display = &provider.display_name;

        if !endpoint_scheme_ok(&base) {
            return ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: credential.is_some(),
                endpoint_reachable: false,
                auth_valid: None,
                model_valid: None,
                message: format!("{display}: endpoint `{base}` is malformed (must start with http:// or https://)"),
                latency_ms: latency(),
            };
        }

        // No credential configured → stop early with a helpful message.
        // Local endpoints (Ollama, LM Studio) work without one.
        let Some(_key) = credential else {
            let local = provider
                .base_url
                .as_deref()
                .map(|u| u.starts_with("http://localhost"))
                .unwrap_or(false)
                || provider.is_custom;
            if local {
                return ProviderTestReport {
                    ok: true,
                    provider_id: provider.id.clone(),
                    credential_configured: false,
                    endpoint_reachable: true,
                    auth_valid: None,
                    model_valid: None,
                    message: format!(
                        "{display}: endpoint {base} — no API key needed for local endpoints, but reachability was not verified (add a key or run the test with credentials to verify)"
                    ),
                    latency_ms: latency(),
                };
            }
            return ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: false,
                endpoint_reachable: false,
                auth_valid: None,
                model_valid: None,
                message: format!(
                    "No API key configured for {display}. Add one in Settings → AI Providers → {display} → Add API Key"
                ),
                latency_ms: latency(),
            };
        };

        // Determine the check endpoint and auth header per provider family.
        let url: String = match provider.id.as_str() {
            "google" => format!(
                "{}/models?key={}",
                base.trim_end_matches('/'),
                urlencode(_key)
            ),
            _ => format!("{}/models", base.trim_end_matches('/')),
        };
        let headers: Vec<(String, String)> = match provider.id.as_str() {
            "anthropic" => vec![
                ("x-api-key".to_string(), _key.to_string()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ],
            _ => vec![("Authorization".to_string(), format!("Bearer {}", _key))],
        };

        let resp = ureq::AgentBuilder::new()
            .timeout_connect(std::time::Duration::from_secs(self.timeout_secs))
            .timeout(std::time::Duration::from_secs(self.timeout_secs))
            .build()
            .get(&url);
        let resp = headers
            .into_iter()
            .fold(resp, |r, (k, v)| r.set(k.as_str(), v.as_str()));

        match resp.call() {
            Ok(response) => {
                let status = response.status();
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(&mut response.into_reader(), &mut body);
                let models: Vec<String> = parse_model_ids(&body);
                let model_valid = match model_id {
                    Some(m) => {
                        if provider.is_openai_compatible || models.is_empty() {
                            // OpenAI-compatible endpoints and empty catalogs
                            // accept arbitrary ids — the model list is a hint.
                            None
                        } else {
                            Some(models.iter().any(|x| x == m))
                        }
                    }
                    None => None,
                };
                ProviderTestReport {
                    ok: (200..300).contains(&status),
                    provider_id: provider.id.clone(),
                    credential_configured: true,
                    endpoint_reachable: true,
                    auth_valid: Some((200..300).contains(&status)),
                    model_valid,
                    message: format!(
                        "{display}: connection OK (HTTP {status}, {} models listed, {} ms)",
                        models.len(),
                        latency()
                    ),
                    latency_ms: latency(),
                }
            }
            Err(ureq::Error::Status(code, _)) => {
                let auth_invalid = code == 401 || code == 403;
                ProviderTestReport {
                    ok: false,
                    provider_id: provider.id.clone(),
                    credential_configured: true,
                    endpoint_reachable: true,
                    auth_valid: Some(!auth_invalid),
                    model_valid: None,
                    message: if auth_invalid {
                        format!("{display}: credentials were rejected (HTTP {code}). Check the API key in Settings → AI Providers → {display}")
                    } else {
                        format!("{display}: endpoint responded with HTTP {code}")
                    },
                    latency_ms: latency(),
                }
            }
            Err(ureq::Error::Transport(t)) => ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: true,
                endpoint_reachable: false,
                auth_valid: None,
                model_valid: None,
                message: format!(
                    "{display}: could not reach {base} — {}",
                    transport_message(&t.kind().clone())
                ),
                latency_ms: latency(),
            },
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Very small JSON model-id extractor (no serde_json dependency here —
/// the response body can contain arbitrary provider payloads, so we only
/// look for `"id":"..."` pairs inside the body).
fn parse_model_ids(body: &str) -> Vec<String> {
    let parts: Vec<&str> = body.split('"').collect();
    let mut ids = Vec::new();
    for w in parts.windows(3) {
        if (w[0] == "id" || w[0].ends_with(":id")) && w[1] == ":" {
            let v = w[2].trim();
            if !v.is_empty() && v.len() < 256 && !v.contains(['{', '}', '[']) {
                ids.push(v.to_string());
            }
        }
    }
    ids
}

fn transport_message(kind: &ureq::ErrorKind) -> String {
    let mut s = kind.to_string();
    s = s.trim().to_string();
    if s.is_empty() {
        s = "network error".to_string();
    }
    s
}

/// Mock connection for deterministic tests. Simulation modes cover the
/// required scenarios: ok, auth failure, wrong endpoint, network down,
/// invalid credential, provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockMode {
    Ok,
    AuthRejected,
    EndpointDown,
    CredentialsRejected,
    Timeout,
}

#[derive(Debug)]
pub struct MockProviderConnection {
    pub mode: MockMode,
    pub models: Vec<String>,
}

impl MockProviderConnection {
    pub fn new(mode: MockMode) -> Self {
        Self {
            mode,
            models: vec![
                "claude-sonnet-4-5".to_string(),
                "claude-opus-4-1".to_string(),
            ],
        }
    }
}

impl ProviderConnection for MockProviderConnection {
    fn test(
        &self,
        provider: &ProviderDefinition,
        credential: Option<&str>,
        model_id: Option<&str>,
    ) -> ProviderTestReport {
        match self.mode {
            MockMode::Ok => ProviderTestReport {
                ok: true,
                provider_id: provider.id.clone(),
                credential_configured: credential.is_some(),
                endpoint_reachable: true,
                auth_valid: Some(true),
                model_valid: model_id.map(|m| self.models.iter().any(|x| x == m)),
                message: format!("{}: connection OK", provider.display_name),
                latency_ms: 12,
            },
            MockMode::AuthRejected => ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: true,
                endpoint_reachable: true,
                auth_valid: Some(false),
                model_valid: None,
                message: format!(
                    "{}: credentials were rejected (HTTP 401)",
                    provider.display_name
                ),
                latency_ms: 30,
            },
            MockMode::EndpointDown => ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: credential.is_some(),
                endpoint_reachable: false,
                auth_valid: None,
                model_valid: None,
                message: format!(
                    "{}: could not reach {}",
                    provider.display_name,
                    provider.endpoint()
                ),
                latency_ms: 5,
            },
            MockMode::CredentialsRejected => ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: credential.is_some(),
                endpoint_reachable: true,
                auth_valid: Some(false),
                model_valid: None,
                message: "No API key configured".to_string(),
                latency_ms: 4,
            },
            MockMode::Timeout => ProviderTestReport {
                ok: false,
                provider_id: provider.id.clone(),
                credential_configured: credential.is_some(),
                endpoint_reachable: false,
                auth_valid: None,
                model_valid: None,
                message: format!(
                    "{}: could not reach endpoint (timeout)",
                    provider.display_name
                ),
                latency_ms: 10_000,
            },
        }
    }
}

/// Convenience: runs `connection.test` with the credential resolved from
/// the store. The resolved secret exists only for the duration of this
/// call and never appears in the returned report.
pub fn test_provider_with_store(
    registry: &ProviderRegistry,
    connection: &dyn ProviderConnection,
    store: &crate::credential::CredentialStore,
    provider_id: &str,
    model_id: Option<&str>,
) -> Result<ProviderTestReport> {
    let provider = registry
        .get_provider(provider_id)
        .context(format!("unknown provider `{provider_id}`"))?;
    let credential = store.get_api_key(provider_id)?;
    Ok(connection.test(provider, credential.as_deref(), model_id))
}

// Re-exported for the crate's public API.
pub use crate::credential::CredentialRef;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::{CredentialStore, MemoryBackend};
    use std::sync::Arc;

    #[test]
    fn providers_are_registered() {
        let r = ProviderRegistry::new();
        for id in [
            "anthropic",
            "openai",
            "google",
            "openrouter",
            "mistral",
            "groq",
            "together",
            "deepseek",
            "xai",
            "ollama",
            "lm-studio",
            "custom-openai",
        ] {
            assert!(r.get_provider(id).is_some(), "missing provider {id}");
        }
    }

    #[test]
    fn openai_compatible_flag() {
        let r = ProviderRegistry::new();
        assert!(r.get_provider("openrouter").unwrap().is_openai_compatible);
        assert!(!r.get_provider("anthropic").unwrap().is_openai_compatible);
        assert!(r.accepts_arbitrary_models("openrouter"));
        assert!(!r.accepts_arbitrary_models("anthropic"));
    }

    #[test]
    fn credential_env_vars() {
        let r = ProviderRegistry::new();
        assert_eq!(r.credential_env_var("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(
            r.credential_env_var("openrouter"),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(r.credential_env_var("ollama"), Some("OLLAMA_API_KEY"));
    }

    #[test]
    fn model_lookup() {
        let r = ProviderRegistry::new();
        assert!(r.get_model("anthropic", "claude-sonnet-4-5").is_some());
        assert!(r.get_model("anthropic", "nope").is_none());
        assert!(r.model_count("anthropic") >= 3);
        // OpenAI-compatible providers accept arbitrary models.
        assert!(r.get_models("openrouter").is_empty());
    }

    #[test]
    fn custom_openai_configuration() {
        let mut r = ProviderRegistry::new();
        r.configure_openai_compatible("http://127.0.0.1:8080/v1", &[])
            .unwrap();
        let p = r.get_provider("custom-openai").unwrap();
        assert_eq!(p.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(r.configure_openai_compatible("not a url", &[]).is_err());
    }

    #[test]
    fn mock_ok_report() {
        let conn = MockProviderConnection::new(MockMode::Ok);
        let r = ProviderRegistry::new();
        let p = r.get_provider("anthropic").unwrap();
        let report = conn.test(p, Some("sk-ant-X"), None);
        assert!(report.ok);
        assert!(report.auth_valid == Some(true));
        assert!(!report.message.contains("sk-ant-X"));
    }

    #[test]
    fn mock_auth_rejected() {
        let conn = MockProviderConnection::new(MockMode::AuthRejected);
        let r = ProviderRegistry::new();
        let p = r.get_provider("anthropic").unwrap();
        let report = conn.test(p, Some("sk-ant-WRONG"), None);
        assert!(!report.ok);
        assert!(report.auth_valid == Some(false));
        assert!(report.endpoint_reachable);
    }

    #[test]
    fn mock_network_unavailable() {
        let conn = MockProviderConnection::new(MockMode::EndpointDown);
        let r = ProviderRegistry::new();
        let p = r.get_provider("openrouter").unwrap();
        let report = conn.test(p, Some("sk-or-X"), None);
        assert!(!report.ok);
        assert!(!report.endpoint_reachable);
        assert!(report.auth_valid.is_none());
    }

    #[test]
    fn mock_timeout() {
        let conn = MockProviderConnection::new(MockMode::Timeout);
        let r = ProviderRegistry::new();
        let p = r.get_provider("openai").unwrap();
        let report = conn.test(p, Some("sk-X"), None);
        assert!(!report.ok);
        assert!(!report.endpoint_reachable);
    }

    #[test]
    fn test_with_store_missing_credential() {
        let conn = MockProviderConnection::new(MockMode::Ok);
        let r = ProviderRegistry::new();
        let store = CredentialStore::with_backend(Arc::new(MemoryBackend::new()));
        let report = test_provider_with_store(&r, &conn, &store, "anthropic", None).unwrap();
        assert!(!report.credential_configured);
    }

    #[test]
    fn test_with_store_has_credential() {
        let conn = MockProviderConnection::new(MockMode::Ok);
        let r = ProviderRegistry::new();
        let store = CredentialStore::with_backend(Arc::new(MemoryBackend::new()));
        store
            .set_api_key("anthropic", "sk-ant-SENTINEL_PROVIDER_TEST_123456789")
            .unwrap();
        let report = test_provider_with_store(&r, &conn, &store, "anthropic", None).unwrap();
        assert!(report.credential_configured);
        assert!(report.ok);
        crate::redact::Redactor::unregister_secret("sk-ant-SENTINEL_PROVIDER_TEST_123456789");
    }

    #[test]
    fn unknown_provider_errors() {
        let conn = MockProviderConnection::new(MockMode::Ok);
        let r = ProviderRegistry::new();
        let store = CredentialStore::with_backend(Arc::new(MemoryBackend::new()));
        assert!(test_provider_with_store(&r, &conn, &store, "nope", None).is_err());
    }

    #[test]
    fn parse_model_ids_extracts_list() {
        let body =
            r#"{"data":[{"id":"claude-sonnet-4-5","object":"model"},{"id":"claude-opus-4-1"}]}"#;
        let ids = parse_model_ids(body);
        assert!(ids.contains(&"claude-sonnet-4-5".to_string()));
        assert!(ids.contains(&"claude-opus-4-1".to_string()));
    }
}
