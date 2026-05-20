//! `KnowledgeService` — gateway-facing facade over the KB store for the
//! desktop `/api/v1/knowledge` API.
//!
//! Collections are a tag veneer over the single KB store (see the project
//! note `kb-desktop-collections`): a collection is metadata here plus a
//! `collection:<id>` tag on its docs. There is no per-collection store or
//! embedder. P1 covers collection metadata CRUD; docs + search grow the
//! service in P2/P3.

use crate::kb::canonicalize::{canonicalize_by_mime, detect_mime, CanonicalizeInput};
use crate::kb::content_store::read::read_doc_body;
use crate::kb::embedder::resolve_embedder;
use crate::kb::jobs::{Job, JobKind};
use crate::kb::model::{
    collection_tag, CallerScope, ChunkStatus, KbChunk, KbCollection, KbDoc, KbStatus,
    COLLECTION_TAG_PREFIX,
};
use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
use crate::kb::store::codec::decode;
use crate::kb::store::schema::{KB_CHUNKS, KB_DOCS};
use crate::kb::store::{collections, docs};
use crate::kb::search::SearchCtx;
use crate::kb::tools::kb_search::{self, KbSearchFilter, KbSearchInput};
use crate::kb::worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool};
use crate::kb::{KbEmbedder, KbIndex, KbPaths, KbStore};
use redb::ReadableTable;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// Service-level errors the HTTP layer maps to status codes + error envelope.
#[derive(Debug, thiserror::Error)]
pub enum KnowledgeError {
    #[error("collection_not_found")]
    CollectionNotFound,
    #[error("duplicate_name")]
    DuplicateName,
    #[error("doc_not_found")]
    DocNotFound,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type KResult<T> = Result<T, KnowledgeError>;

/// A document's API-facing summary. `status()` derives indexing state: a doc
/// is `ready` once its chunks are embedded+indexed, else `indexing`.
pub struct DocInfo {
    pub id: String,
    pub title: String,
    pub mime: String,
    pub bytes: u64,
    pub chunk_count: usize,
    pub created_at: i64,
    pub updated_at: i64,
}

impl DocInfo {
    pub fn status(&self) -> &'static str {
        if self.chunk_count > 0 {
            "ready"
        } else {
            "indexing"
        }
    }
}

/// One semantic search result.
pub struct SearchHit {
    pub doc_id: String,
    pub collection_id: Option<String>,
    pub collection_name: Option<String>,
    pub source_title: String,
    pub chunk_text: String,
    pub score: f32,
}

/// Aggregate knowledge-base stats.
pub struct KbStats {
    pub collection_count: usize,
    pub doc_count: usize,
    pub chunk_count: usize,
    pub bytes: u64,
}

/// An embedder the user can select for a collection.
pub struct EmbedderInfo {
    pub id: String,
    pub label: String,
    pub dim: usize,
    pub downloaded: bool,
    pub is_default: bool,
}

pub struct KnowledgeService {
    store: Arc<KbStore>,
    paths: Arc<KbPaths>,
    index: Arc<KbIndex>,
    embedder: Arc<dyn KbEmbedder>,
    kb_root: PathBuf,
    /// Broadcasts `knowledge.doc.status_changed` JSON events to SSE
    /// subscribers (`GET /api/v1/knowledge/events`).
    events: broadcast::Sender<String>,
    /// Asymmetric query instruction from `memorySearch.queryInstruction`
    /// (default None = symmetric). Applied to search queries' dense side.
    query_instruction: Option<String>,
    /// Max accepted upload size in bytes (`kb.maxDocMb`, default 50 MB).
    max_doc_bytes: usize,
}

/// Default max upload size when `kb.maxDocMb` is unset (50 MB).
const DEFAULT_MAX_DOC_BYTES: usize = 50 * 1024 * 1024;

impl KnowledgeService {
    /// Open (or create) the KB under `kb_root` (e.g. `<base>/kb`): store,
    /// HNSW + full-text index, and the resolved embedder.
    pub fn open(kb_root: PathBuf) -> anyhow::Result<Self> {
        let paths = Arc::new(KbPaths::new(&kb_root));
        paths.ensure_layout()?;
        let store = Arc::new(KbStore::open(&kb_root.join("kb.redb"))?);
        let embedder = resolve_embedder(&kb_root);
        let dim = embedder.dimension();
        let index = Arc::new(KbIndex::open_and_rebuild_with_dim(&paths, &store, dim)?);
        let (events, _) = broadcast::channel(256);
        let cfg = crate::config::load().ok();
        let query_instruction = cfg
            .as_ref()
            .and_then(|c| c.raw.memory_search.clone())
            .and_then(|m| m.query_instruction);
        // kb.maxDocMb → bytes; default 50 MB, clamp negatives/zero to default.
        let max_doc_bytes = cfg
            .as_ref()
            .and_then(|c| c.raw.kb.as_ref())
            .and_then(|k| k.max_doc_mb)
            .filter(|mb| *mb > 0)
            .map(|mb| mb as usize * 1024 * 1024)
            .unwrap_or(DEFAULT_MAX_DOC_BYTES);
        Ok(Self {
            store,
            paths,
            index,
            embedder,
            kb_root,
            events,
            query_instruction,
            max_doc_bytes,
        })
    }

    /// Max accepted upload size in bytes (`kb.maxDocMb`, default 50 MB).
    pub fn max_doc_bytes(&self) -> usize {
        self.max_doc_bytes
    }

    /// Subscribe to `knowledge.doc.status_changed` events (for SSE).
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Emit a `status_changed=ready` event for each doc that has newly gained
    /// its first indexed chunk since `emitted` was last updated. `emitted`
    /// tracks already-announced docs to avoid duplicates.
    fn emit_ready_transitions(&self, emitted: &mut HashSet<String>) {
        if let Ok(ready) = self.ready_doc_ids() {
            for id in ready {
                if emitted.insert(id.clone()) {
                    let payload = serde_json::json!({
                        "type": "knowledge.doc.status_changed",
                        "docId": id,
                        "status": "ready",
                    })
                    .to_string();
                    let _ = self.events.send(payload);
                }
            }
        }
    }

    /// Active doc ids that currently have ≥1 indexed chunk (i.e. `ready`).
    fn ready_doc_ids(&self) -> anyhow::Result<HashSet<String>> {
        let rtx = self.store.begin_read()?;
        Ok(self
            .chunk_counts(&rtx)?
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .map(|(id, _)| id)
            .collect())
    }

    pub fn kb_root(&self) -> &Path {
        &self.kb_root
    }
    pub fn store(&self) -> &Arc<KbStore> {
        &self.store
    }

    /// Process one queued KB job (chunk + embed + index). Returns whether a
    /// job was claimed. Used by the background worker and tests.
    pub fn drain_once(&self) -> anyhow::Result<bool> {
        let ctx = HandlerCtx {
            store: self.store.clone(),
            paths: self.paths.clone(),
            embedder: self.embedder.clone(),
            index: self.index.clone(),
        };
        WorkerPool::run_one_blocking(&ctx, &WorkerConfig::default(), &DefaultDispatcher)
    }

    /// Requeue jobs whose claiming worker died mid-flight (claim TTL
    /// expired). Run periodically by the background worker so a crash or
    /// restart can't strand a `Running` job — and its dedupe key —
    /// forever. Returns how many jobs were reclaimed.
    pub fn reclaim_stale(&self) -> anyhow::Result<usize> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let max_attempts = WorkerConfig::default().max_attempts;
        let wtx = self.store.begin_write()?;
        let n = crate::kb::store::jobs::reclaim_stale(&wtx, now_ms, max_attempts)?.len();
        wtx.commit()?;
        Ok(n)
    }

    /// Spawn a background thread that drains KB jobs forever, so document
    /// indexing runs asynchronously after upload. Idempotent per service
    /// instance is the caller's responsibility (call once at startup).
    pub fn spawn_worker(self: &Arc<Self>) {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("kb-knowledge-worker".into())
            .spawn(move || {
                // Seed with already-ready docs so we don't replay a burst of
                // status events for the existing corpus on startup.
                let mut emitted: HashSet<String> = this.ready_doc_ids().unwrap_or_default();
                // Recover jobs stranded by a crash/restart mid-claim. The
                // claim TTL is 60s; check a bit more often than that.
                let reclaim_every = Duration::from_secs(30);
                let mut next_reclaim = std::time::Instant::now() + reclaim_every;
                loop {
                    if std::time::Instant::now() >= next_reclaim {
                        match this.reclaim_stale() {
                            Ok(n) if n > 0 => {
                                tracing::info!("kb knowledge worker: reclaimed {n} stale jobs")
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("kb knowledge worker reclaim: {e:#}"),
                        }
                        next_reclaim = std::time::Instant::now() + reclaim_every;
                    }
                    match this.drain_once() {
                        // A job finished — emit status_changed=ready for any doc
                        // that just gained its first indexed chunk.
                        Ok(true) => this.emit_ready_transitions(&mut emitted),
                        Ok(false) => std::thread::sleep(Duration::from_millis(500)),
                        Err(e) => {
                            tracing::warn!("kb knowledge worker: {e:#}");
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    }
                }
            })
            .expect("spawn kb knowledge worker thread");
    }

    /// Ingest a document into a collection. Stores + tags it and enqueues the
    /// embed job (drained by the background worker). Returns `(doc_id, noop)`;
    /// `noop` means identical content was already present. `mime` overrides
    /// MIME detection (the JSON upload path passes it explicitly).
    pub fn ingest(
        &self,
        collection_id: &str,
        title: &str,
        bytes: &[u8],
        mime: Option<&str>,
    ) -> KResult<(String, bool)> {
        self.get_collection(collection_id)?; // 404 if the collection is gone
        let detected = mime
            .map(|m| m.to_string())
            .unwrap_or_else(|| detect_mime(bytes, Some(title)));
        let ext = title.rsplit('.').next().filter(|e| *e != title).unwrap_or("");
        let mut canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: &detected,
            hint_title: Some(title),
            logical_source_id_seed: None,
        })?
        .ok_or_else(|| {
            KnowledgeError::Internal(anyhow::anyhow!("unsupported or empty content (mime={detected})"))
        })?;
        canon.metadata.tags.push(collection_tag(collection_id));
        let lsid = canon.metadata.logical_source_id.as_str().to_string();
        let out = ingest_canonicalized(
            &self.store,
            IngestInput {
                canon: &canon,
                raw_bytes: bytes,
                raw_ext: ext,
                visibility: None,
                owner_user_id: None,
                seen_key: Some(("knowledge", &lsid)),
                source: None,
                paths: &self.paths,
            },
        )?;
        Ok((out.doc_id, out.noop))
    }

    /// Active documents in a collection, newest first, with chunk counts.
    pub fn list_docs(&self, collection_id: &str) -> KResult<Vec<DocInfo>> {
        self.get_collection(collection_id)?;
        let tag = collection_tag(collection_id);
        let rtx = self.store.begin_read()?;
        let counts = self.chunk_counts(&rtx)?;
        let docs = self.collect_active_docs(&rtx, &tag)?;
        let mut out: Vec<DocInfo> = docs.into_iter().map(|d| self.doc_info(d, &counts)).collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    fn collect_active_docs(
        &self,
        rtx: &redb::ReadTransaction,
        tag: &str,
    ) -> anyhow::Result<Vec<KbDoc>> {
        let mut out = Vec::new();
        let tbl = rtx.open_table(KB_DOCS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let d: KbDoc = decode(v.value())?;
            if d.status == KbStatus::Active && d.tags.iter().any(|t| t == tag) {
                out.push(d);
            }
        }
        Ok(out)
    }

    pub fn get_doc(&self, collection_id: &str, doc_id: &str) -> KResult<DocInfo> {
        let rtx = self.store.begin_read()?;
        let d = self.active_doc_in_collection(&rtx, collection_id, doc_id)?;
        let counts = self.chunk_counts(&rtx)?;
        Ok(self.doc_info(d, &counts))
    }

    /// `(mime, body)` of a document's canonical markdown, for display/editing.
    pub fn doc_content(&self, collection_id: &str, doc_id: &str) -> KResult<(String, String)> {
        let rtx = self.store.begin_read()?;
        let d = self.active_doc_in_collection(&rtx, collection_id, doc_id)?;
        drop(rtx);
        let abs = self.paths.root.join(&d.markdown_path);
        let body = read_doc_body(&abs)?;
        Ok((d.mime, body))
    }

    /// Re-enqueue a chunk+embed job for an existing document (e.g. after an
    /// embedder change). The background worker re-indexes it.
    pub fn reindex_doc(&self, collection_id: &str, doc_id: &str) -> KResult<()> {
        let rtx = self.store.begin_read()?;
        let d = self.active_doc_in_collection(&rtx, collection_id, doc_id)?;
        drop(rtx);
        let job = Job::new(JobKind::ChunkAndEmbed {
            doc_id: d.id.clone(),
            doc_version: d.version,
        });
        let wtx = self.store.begin_write()?;
        crate::kb::store::jobs::enqueue(&wtx, &job)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Tombstone a document; the compactor reaps its chunks/vectors later.
    pub fn delete_doc(&self, collection_id: &str, doc_id: &str) -> KResult<()> {
        let rtx = self.store.begin_read()?;
        let mut d = self.active_doc_in_collection(&rtx, collection_id, doc_id)?;
        drop(rtx);
        d.status = KbStatus::Tombstoned;
        let wtx = self.store.begin_write()?;
        docs::put(&wtx, &d)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Semantic search over one or more collections (empty = all). Hits below
    /// `score_threshold` are dropped. Reuses the KB hybrid search pipeline.
    pub fn search(
        &self,
        query: &str,
        collection_ids: &[String],
        top_k: usize,
        score_threshold: f32,
    ) -> KResult<Vec<SearchHit>> {
        let ctx = self.search_ctx();
        let tags: Vec<String> = collection_ids.iter().map(|id| collection_tag(id)).collect();
        let out = kb_search::run(
            &ctx,
            KbSearchInput {
                query: query.to_string(),
                k: top_k,
                filter: KbSearchFilter { tags, ..Default::default() },
                mode: "hybrid".into(),
                diversity: "mmr".into(),
                mmr_lambda: 0.5,
                boost_entities: vec![],
                query_instruction: self.query_instruction.clone(),
            },
            &CallerScope::default(),
        )?;
        let rtx = self.store.begin_read()?;
        let names: HashMap<String, String> = collections::list(&rtx)?
            .into_iter()
            .map(|c| (c.id, c.name))
            .collect();
        let mut hits = Vec::new();
        for r in out.results {
            if r.score < score_threshold {
                continue;
            }
            let cid = docs::get(&rtx, &r.doc_id)?.and_then(|d| {
                d.tags
                    .iter()
                    .find_map(|t| t.strip_prefix(COLLECTION_TAG_PREFIX).map(|s| s.to_string()))
            });
            let cname = cid.as_ref().and_then(|id| names.get(id).cloned());
            hits.push(SearchHit {
                doc_id: r.doc_id,
                collection_id: cid,
                collection_name: cname,
                source_title: r.doc_title,
                chunk_text: r.text,
                score: r.score,
            });
        }
        Ok(hits)
    }

    /// Aggregate stats across all collections/docs/chunks.
    pub fn stats(&self) -> KResult<KbStats> {
        let rtx = self.store.begin_read()?;
        let collection_count = collections::list(&rtx)?.len();
        let chunk_count: usize = self.chunk_counts(&rtx)?.values().sum();
        let docs = self.all_active_docs(&rtx)?;
        let bytes: u64 = docs
            .iter()
            .map(|d| {
                std::fs::metadata(self.paths.root.join(&d.markdown_path))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        Ok(KbStats {
            collection_count,
            doc_count: docs.len(),
            chunk_count,
            bytes,
        })
    }

    /// Embedders the UI can offer. The active one is marked default; local BGE
    /// models count as "downloaded" when present under `<base>/models/`.
    pub fn embedders(&self) -> Vec<EmbedderInfo> {
        let base = self.kb_root.parent().unwrap_or(&self.kb_root);
        let models = base.join("models");
        let active = self.embedder.embedder_id().to_string();
        [
            ("bge-small-zh", "BGE-Small-ZH", 512usize),
            ("bge-base-zh", "BGE-Base-ZH", 768),
            ("bge-small-en", "BGE-Small-EN", 384),
            ("Qwen3-Embedding-0.6B", "Qwen3-Embedding-0.6B (remote)", 1024),
        ]
        .iter()
        .map(|(id, label, dim)| EmbedderInfo {
            id: id.to_string(),
            label: label.to_string(),
            dim: *dim,
            downloaded: models.join(id).join("model.safetensors").exists(),
            is_default: active.contains(id),
        })
        .collect()
    }

    fn search_ctx(&self) -> SearchCtx {
        SearchCtx {
            store: self.store.clone(),
            index: self.index.clone(),
            paths: self.paths.clone(),
            embedder: self.embedder.clone(),
        }
    }

    fn all_active_docs(&self, rtx: &redb::ReadTransaction) -> anyhow::Result<Vec<KbDoc>> {
        let mut out = Vec::new();
        let tbl = rtx.open_table(KB_DOCS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let d: KbDoc = decode(v.value())?;
            if d.status == KbStatus::Active {
                out.push(d);
            }
        }
        Ok(out)
    }

    /// Fetch a doc that is Active AND tagged into the given collection, else
    /// `DocNotFound` (also hides docs that belong to a different collection).
    fn active_doc_in_collection(
        &self,
        rtx: &redb::ReadTransaction,
        collection_id: &str,
        doc_id: &str,
    ) -> KResult<KbDoc> {
        let tag = collection_tag(collection_id);
        docs::get(rtx, doc_id)?
            .filter(|d| d.status == KbStatus::Active && d.tags.iter().any(|t| t == &tag))
            .ok_or(KnowledgeError::DocNotFound)
    }

    fn chunk_counts(&self, rtx: &redb::ReadTransaction) -> anyhow::Result<HashMap<String, usize>> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let tbl = rtx.open_table(KB_CHUNKS)?;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let c: KbChunk = decode(v.value())?;
            if c.status == ChunkStatus::Active {
                *counts.entry(c.doc_id).or_default() += 1;
            }
        }
        Ok(counts)
    }

    fn doc_info(&self, d: KbDoc, counts: &HashMap<String, usize>) -> DocInfo {
        let bytes = std::fs::metadata(self.paths.root.join(&d.markdown_path))
            .map(|m| m.len())
            .unwrap_or(0);
        DocInfo {
            chunk_count: counts.get(&d.id).copied().unwrap_or(0),
            id: d.id,
            title: d.title,
            mime: d.mime,
            bytes,
            created_at: d.created_at,
            updated_at: d.updated_at,
        }
    }

    /// All collections, newest first.
    pub fn list_collections(&self) -> KResult<Vec<KbCollection>> {
        let rtx = self.store.begin_read()?;
        let mut v = collections::list(&rtx)?;
        v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(v)
    }

    pub fn get_collection(&self, id: &str) -> KResult<KbCollection> {
        let rtx = self.store.begin_read()?;
        collections::get(&rtx, id)?.ok_or(KnowledgeError::CollectionNotFound)
    }

    pub fn create_collection(
        &self,
        name: &str,
        description: Option<String>,
        embed_model: Option<String>,
    ) -> KResult<KbCollection> {
        {
            let rtx = self.store.begin_read()?;
            if collections::find_by_name(&rtx, name)?.is_some() {
                return Err(KnowledgeError::DuplicateName);
            }
        }
        let now = now_ms();
        let c = KbCollection {
            id: format!("col_{}", ulid::Ulid::new().to_string().to_lowercase()),
            name: name.to_string(),
            description,
            embed_model,
            created_at: now,
            updated_at: now,
        };
        let wtx = self.store.begin_write()?;
        collections::put(&wtx, &c)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(c)
    }

    /// Update name/description. `description = None` leaves it unchanged;
    /// `Some(None)` clears it; `Some(Some(x))` sets it. Renames keep the
    /// name index consistent and reject collisions.
    pub fn update_collection(
        &self,
        id: &str,
        name: Option<String>,
        description: Option<Option<String>>,
    ) -> KResult<KbCollection> {
        let mut c = self.get_collection(id)?;
        let old_name = c.name.clone();
        if let Some(new_name) = name {
            if new_name != c.name {
                let rtx = self.store.begin_read()?;
                if let Some(other) = collections::find_by_name(&rtx, &new_name)? {
                    if other != id {
                        return Err(KnowledgeError::DuplicateName);
                    }
                }
                c.name = new_name;
            }
        }
        if let Some(desc) = description {
            c.description = desc;
        }
        c.updated_at = now_ms();
        let wtx = self.store.begin_write()?;
        if c.name != old_name {
            collections::unindex_name(&wtx, &old_name)?;
        }
        collections::put(&wtx, &c)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(c)
    }

    /// Delete a collection and tombstone all of its documents (the compactor
    /// reaps their chunks/vectors later). Returns the number of docs removed.
    pub fn delete_collection(&self, id: &str) -> KResult<usize> {
        self.get_collection(id)?; // 404 if absent
        let tag = collection_tag(id);
        let rtx = self.store.begin_read()?;
        let docs_to_remove = self.collect_active_docs(&rtx, &tag)?;
        drop(rtx);
        let wtx = self.store.begin_write()?;
        for mut d in docs_to_remove.iter().cloned() {
            d.status = KbStatus::Tombstoned;
            docs::put(&wtx, &d)?;
        }
        collections::delete(&wtx, id)?;
        wtx.commit().map_err(anyhow::Error::from)?;
        Ok(docs_to_remove.len())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn svc() -> (TempDir, KnowledgeService) {
        let tmp = TempDir::new().unwrap();
        let s = KnowledgeService::open(tmp.path().join("kb")).unwrap();
        (tmp, s)
    }

    #[test]
    fn create_then_list_and_get() {
        let (_t, s) = svc();
        let c = s.create_collection("产品手册", Some("v3".into()), None).unwrap();
        assert!(c.id.starts_with("col_"));
        assert_eq!(s.get_collection(&c.id).unwrap().name, "产品手册");
        assert_eq!(s.list_collections().unwrap().len(), 1);
    }

    #[test]
    fn duplicate_name_rejected() {
        let (_t, s) = svc();
        s.create_collection("产品手册", None, None).unwrap();
        assert!(matches!(
            s.create_collection("产品手册", None, None),
            Err(KnowledgeError::DuplicateName)
        ));
    }

    #[test]
    fn rename_frees_old_name() {
        let (_t, s) = svc();
        let c = s.create_collection("旧名", None, None).unwrap();
        s.update_collection(&c.id, Some("新名".into()), None).unwrap();
        // old name is now reusable
        s.create_collection("旧名", None, None).unwrap();
        assert_eq!(s.get_collection(&c.id).unwrap().name, "新名");
    }

    #[test]
    fn ingest_enqueues_and_worker_drains() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let (doc_id, noop) = s
            .ingest(
                &c.id,
                "note.md",
                b"# Title\n\nquantum entanglement is a phenomenon of two particles.",
                Some("text/markdown"),
            )
            .unwrap();
        assert!(!noop);
        assert!(doc_id.starts_with("doc_") || !doc_id.is_empty());
        // Drain the enqueued embed job(s).
        let mut drained = 0;
        while s.drain_once().unwrap() {
            drained += 1;
            if drained > 50 {
                break;
            }
        }
        assert!(drained >= 1, "expected the embed job to be drained");
    }

    #[test]
    fn doc_lifecycle_list_content_delete() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let (doc_id, _) = s
            .ingest(
                &c.id,
                "note.md",
                b"# Title\n\nhello world body text for the knowledge base.",
                Some("text/markdown"),
            )
            .unwrap();
        while s.drain_once().unwrap() {}
        let listed = s.list_docs(&c.id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, doc_id);
        assert_eq!(listed[0].status(), "ready");
        let (mime, body) = s.doc_content(&c.id, &doc_id).unwrap();
        assert_eq!(mime, "text/markdown");
        assert!(body.contains("hello world"), "body was: {body}");
        s.delete_doc(&c.id, &doc_id).unwrap();
        assert!(s.list_docs(&c.id).unwrap().is_empty());
        assert!(matches!(
            s.get_doc(&c.id, &doc_id),
            Err(KnowledgeError::DocNotFound)
        ));
    }

    #[test]
    fn search_and_stats_after_ingest() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        s.ingest(&c.id, "a.md", b"# A\n\nquantum entanglement links two particles.", Some("text/markdown")).unwrap();
        s.ingest(&c.id, "b.md", b"# B\n\nthe capital of France is Paris.", Some("text/markdown")).unwrap();
        while s.drain_once().unwrap() {}
        // BM25 side of the hybrid search matches the lexical term regardless of
        // the (stub) embedder, so the doc is findable.
        let hits = s.search("two particles", std::slice::from_ref(&c.id), 5, 0.0).unwrap();
        assert!(!hits.is_empty(), "expected a hit for 'two particles'");
        assert_eq!(hits[0].collection_id.as_deref(), Some(c.id.as_str()));
        let st = s.stats().unwrap();
        assert_eq!(st.collection_count, 1);
        assert_eq!(st.doc_count, 2);
        assert!(st.chunk_count >= 2);
    }

    #[test]
    fn sse_emits_status_changed_when_doc_becomes_ready() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let mut rx = s.subscribe();
        let (doc_id, _) = s
            .ingest(&c.id, "a.md", b"# A\n\nhello indexed body", Some("text/markdown"))
            .unwrap();
        while s.drain_once().unwrap() {}
        let mut emitted = HashSet::new();
        s.emit_ready_transitions(&mut emitted);
        let msg = rx.try_recv().expect("expected an SSE status event");
        assert!(msg.contains("knowledge.doc.status_changed"), "got: {msg}");
        assert!(msg.contains(&doc_id), "got: {msg}");
        // idempotent: no duplicate for the same doc
        s.emit_ready_transitions(&mut emitted);
        assert!(rx.try_recv().is_err(), "should not re-emit for the same doc");
    }

    #[test]
    fn delete_collection_cascades_docs() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        s.ingest(&c.id, "a.md", b"# A\n\nbody one here", Some("text/markdown")).unwrap();
        s.ingest(&c.id, "b.md", b"# B\n\nbody two here", Some("text/markdown")).unwrap();
        while s.drain_once().unwrap() {}
        let removed = s.delete_collection(&c.id).unwrap();
        assert_eq!(removed, 2);
        assert!(matches!(
            s.get_collection(&c.id),
            Err(KnowledgeError::CollectionNotFound)
        ));
    }

    #[test]
    fn reindex_enqueues_drainable_job() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let (doc_id, _) = s
            .ingest(&c.id, "a.md", b"# A\n\nhello particles here", Some("text/markdown"))
            .unwrap();
        while s.drain_once().unwrap() {}
        s.reindex_doc(&c.id, &doc_id).unwrap();
        assert!(
            s.drain_once().unwrap(),
            "reindex should enqueue a drainable job"
        );
    }

    #[test]
    fn ingest_into_missing_collection_404s() {
        let (_t, s) = svc();
        assert!(matches!(
            s.ingest("col_nope", "x.md", b"hi", Some("text/markdown")),
            Err(KnowledgeError::CollectionNotFound)
        ));
    }

    #[test]
    fn delete_then_missing() {
        let (_t, s) = svc();
        let c = s.create_collection("x", None, None).unwrap();
        s.delete_collection(&c.id).unwrap();
        assert!(matches!(
            s.get_collection(&c.id),
            Err(KnowledgeError::CollectionNotFound)
        ));
        assert!(matches!(
            s.delete_collection(&c.id),
            Err(KnowledgeError::CollectionNotFound)
        ));
    }
}
