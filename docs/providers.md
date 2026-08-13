# Providers & Model Abstraction

The provider registry (`crates/terminal-session/src/provider.rs`) is the
single source of truth for credential injection and model catalog.

## Registered providers

| id | display | endpoint | credential env var |
|----|---------|----------|--------------------|
| `anthropic` | Anthropic | provider default | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI | provider default | `OPENAI_API_KEY` |
| `google` | Google | provider default | `GOOGLE_API_KEY` |
| `openrouter` | OpenRouter | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |
| `mistral` | Mistral | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` |
| `groq` | Groq | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` |
| `together` | Together | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` |
| `deepseek` | DeepSeek | `https://api.deepseek.com/v1` | `DEEPSEEK_API_KEY` |
| `xai` | xAI | `https://api.x.ai/v1` | `XAI_API_KEY` |
| `ollama` | Ollama (Local) | `http://localhost:11434/v1` | (parity slot, no key) |

`ProviderDefinition` fields: id, display name, optional base URL,
`is_openai_compatible` (endpoints speaking `/v1/chat/completions`-style
APIs), the credential env var, extra headers (values must never hold
secrets — use the keychain), and a custom flag.

## Credential flow

1. `AgentRuntime::spawn` resolves the provider id from `provider_id` (or
   from the `credential_ref` URI).
2. The adapter (or the registry) names the env var, e.g. `ANTHROPIC_API_KEY`.
3. The key is read from the OS keychain via `CredentialStore::get_api_key`.
4. The key value is registered with the `Redactor` *and* injected into the
   child environment — ephemeral `AgentLaunchContext` only.
5. Any later output/error/log/IPC/persistence containing the value is
   masked process-wide.

## Model abstraction

`AgentLaunchConfig.model_id` is a free-form string (e.g. a Claude model id
through OpenRouter) stored with the launch config; the model catalog lives
with the provider registry for future selection UI. Nothing in the engine
parses model ids — agents receive what the user configured.

## Custom endpoints

`is_custom` providers can be registered at runtime (registry API); the
generic CLI adapter hosts arbitrary command definitions, so any TUI agent
can be launched without provider wiring (`docs/agent-runtime.md`).

## Honesty constraint

Capabilities and providers are only claimed when verified — the 2B.1
compatibility matrix (`docs/agent-compatibility.md`) records which
behavior was actually observed per agent/provider, including
authentication failures when no key is configured.
