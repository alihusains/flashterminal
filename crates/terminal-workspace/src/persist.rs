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

pub const CURRENT_VERSION: u32 = 1;

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
/// so the caller can back up and start fresh (fail-safe, §36).
pub fn load(path: &std::path::Path) -> Result<PersistedState> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("parse state json")?;
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

/// Version migration chain. Currently v0 (missing version) → v1.
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
        // Future migrations chain here (v1 -> v2, ...).
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
            version: 1,
            workspaces: vec![ws],
            active_workspace: Some(ws_id),
            tasks: None,
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
        assert_eq!(back.version, 1);
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
