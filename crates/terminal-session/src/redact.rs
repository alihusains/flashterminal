//! Secret redaction (Phase 2B §22).
//!
//! A basic redaction layer applied to logs, errors, diagnostic reports,
//! agent output, and IPC event payloads so obvious credential leakage is
//! avoided. This is deliberately *not* a perfect DLP layer — it catches:
//!
//! * known API key shapes (Anthropic `sk-ant-…`, OpenAI `sk-…`, Google
//!   `AIza…`, xAI `xai-…`, GitHub `ghp_…`, generic `Bearer …` tokens, etc.)
//! * exact secret values registered at runtime (e.g. a credential resolved
//!   from the keychain just before an agent process is spawned)
//! * sentinel values registered by tests (security tests use these to prove
//!   secrets never reach logs / IPC / persisted files / error reports)

use std::sync::Mutex;
use std::sync::OnceLock;

const MASK: &str = "***REDACTED***";

/// Process-wide registry of exact secret values known to this process.
struct Registry {
    secrets: Mutex<Vec<String>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry {
        secrets: Mutex::new(Vec::new()),
    })
}

/// A known secret shape: a prefix plus the minimum length of the token that
/// follows it before we consider it a secret worth masking. Tokens shorter
/// than that (e.g. the word `Bearer` or `skill`) are left alone.
struct Shape {
    prefix: &'static str,
    min_token_len: usize,
}

const KNOWN_SHAPES: &[Shape] = &[
    // Anthropic keys: sk-ant-api03-<48 chars> (also sk-ant-...)
    Shape {
        prefix: "sk-ant-",
        min_token_len: 12,
    },
    // OpenAI project keys: sk-proj-<48>
    Shape {
        prefix: "sk-proj-",
        min_token_len: 12,
    },
    // Generic OpenAI keys: sk-<32+>
    Shape {
        prefix: "sk-",
        min_token_len: 20,
    },
    // Google AI Studio keys: AIza<35>
    Shape {
        prefix: "AIza",
        min_token_len: 15,
    },
    // xAI keys: xai-<48>
    Shape {
        prefix: "xai-",
        min_token_len: 12,
    },
    // GitHub tokens
    Shape {
        prefix: "ghp_",
        min_token_len: 10,
    },
    Shape {
        prefix: "gho_",
        min_token_len: 10,
    },
    // Authorization header values
    Shape {
        prefix: "Bearer ",
        min_token_len: 16,
    },
    // Key=value forms
    Shape {
        prefix: "API_KEY=",
        min_token_len: 10,
    },
    Shape {
        prefix: "api_key=",
        min_token_len: 10,
    },
    Shape {
        prefix: "apikey=",
        min_token_len: 10,
    },
];

/// Extends `start` over the run of token characters beginning at `start`.
fn token_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() {
        let c = bytes[end];
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'+' | b'/' | b'=' | b':')
        {
            end += 1;
        } else {
            break;
        }
    }
    end
}

/// Finds the byte range `(start, end)` of the first secret in `text`, if any.
fn find_secret(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    // 1. Exact registered values (longest first so nested sentinels mask
    //    the whole value).
    let registered = registry().secrets.lock().unwrap();
    let mut candidates: Vec<&String> = registered.iter().filter(|s| s.len() >= 8).collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for s in candidates {
        if let Some(idx) = text.find(s.as_str()) {
            return Some((idx, idx + s.len()));
        }
    }
    drop(registered);

    // 2. Known key shapes.
    for shape in KNOWN_SHAPES {
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(shape.prefix) {
            let idx = search_from + rel;
            let token_start = idx + shape.prefix.len();
            if token_start < bytes.len() {
                let end = token_end(bytes, token_start);
                if end - token_start >= shape.min_token_len {
                    return Some((idx, end));
                }
            }
            search_from = idx + shape.prefix.len();
        }
    }
    None
}

/// The process-wide secret redactor.
pub struct Redactor;

impl Redactor {
    /// Registers an exact secret value to be masked everywhere. Used for
    /// credentials resolved from the keychain right before they are injected
    /// into an agent process.
    pub fn register_secret(secret: &str) {
        let s = secret.trim();
        if s.len() >= 8 {
            registry().secrets.lock().unwrap().push(s.to_string());
        }
    }

    /// Unregisters a previously registered secret (used when a credential is
    /// replaced or removed).
    pub fn unregister_secret(secret: &str) {
        registry().secrets.lock().unwrap().retain(|s| s != secret);
    }

    /// Redacts all known secrets in `text`. Returns the masked string.
    pub fn redact(text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let mut out = text.to_string();
        while let Some((s, e)) = find_secret(&out) {
            if e <= s {
                break;
            }
            out.replace_range(s..e, MASK);
        }
        out
    }

    /// Convenience for error strings: `Redactor::redact(&err.to_string())`.
    pub fn redact_text<I: std::fmt::Display>(text: &I) -> String {
        Self::redact(&text.to_string())
    }

    /// True when `text` contains no known secret (used by security tests).
    pub fn is_secret_free(text: &str) -> bool {
        find_secret(text).is_none()
    }

    /// True when `text` contains none of the given sentinel values.
    pub fn is_free_of(text: &str, sentinels: &[&str]) -> bool {
        sentinels.iter().all(|s| !text.contains(s))
    }

    /// Clears all registered secrets (tests only).
    pub fn clear_registered() {
        registry().secrets.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_anthropic_key_shape() {
        let text = "failed with key sk-ant-api03-abcdefghijklmnopqrstuvwxyz123456";
        let out = Redactor::redact(text);
        assert!(!out.contains("sk-ant-api03"));
        assert!(out.contains(MASK));
    }

    #[test]
    fn masks_bearer_token() {
        let out = Redactor::redact("Authorization: Bearer sk-1234567890abcdefghijklmnop");
        assert!(!out.contains("sk-1234567890abcdefghijklmnop"));
        assert!(out.contains(MASK));
    }

    #[test]
    fn does_not_mask_short_tokens() {
        // "Bearer token" is a description, not a secret.
        let out = Redactor::redact("sets the Bearer token in the header");
        assert!(!out.contains(MASK));
        // Short sk- words (e.g. "skill") must survive.
        let out = Redactor::redact("develop your skill level");
        assert!(!out.contains(MASK));
    }

    #[test]
    fn masks_registered_sentinel() {
        Redactor::register_secret("SENTINEL_SECRET_VALUE_42");
        let out = Redactor::redact("error: SENTINEL_SECRET_VALUE_42 leaked");
        assert!(!out.contains("SENTINEL_SECRET_VALUE_42"));
        assert!(out.contains(MASK));
        Redactor::unregister_secret("SENTINEL_SECRET_VALUE_42");
    }

    #[test]
    fn registered_secret_cleared_after_unregister() {
        Redactor::register_secret("SENTINEL_SECRET_VALUE_43");
        Redactor::unregister_secret("SENTINEL_SECRET_VALUE_43");
        let out = Redactor::redact("SENTINEL_SECRET_VALUE_43 visible again");
        assert!(out.contains("SENTINEL_SECRET_VALUE_43"));
    }

    #[test]
    fn masks_when_secret_embedded_in_other_text() {
        Redactor::register_secret("SENTINEL_EMBEDDED_KEY");
        let out = Redactor::redact("spawn failed: prefix SENTINEL_EMBEDDED_KEY suffix");
        assert!(!out.contains("SENTINEL_EMBEDDED_KEY"));
        assert!(out.contains(MASK));
        Redactor::unregister_secret("SENTINEL_EMBEDDED_KEY");
    }

    #[test]
    fn secret_free_check() {
        assert!(Redactor::is_secret_free("plain text, no keys"));
        Redactor::register_secret("SENTINEL_FREE_CHECK_1");
        assert!(!Redactor::is_secret_free("contains SENTINEL_FREE_CHECK_1"));
        Redactor::unregister_secret("SENTINEL_FREE_CHECK_1");
    }

    #[test]
    fn multiple_secrets_in_one_line() {
        Redactor::register_secret("SENTINEL_A_1");
        Redactor::register_secret("SENTINEL_B_1");
        let out = Redactor::redact("SENTINEL_A_1 and SENTINEL_B_1 both here");
        assert!(!out.contains("SENTINEL_A_1"));
        assert!(!out.contains("SENTINEL_B_1"));
        Redactor::unregister_secret("SENTINEL_A_1");
        Redactor::unregister_secret("SENTINEL_B_1");
    }
}
