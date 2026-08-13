//! BYOK credential storage (Phase 2B §16–§19).
//!
//! API keys are stored in the native OS credential store (macOS Keychain
//! via the `keyring` crate) — never in workspace JSON, agent JSON, SQLite,
//! environment snapshots, logs, or IPC messages.
//!
//! Configuration persists a *reference* (`keychain://flashterminal/<provider>`)
//! rather than the secret itself:
//!
//! ```json
//! { "provider": "anthropic", "credential_ref": "keychain://flashterminal/anthropic" }
//! ```
//!
//! The storage backend is abstracted behind [`CredentialBackend`] so Linux
//! (Secret Service) and Windows (Credential Manager) providers can be added
//! later without touching callers. Tests use [`MemoryBackend`]; production
//! defaults to [`KeychainBackend`].

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A reference to a stored credential — a URI, never the secret itself.
///
/// Canonical form: `keychain://flashterminal/<provider_id>`.
/// Serialized as a plain string so persisted configs never contain secrets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialRef(String);

impl CredentialRef {
    pub const SCHEME: &'static str = "keychain";

    /// Builds `keychain://flashterminal/<provider_id>`.
    pub fn keychain(provider_id: &str) -> Self {
        Self(format!("{}://flashterminal/{}", Self::SCHEME, provider_id))
    }

    /// The provider this reference points at, if it parses.
    pub fn provider_id(&self) -> Option<&str> {
        let rest = self.0.strip_prefix("keychain://flashterminal/")?;
        if rest.is_empty() || rest.contains('/') {
            None
        } else {
            Some(rest)
        }
    }

    /// The raw URI string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The keyring service name for OS storage.
    pub fn keyring_service(&self) -> Option<String> {
        self.provider_id().map(|p| format!("flashterminal-{}", p))
    }

    /// The keyring account name for OS storage.
    pub fn keyring_account(&self) -> &'static str {
        "api_key"
    }

    /// Tries to parse an arbitrary string as a credential reference.
    pub fn parse(s: &str) -> Option<Self> {
        let r = Self(s.to_string());
        r.provider_id().is_some().then_some(r)
    }
}

impl std::fmt::Display for CredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// OS credential storage backend. Production uses the keychain; tests use
/// an in-memory map. Linux/Windows backends can be added later.
pub trait CredentialBackend: Send + Sync {
    /// Stores `secret` under the given service/account.
    fn store(&self, service: &str, account: &str, secret: &str) -> Result<()>;
    /// Retrieves the secret; `Ok(None)` when no entry exists.
    fn retrieve(&self, service: &str, account: &str) -> Result<Option<String>>;
    /// Deletes the entry; errors if it does not exist.
    fn delete(&self, service: &str, account: &str) -> Result<()>;

    /// Non-destructive existence check (defaults to a retrieve).
    fn contains(&self, service: &str, account: &str) -> bool {
        self.retrieve(service, account).ok().flatten().is_some()
    }
}

/// Native OS keychain backend (macOS Keychain; `keyring` falls back to the
/// platform's native store — Secret Service on Linux, Credential Manager on
/// Windows — giving us equivalent providers for those platforms today).
pub struct KeychainBackend;

impl CredentialBackend for KeychainBackend {
    fn store(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        let entry = keyring::Entry::new(service, account)
            .with_context(|| format!("open keychain entry {service}/{account}"))?;
        entry
            .set_password(secret)
            .with_context(|| format!("store secret in keychain {service}/{account}"))
    }

    fn retrieve(&self, service: &str, account: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(service, account)
            .with_context(|| format!("open keychain entry {service}/{account}"))?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("read keychain entry {service}/{account}")),
        }
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let entry = keyring::Entry::new(service, account)
            .with_context(|| format!("open keychain entry {service}/{account}"))?;
        entry
            .delete_password()
            .with_context(|| format!("delete keychain entry {service}/{account}"))
    }
}

/// In-memory backend for tests and the headless CLI (`terminal serve`).
/// Never writes to disk.
pub struct MemoryBackend {
    entries: Mutex<HashMap<(String, String), String>>,
}

impl Default for MemoryBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MemoryBackend {
    /// Never reveals stored values — a derived Debug would print every
    /// secret in the map (Phase 2B.1 §31 security review).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<String> = self
            .entries
            .lock()
            .unwrap()
            .keys()
            .map(|(s, a)| format!("{s}/{a}"))
            .collect();
        f.debug_struct("MemoryBackend")
            .field("entries", &format_args!("{keys:?} (values redacted)"))
            .finish()
    }
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl CredentialBackend for MemoryBackend {
    fn store(&self, service: &str, account: &str, secret: &str) -> Result<()> {
        self.entries.lock().unwrap().insert(
            (service.to_string(), account.to_string()),
            secret.to_string(),
        );
        Ok(())
    }

    fn retrieve(&self, service: &str, account: &str) -> Result<Option<String>> {
        Ok(self
            .entries
            .lock()
            .unwrap()
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let mut map = self.entries.lock().unwrap();
        match map.remove(&(service.to_string(), account.to_string())) {
            Some(_) => Ok(()),
            None => anyhow::bail!("keychain entry {service}/{account} does not exist"),
        }
    }
}

impl Default for CredentialStore {
    /// Default: the native OS keychain (read-only at construction; the
    /// keychain is only touched on store/retrieve).
    fn default() -> Self {
        Self::system()
    }
}

/// The credential store. All callers (engine, IPC, provider test) go
/// through here; nothing else touches the backend directly.
#[derive(Clone)]
pub struct CredentialStore {
    backend: Arc<dyn CredentialBackend>,
}

impl CredentialStore {
    /// A store backed by the native OS keychain.
    pub fn system() -> Self {
        Self {
            backend: Arc::new(KeychainBackend),
        }
    }

    /// A store with a custom backend (tests: [`MemoryBackend`]).
    pub fn with_backend(backend: Arc<dyn CredentialBackend>) -> Self {
        Self { backend }
    }

    /// The backend (for tests and the CLI surface).
    pub fn backend(&self) -> &Arc<dyn CredentialBackend> {
        &self.backend
    }

    /// Stores an API key for a provider. The key is tucked into the OS
    /// store and redacted process-wide as soon as it arrives.
    pub fn set_api_key(&self, provider_id: &str, api_key: &str) -> Result<()> {
        let key = api_key.trim().to_string();
        anyhow::ensure!(
            key.len() >= 8,
            "API key too short — refusing to store what looks like a placeholder"
        );
        let cred_ref = CredentialRef::keychain(provider_id);
        let service = cred_ref
            .keyring_service()
            .context("malformed credential reference")?;
        self.backend
            .store(&service, cred_ref.keyring_account(), &key)?;
        Redactor::register_secret(&key);
        tracing::debug!("stored credential for provider {}", provider_id);
        Ok(())
    }

    /// Retrieves an API key for a provider (`Ok(None)` if not configured).
    /// The resolved value is returned to the caller but never logged; the
    /// caller must not persist it.
    pub fn get_api_key(&self, provider_id: &str) -> Result<Option<String>> {
        let cred_ref = CredentialRef::keychain(provider_id);
        let service = cred_ref
            .keyring_service()
            .context("malformed credential reference")?;
        match self
            .backend
            .retrieve(&service, cred_ref.keyring_account())?
        {
            Some(key) => Ok(Some(key)),
            None => Ok(None),
        }
    }

    /// Removes the API key for a provider and unregisters it from the
    /// redactor so stale redaction entries do not accumulate.
    pub fn remove_api_key(&self, provider_id: &str) -> Result<()> {
        let cred_ref = CredentialRef::keychain(provider_id);
        let service = cred_ref
            .keyring_service()
            .context("malformed credential reference")?;
        if let Some(key) = self
            .backend
            .retrieve(&service, cred_ref.keyring_account())?
        {
            Redactor::unregister_secret(&key);
        }
        self.backend.delete(&service, cred_ref.keyring_account())
    }

    /// Whether an API key is configured for a provider.
    pub fn is_configured(&self, provider_id: &str) -> bool {
        self.get_api_key(provider_id).ok().flatten().is_some()
    }

    /// Resolves a credential reference (URI) to its stored secret, if the
    /// reference names a provider with a configured key.
    pub fn resolve(&self, cred_ref: &CredentialRef) -> Result<Option<String>> {
        let Some(provider_id) = cred_ref.provider_id() else {
            anyhow::bail!("unsupported credential reference `{cred_ref}`");
        };
        self.get_api_key(provider_id)
    }
}

use crate::redact::Redactor;

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CredentialStore {
        CredentialStore::with_backend(Arc::new(MemoryBackend::new()))
    }

    #[test]
    fn credential_ref_uri_roundtrip() {
        let r = CredentialRef::keychain("anthropic");
        assert_eq!(r.as_str(), "keychain://flashterminal/anthropic");
        assert_eq!(r.provider_id(), Some("anthropic"));
        let parsed = CredentialRef::parse("keychain://flashterminal/openrouter").unwrap();
        assert_eq!(parsed.provider_id(), Some("openrouter"));
        assert_eq!(
            r.keyring_service().as_deref(),
            Some("flashterminal-anthropic")
        );
        assert!(CredentialRef::parse("plaintext secret").is_none());
        assert!(CredentialRef::parse("keychain://flashterminal/").is_none());
    }

    #[test]
    fn debug_never_reveals_stored_secrets() {
        // Phase 2B.1 §31 audit: derived Debug on the in-memory backend would
        // print stored API keys; the manual impl must redact them.
        let b = MemoryBackend::new();
        b.store(
            "flashterminal-anthropic",
            "api_key",
            "sk-ant-SECRET_VALUE_1234567890",
        )
        .unwrap();
        let dbg = format!("{b:?}");
        assert!(
            !dbg.contains("SECRET_VALUE"),
            "Debug must not print stored secrets: {dbg}"
        );
        assert!(
            dbg.contains("redacted"),
            "Debug should note redaction: {dbg}"
        );
    }

    #[test]
    fn store_retrieve_remove_roundtrip() {
        let s = store();
        assert!(!s.is_configured("anthropic"));
        s.set_api_key("anthropic", "sk-ant-SENTINEL_KEY_1234567890")
            .unwrap();
        assert!(s.is_configured("anthropic"));
        assert_eq!(
            s.get_api_key("anthropic").unwrap().unwrap(),
            "sk-ant-SENTINEL_KEY_1234567890"
        );
        s.remove_api_key("anthropic").unwrap();
        assert!(!s.is_configured("anthropic"));
        assert_eq!(s.get_api_key("anthropic").unwrap(), None);
        Redactor::unregister_secret("sk-ant-SENTINEL_KEY_1234567890");
    }

    #[test]
    fn resolve_via_credential_ref() {
        let s = store();
        s.set_api_key("openrouter", "sk-or-SENTINEL_KEY_1234567890")
            .unwrap();
        let r = CredentialRef::keychain("openrouter");
        assert_eq!(
            s.resolve(&r).unwrap().unwrap(),
            "sk-or-SENTINEL_KEY_1234567890"
        );
        let missing = CredentialRef::keychain("nope");
        assert_eq!(s.resolve(&missing).unwrap(), None);
        Redactor::unregister_secret("sk-or-SENTINEL_KEY_1234567890");
    }

    #[test]
    fn rejects_short_placeholder_keys() {
        let s = store();
        assert!(s.set_api_key("anthropic", "sk-123").is_err());
        assert!(s.set_api_key("anthropic", "").is_err());
    }

    #[test]
    fn memory_backend_delete_missing_errors() {
        let b = MemoryBackend::new();
        assert!(b.delete("flashterminal-x", "api_key").is_err());
    }

    #[test]
    fn providers_are_independent() {
        let s = store();
        s.set_api_key("anthropic", "sk-ant-SENTINEL_KEY_AAA_1234567890")
            .unwrap();
        s.set_api_key("openai", "sk-SENTINEL_KEY_BBB_1234567890")
            .unwrap();
        assert!(s.is_configured("anthropic"));
        assert!(s.is_configured("openai"));
        s.remove_api_key("anthropic").unwrap();
        assert!(!s.is_configured("anthropic"));
        assert!(s.is_configured("openai"));
        Redactor::unregister_secret("sk-ant-SENTINEL_KEY_AAA_1234567890");
        Redactor::unregister_secret("sk-SENTINEL_KEY_BBB_1234567890");
    }
}
