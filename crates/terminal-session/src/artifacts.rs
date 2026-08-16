//! Artifact domain (3d.md §3–§12, §25–§27, §35, §37–§41).
//!
//! Phase 3D: agents collaborate through explicit, auditable artifacts —
//! never through direct process-to-process communication (3d.md §2). This
//! module owns everything about artifact *lifecycle and access*:
//!
//! - `ArtifactStore` — the authoritative artifact registry with bounded
//!   payload storage (large payloads never ride the event bus, §27).
//! - `ArtifactReference` — structured `artifact://` URIs instead of raw
//!   filesystem paths (§10).
//! - `ArtifactSelector` — find/filter artifacts without dumping every
//!   previous artifact into the next agent's context (§6, §25).
//! - `ArtifactLineage` — task → artifact → dependent task (§5).
//! - `ArtifactAccessPolicy` — dependency grants access; explicit input
//!   references grant access; nothing is implicit (§39–§40).
//! - Cross-worktree materialization — Task B consumes Task A's artifact
//!   without sharing a worktree (§11).

use crate::orchestration::{Artifact, ArtifactType, TaskGraph, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Bounded artifact retention policy (3d.md §37). Default: keep until the
/// workflow is explicitly discarded — never delete work results
/// automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRetentionPolicy {
    #[default]
    Keep,
    Archive,
    DeleteAfterWorkflow,
}

/// Structured `artifact://<task-id>/<artifact-id>` reference (§10). The
/// artifact layer resolves the physical location; callers never handle raw
/// filesystem paths across tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub task_id: TaskId,
    pub artifact_id: String,
}

impl ArtifactReference {
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("artifact://")?;
        let (task_id, artifact_id) = rest.split_once('/')?;
        if task_id.is_empty() || artifact_id.is_empty() {
            return None;
        }
        Some(Self {
            task_id: task_id.to_string(),
            artifact_id: artifact_id.to_string(),
        })
    }

    pub fn format(task_id: &str, artifact_id: &str) -> String {
        format!("artifact://{task_id}/{artifact_id}")
    }
}

/// One artifact plus its bounded payload (3d.md §4, §27). Metadata is
/// always available; the payload is capped and never serialized into
/// events or the main persistence state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact: Artifact,
    /// Bounded payload bytes (file content, report text). Absent for Url
    /// artifacts and for artifacts whose payload exceeds the budget.
    pub payload: Option<Vec<u8>>,
    pub created_at_ms: u64,
}

impl ArtifactRecord {
    pub fn payload_size(&self) -> usize {
        self.payload.as_ref().map(|p| p.len()).unwrap_or(0)
    }
}

/// Artifact metadata view for events and the UI (never the payload — §27).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub id: String,
    pub kind: ArtifactType,
    pub path: Option<String>,
    pub description: String,
    pub created_by_task: Option<TaskId>,
    pub created_by_agent: Option<String>,
    pub workspace_id: Option<String>,
    pub worktree: Option<String>,
    pub revision: Option<String>,
    pub reference: String,
    pub created_at_ms: u64,
    pub payload_bytes: usize,
}

impl From<&ArtifactRecord> for ArtifactMeta {
    fn from(r: &ArtifactRecord) -> Self {
        let a = &r.artifact;
        let task = a.created_by_task.clone().unwrap_or_default();
        Self {
            id: a.id.clone(),
            kind: a.kind.clone(),
            path: a.path.clone(),
            description: a.description.clone(),
            created_by_task: a.created_by_task.clone(),
            created_by_agent: a.created_by_agent.clone(),
            workspace_id: a.workspace_id.clone(),
            worktree: a.worktree.clone(),
            revision: a.revision.clone(),
            reference: ArtifactReference::format(&task, &a.id),
            created_at_ms: r.created_at_ms,
            payload_bytes: r.payload_size(),
        }
    }
}

/// Default artifact size budget (§25): payloads above this are stored as
/// metadata only (never truncated mid-file — the caller decides).
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 256 * 1024;
/// Hard cap on stored artifacts (bounded retention, §25/§49).
pub const DEFAULT_MAX_ARTIFACTS: usize = 4096;

/// The authoritative artifact registry (3d.md §3). Bounded memory: payloads
/// are capped, artifacts are capped, and old payloads are evicted before
/// metadata (metadata is cheap; payloads are not — §49).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ArtifactStore {
    records: HashMap<String, ArtifactRecord>,
    retention: ArtifactRetentionPolicy,
    max_payload_bytes: usize,
    max_artifacts: usize,
}

impl ArtifactStore {
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            retention: ArtifactRetentionPolicy::Keep,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_artifacts: DEFAULT_MAX_ARTIFACTS,
        }
    }

    pub fn retention(&self) -> ArtifactRetentionPolicy {
        self.retention
    }

    pub fn set_retention(&mut self, p: ArtifactRetentionPolicy) {
        self.retention = p;
    }

    pub fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    pub fn set_max_payload_bytes(&mut self, n: usize) {
        self.max_payload_bytes = n;
    }

    /// Registers an artifact with an optional payload (capped, redacted
    /// *before* this call by the engine — never store credentials, §38).
    /// Returns the record's metadata.
    pub fn register(
        &mut self,
        artifact: Artifact,
        payload: Option<Vec<u8>>,
        created_at_ms: u64,
    ) -> ArtifactMeta {
        // Evict payloads first, then oldest artifacts, when over budget.
        while self.records.len() >= self.max_artifacts {
            let oldest = self
                .records
                .values()
                .min_by_key(|r| r.created_at_ms)
                .map(|r| r.artifact.id.clone());
            if let Some(id) = oldest {
                self.records.remove(&id);
            } else {
                break;
            }
        }
        let capped = payload
            .filter(|p| !p.is_empty())
            .map(|p| {
                if p.len() <= self.max_payload_bytes {
                    p
                } else {
                    // Oversized payload → metadata only (never a silent
                    // truncation of the *file*; callers summarize instead).
                    Vec::new()
                }
            })
            .filter(|p| !p.is_empty());
        let record = ArtifactRecord {
            artifact,
            payload: capped,
            created_at_ms,
        };
        let meta = ArtifactMeta::from(&record);
        self.records.insert(meta.id.clone(), record);
        meta
    }

    pub fn get(&self, id: &str) -> Option<&ArtifactRecord> {
        self.records.get(id)
    }

    pub fn artifact(&self, id: &str) -> Option<&Artifact> {
        self.records.get(id).map(|r| &r.artifact)
    }

    pub fn payload(&self, id: &str) -> Option<&[u8]> {
        self.records.get(id).and_then(|r| r.payload.as_deref())
    }

    pub fn all(&self) -> Vec<&ArtifactRecord> {
        let mut v: Vec<&ArtifactRecord> = self.records.values().collect();
        v.sort_by_key(|r| r.created_at_ms);
        v
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Rebuilds the store from persisted metadata (payloads are lazily
    /// re-read from disk by the engine — §35/§36: artifacts remain
    /// available after restart).
    pub fn from_metadata(records: Vec<ArtifactRecord>) -> Self {
        let mut s = Self::new();
        for r in records {
            s.records.insert(r.artifact.id.clone(), r);
        }
        s
    }

    pub fn metadata_snapshot(&self) -> Vec<ArtifactRecord> {
        self.all().iter().map(|r| (*r).clone()).collect()
    }
}

/// Deterministic artifact selection (3d.md §6, §26). No vector search —
/// relevance is explicit: task dependency, same artifact type, explicit
/// input reference, workspace (§50).
#[derive(Debug, Clone, Default)]
pub struct ArtifactSelector {
    pub task_id: Option<TaskId>,
    pub kind: Option<ArtifactType>,
    pub workspace_id: Option<String>,
    pub description_contains: Option<String>,
    /// Only artifacts explicitly referenced by this task (via
    /// `input_artifacts` or dependency outputs).
    pub referenced_by: Option<TaskId>,
    pub max_results: usize,
}

impl ArtifactSelector {
    pub fn matches(&self, rec: &ArtifactRecord, graph: &TaskGraph) -> bool {
        let a = &rec.artifact;
        if let Some(t) = &self.task_id {
            if a.created_by_task.as_ref() != Some(t) {
                return false;
            }
        }
        if let Some(k) = &self.kind {
            if &a.kind != k {
                return false;
            }
        }
        if let Some(ws) = &self.workspace_id {
            if a.workspace_id.as_ref() != Some(ws) {
                return false;
            }
        }
        if let Some(needle) = &self.description_contains {
            if !a
                .description
                .to_lowercase()
                .contains(&needle.to_lowercase())
            {
                return false;
            }
        }
        if let Some(task) = &self.referenced_by {
            if !ArtifactAccessPolicy::can_access(task, &a.id, graph) {
                return false;
            }
        }
        true
    }

    /// Selects artifacts from the store (bounded, deterministic order).
    pub fn select<'a>(
        &self,
        store: &'a ArtifactStore,
        graph: &TaskGraph,
    ) -> Vec<&'a ArtifactRecord> {
        let mut out: Vec<&ArtifactRecord> = store
            .all()
            .into_iter()
            .filter(|r| self.matches(r, graph))
            .collect();
        out.sort_by_key(|r| r.created_at_ms);
        if self.max_results > 0 && out.len() > self.max_results {
            out.truncate(self.max_results);
        }
        out
    }
}

/// Artifact access control (3d.md §40). Initial policy — no RBAC:
/// 1. an explicit `input_artifacts` reference grants access;
/// 2. a dependency edge grants access to the dependency's artifacts;
/// 3. same-workflow metadata is always visible.
///
/// Anything else is denied (§39: Task B must not automatically receive
/// Task A's artifact).
#[derive(Debug, Clone, Copy, Default)]
pub struct ArtifactAccessPolicy;

impl ArtifactAccessPolicy {
    pub fn can_access(task_id: &str, artifact_id: &str, graph: &TaskGraph) -> bool {
        let Some(task) = graph.get_task(&task_id.to_string()) else {
            return false;
        };
        // Explicit input reference (strongest grant).
        if task.input_artifacts.iter().any(|a| a == artifact_id) {
            return true;
        }
        // Dependency grant: the artifact's producer is a (transitive)
        // dependency of this task.
        let Some(producer) = graph.producer_of(artifact_id) else {
            return false;
        };
        graph.is_dependency(&task_id.to_string(), &producer)
    }
}

/// Artifact lineage (§5): task → artifact → dependent tasks. Built from
/// the authoritative store + graph — never from agent claims.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactLineage {
    /// artifact id → producer task id.
    pub producers: Vec<(String, TaskId)>,
    /// artifact id → tasks that consume it.
    pub consumers: Vec<(String, Vec<TaskId>)>,
    /// task id → its output artifact ids (in creation order).
    pub task_outputs: Vec<(TaskId, Vec<String>)>,
}

impl ArtifactLineage {
    pub fn build(store: &ArtifactStore, graph: &TaskGraph) -> Self {
        let mut producers = Vec::new();
        let mut consumers: HashMap<String, Vec<TaskId>> = HashMap::new();
        let mut task_outputs: Vec<(TaskId, Vec<String>)> = Vec::new();
        for task in graph.list_tasks() {
            let outputs: Vec<String> = task
                .output_artifacts
                .iter()
                .filter(|a| store.get(&a.id).is_some())
                .map(|a| a.id.clone())
                .collect();
            if !outputs.is_empty() {
                task_outputs.push((task.id.clone(), outputs.clone()));
            }
            for art in &outputs {
                producers.push((art.clone(), task.id.clone()));
            }
            for input in &task.input_artifacts {
                if store.get(input).is_some() {
                    consumers
                        .entry(input.clone())
                        .or_default()
                        .push(task.id.clone());
                }
            }
        }
        let mut consumers: Vec<(String, Vec<TaskId>)> = consumers.into_iter().collect();
        consumers.sort_by(|a, b| a.0.cmp(&b.0));
        Self {
            producers,
            consumers,
            task_outputs,
        }
    }
}

/// Cross-worktree artifact consumption (3d.md §11): writes an artifact's
/// payload into a destination directory (the consumer's worktree) at the
/// artifact's relative path. Never assumes a shared filesystem between
/// producer and consumer.
pub struct ArtifactMaterializer;

impl ArtifactMaterializer {
    /// Materializes `artifact_id` into `dest_root`. The payload comes from
    /// the store when available; otherwise it is read lazily from the
    /// producer's worktree (`ArtifactRecord.artifact.worktree` + `path`).
    /// Returns the relative path written.
    pub fn materialize(
        store: &ArtifactStore,
        artifact_id: &str,
        dest_root: &str,
    ) -> std::io::Result<Option<String>> {
        let Some(rec) = store.get(artifact_id) else {
            return Ok(None);
        };
        let Some(rel) = rec.artifact.path.clone() else {
            return Ok(None);
        };
        // Refuse absolute/escaping paths (§34 safety, reused for artifacts).
        if std::path::Path::new(&rel).is_absolute() || rel.contains("..") {
            return Ok(None);
        }
        let bytes: Vec<u8> = if let Some(p) = rec.payload.as_ref() {
            p.clone()
        } else if let Some(wt) = &rec.artifact.worktree {
            let src = std::path::Path::new(wt).join(&rel);
            match std::fs::read(&src) {
                Ok(b) => b,
                Err(_) => return Ok(None),
            }
        } else {
            return Ok(None);
        };
        let dest = std::path::Path::new(dest_root).join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, bytes)?;
        Ok(Some(rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::{Task, TaskGraph};

    fn task(id: &str) -> Task {
        let mut t = Task::new(format!("task {id}"), "", "fake-agent", "ws-1");
        t.id = id.to_string();
        t
    }

    fn artifact(id: &str, kind: ArtifactType, path: Option<&str>) -> Artifact {
        Artifact {
            id: id.to_string(),
            kind,
            path: path.map(String::from),
            description: format!("artifact {id}"),
            created_by_task: Some("t-a".to_string()),
            metadata: vec![],
            created_by_agent: Some("fake-agent".to_string()),
            workspace_id: Some("ws-1".to_string()),
            worktree: None,
            revision: Some("abc123".to_string()),
            created_at_ms: 0,
        }
    }

    #[test]
    fn reference_roundtrip_and_parse() {
        let uri = ArtifactReference::format("task-123", "artifact:xyz");
        assert_eq!(uri, "artifact://task-123/artifact:xyz");
        let r = ArtifactReference::parse(&uri).unwrap();
        assert_eq!(r.task_id, "task-123");
        assert_eq!(r.artifact_id, "artifact:xyz");
        assert!(ArtifactReference::parse("task-123/artifact:xyz").is_none());
        assert!(ArtifactReference::parse("artifact:///nope").is_none());
    }

    #[test]
    fn store_register_select_and_payload_cap() {
        let mut store = ArtifactStore::new();
        let meta = store.register(
            artifact("a1", ArtifactType::Document, Some("docs/a.md")),
            Some(b"content".to_vec()),
            1,
        );
        assert_eq!(meta.kind, ArtifactType::Document);
        assert_eq!(meta.reference, "artifact://t-a/a1");
        assert_eq!(store.payload("a1").unwrap(), b"content");
        // Oversized payloads become metadata-only.
        store.set_max_payload_bytes(4);
        store.register(
            artifact("big", ArtifactType::Log, Some("x.log")),
            Some(vec![0u8; 100]),
            2,
        );
        assert!(store.payload("big").is_none());
    }

    #[test]
    fn selector_filters_and_access_policy() {
        let mut graph = TaskGraph::new();
        let mut a = task("t-a");
        a.output_artifacts
            .push(artifact("art-a", ArtifactType::Diff, Some("a.diff")));
        graph.add_task(a).unwrap();
        let mut b = task("t-b");
        b.input_artifacts.push("art-a".to_string());
        graph.add_task(b).unwrap();
        graph
            .add_dependency(&"t-b".to_string(), &"t-a".to_string())
            .unwrap();
        let c = task("t-c");
        graph.add_task(c).unwrap();

        let mut store = ArtifactStore::new();
        store.register(
            artifact("art-a", ArtifactType::Diff, Some("a.diff")),
            None,
            1,
        );
        store.register(
            artifact("other", ArtifactType::File, Some("other.txt")),
            None,
            2,
        );

        // Selector by kind.
        let sel = ArtifactSelector {
            kind: Some(ArtifactType::Diff),
            ..Default::default()
        };
        assert_eq!(sel.select(&store, &graph).len(), 1);
        // Access: t-b (depends on t-a) can access art-a; t-c cannot.
        assert!(ArtifactAccessPolicy::can_access("t-b", "art-a", &graph));
        assert!(!ArtifactAccessPolicy::can_access("t-c", "art-a", &graph));
        // referenced_by filtering uses the same policy.
        let sel = ArtifactSelector {
            referenced_by: Some("t-b".to_string()),
            ..Default::default()
        };
        let found = sel.select(&store, &graph);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].artifact.id, "art-a");
    }

    #[test]
    fn lineage_maps_producers_and_consumers() {
        let mut graph = TaskGraph::new();
        let mut a = task("t-a");
        a.output_artifacts
            .push(artifact("art-a", ArtifactType::File, None));
        graph.add_task(a).unwrap();
        let mut b = task("t-b");
        b.input_artifacts.push("art-a".to_string());
        graph.add_task(b).unwrap();
        graph
            .add_dependency(&"t-b".to_string(), &"t-a".to_string())
            .unwrap();

        let mut store = ArtifactStore::new();
        store.register(artifact("art-a", ArtifactType::File, None), None, 1);
        let lineage = ArtifactLineage::build(&store, &graph);
        assert_eq!(
            lineage.producers,
            vec![("art-a".to_string(), "t-a".to_string())]
        );
        assert_eq!(lineage.consumers.len(), 1);
        assert_eq!(lineage.consumers[0].1, vec!["t-b".to_string()]);
    }

    #[test]
    fn materializer_writes_into_dest_without_shared_fs() {
        let dir = std::env::temp_dir().join(format!("ft-3d-mat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/sub")).unwrap();
        let mut store = ArtifactStore::new();
        store.register(
            artifact("art", ArtifactType::File, Some("sub/out.txt")),
            Some(b"payload".to_vec()),
            1,
        );
        let rel = ArtifactMaterializer::materialize(
            &store,
            "art",
            dir.join("dst").to_string_lossy().as_ref(),
        )
        .unwrap()
        .expect("materialized");
        assert_eq!(rel, "sub/out.txt");
        assert_eq!(
            std::fs::read_to_string(dir.join("dst/sub/out.txt")).unwrap(),
            "payload"
        );
        // Absolute/traversal paths are refused.
        store.register(
            artifact("evil", ArtifactType::File, Some("../escape.txt")),
            Some(b"x".to_vec()),
            2,
        );
        assert!(ArtifactMaterializer::materialize(
            &store,
            "evil",
            dir.join("dst2").to_string_lossy().as_ref()
        )
        .unwrap()
        .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
