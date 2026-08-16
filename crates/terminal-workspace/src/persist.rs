//! Versioned persistence (§19, §20, §30).
//!
//! On-disk format is versioned JSON with a migration entry point:
//!
//! ```json
//! { "version": 1, "workspaces": [...], "active_workspace": "..." }
//! ```
//!
//! We persist **structure** (workspaces, tabs, pane tree, ratios, titles,
//! cwd, active tab/pane) — never child-process state. Restoration recreates
//! the structure and the engine best-effort re-spawns sessions from each
//! pane's `cwd` (§19: "restore workspace structure, then recreate terminal
//! sessions where appropriate").

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::model::PersistedState;

/// v1: workspaces + orchestration + worktrees + artifacts + adaptive.
/// v2 (Phase 4 §17/§25/§29): persisted audit trail + persisted policy
/// state (network/secret/budget policy, budget ledger, pending approvals).
pub const CURRENT_VERSION: u32 = 2;

/// Default on-disk location (overridable for tests).
pub fn default_state_path() -> std::path::PathBuf {
    let dir = std::env::var("FLASHTERMINAL_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".flashterminal"))
                .unwrap_or_else(|_| std::path::PathBuf::from(".flashterminal"))
        });
    dir.join("state.json")
}

/// Serializes the full state (pretty JSON, atomic write via temp+rename).
pub fn save(state: &PersistedState, path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create state dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(state).context("serialize state")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Loads and migrates state. Unknown/newer versions return an explicit error
/// so the caller can back up and start fresh (fail-safe, §36). Corrupt or
/// truncated files are backed up to `<path>.corrupt-<unix>` and reported
/// explicitly — never silently deserialized (§28).
pub fn load(path: &std::path::Path) -> Result<PersistedState> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("state file {} does not exist", path.display())
        }
        Err(e) => bail!("read {}: {e}", path.display()),
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            // §28: never blindly deserialize corrupted state — quarantine it
            // and fail loudly with a recovery hint.
            let backup = format!("{}.corrupt-{}", path.display(), now_unix());
            if let Err(be) = std::fs::rename(path, &backup) {
                tracing::warn!("quarantine {path:?} failed: {be}");
            }
            bail!(
                "state file {} is corrupt or truncated: {e}\ncorrupted copy preserved at: {backup}",
                path.display()
            );
        }
    };
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version > CURRENT_VERSION as u64 {
        bail!(
            "state version {} is newer than supported {} — refusing to load",
            version,
            CURRENT_VERSION
        );
    }
    let migrated = migrate(value, version)?;
    let state: PersistedState =
        serde_json::from_value(migrated).context("deserialize persisted state")?;
    Ok(state)
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Version migration chain. Currently v0 (missing version) → v1 → v2.
fn migrate(value: Value, version: u64) -> Result<Value> {
    let mut v = value;
    if version == 0 {
        // Heuristic: an unversioned file with a "workspaces" array is v1.
        if v.get("workspaces").is_some() {
            if let Some(obj) = v.as_object_mut() {
                obj.insert("version".into(), serde_json::json!(1));
            }
            v = migrate(v, 1)?;
        }
    }
    if version == 1 {
        // v1 → v2 (Phase 4 §29): audit + policy are optional serde(default)
        // fields, so a v1 payload is already decodable as v2 — stamp the
        // version so future migrations can depend on it.
        if let Some(obj) = v.as_object_mut() {
            obj.insert("version".into(), serde_json::json!(2));
        }
        v = migrate(v, 2)?;
    }
    if version == 2 {
        // Future migrations chain here (v2 -> v3, ...).
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PaneNode, Tab, Workspace};
    use std::path::PathBuf;
    use terminal_session::execution::{ExecutionId, ExecutionKind};

    fn sample_state() -> PersistedState {
        let mut ws = Workspace::new("My App", "/tmp");
        let root = PaneNode::leaf(crate::model::Pane::new(
            ExecutionKind::Terminal,
            ExecutionId::new(),
            "/tmp",
        ));
        let tab = Tab::new(&ws.id, root);
        let tab_id = tab.id.clone();
        ws.tabs.push(tab);
        ws.active_tab = Some(tab_id);
        let ws_id = ws.id.clone();
        PersistedState {
            version: crate::persist::CURRENT_VERSION,
            workspaces: vec![ws],
            active_workspace: Some(ws_id),
            tasks: None,
            worktrees: None,
            artifacts: None,
            review_reports: None,
            replan_signals: None,
            adaptive: None,
            audit: None,
            policy: None,
        }
    }

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("ft-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        let s = sample_state();
        save(&s, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_newer_version() {
        let dir = std::env::temp_dir().join(format!("ft-persist2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"version": 99, "workspaces": [], "active_workspace": null}"#,
        )
        .unwrap();
        assert!(load(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_unversioned() {
        let dir = std::env::temp_dir().join(format!("ft-persist3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        std::fs::write(&path, r#"{"workspaces": [], "active_workspace": null}"#).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.version, CURRENT_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_v1_to_current() {
        // A real v1 payload (no audit/policy fields) must load as the
        // current version with empty policy/audit — the permanent schema
        // migration test (§29: old state → current version).
        let dir = std::env::temp_dir().join(format!("ft-persist4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "workspaces": [], "active_workspace": null}"#,
        )
        .unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.version, CURRENT_VERSION);
        assert!(back.audit.is_none());
        assert!(back.policy.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_is_quarantined_and_reported() {
        // §28: truncated/corrupt JSON must fail explicitly, never
        // silently parse; the broken file is preserved for recovery.
        let dir = std::env::temp_dir().join(format!("ft-persist5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        std::fs::write(&path, r#"{"version": 2, "workspaces": [{"id": "#).unwrap();
        let err = load(&path).unwrap_err().to_string();
        assert!(
            err.contains("corrupt or truncated"),
            "unexpected error: {err}"
        );
        assert!(err.contains("preserved at"), "recovery hint missing: {err}");
        // The original file must no longer sit at the state path.
        assert!(!path.exists());
        // A quarantined backup must exist.
        let entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            entries.iter().any(|e| e.starts_with("state.json.corrupt-")),
            "no quarantine backup in {entries:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_policy_and_audit_survive_roundtrip() {
        // §17/§25/§29: audit + policy persisted, restored.
        let dir = std::env::temp_dir().join(format!("ft-persist6-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path: PathBuf = dir.join("state.json");
        let mut s = sample_state();
        s.audit = Some(vec![terminal_session::audit::AuditEvent::new(
            terminal_session::audit::AuditEventKind::ActionAllowed,
            "wf",
            "cd /tmp",
            "engine",
        )
        .with_risk(terminal_session::policy::RiskLevel::Low)]);
        s.policy = Some(terminal_session::policy::PersistedPolicyState::from(
            &terminal_session::policy::PolicyEngine::new(),
        ));
        save(&s, &path).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.version, CURRENT_VERSION);
        assert_eq!(back.audit.as_ref().map(|a| a.len()), Some(1));
        assert!(back.policy.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pane_types_serialize() {
        assert_eq!(
            serde_json::to_string(&ExecutionKind::Terminal).unwrap(),
            "\"terminal\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionKind::Agent).unwrap(),
            "\"agent\""
        );
    }
}
