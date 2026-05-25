//! `KnowledgeService` — gateway-facing facade over the KB store for the
//! desktop `/api/v1/knowledge` API.
//!
//! Collections are a tag veneer over the single KB store (see the project
//! note `kb-desktop-collections`): a collection is metadata here plus a
//! `collection:<id>` tag on its docs. There is no per-collection store or
//! embedder. P1 covers collection metadata CRUD; docs + search grow the
//! service in P2/P3.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use redb::ReadableTable;
use tokio::sync::broadcast;

use crate::kb::{
    KbEmbedder, KbIndex, KbPaths, KbStore,
    canonicalize::{CanonicalizeInput, canonicalize_by_mime, detect_mime},
    content_store::read::read_doc_body,
    embedder::resolve_embedder,
    jobs::{Job, JobKind, JobStatus},
    model::{
        COLLECTION_TAG_PREFIX, CallerScope, ChunkStatus, KbChunk, KbCollection, KbDoc, KbStatus,
        collection_tag,
    },
    pipeline::{IngestInput, ingest_canonicalized},
    search::SearchCtx,
    store::{
        codec::decode,
        collections, docs,
        schema::{KB_CHUNKS, KB_DOCS},
    },
    tools::kb_search::{self, KbSearchFilter, KbSearchInput},
    worker::{DefaultDispatcher, HandlerCtx, WorkerConfig, WorkerPool},
};

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
/// is `ready` once its chunks are embedded+indexed; `failed` if its indexing
/// job exhausted retries with no chunks produced; else still `indexing`.
pub struct DocInfo {
    pub id: String,
    pub title: String,
    pub mime: String,
    pub bytes: u64,
    pub chunk_count: usize,
    /// The current-version ChunkAndEmbed job is in `Failed` state and no
    /// chunks were produced — indexing permanently failed for this doc.
    pub failed: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl DocInfo {
    /// `ready` (has chunks) > `failed` (index job exhausted, no chunks) >
    /// `indexing` (still in flight).
    pub fn status(&self) -> &'static str {
        if self.chunk_count > 0 {
            "ready"
        } else if self.failed {
            "failed"
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
    /// Canonicalized allowed roots for the loopback-only `/docs/from-path`
    /// ingest endpoint (`kb.allowedUploadRoots`, default ~/Documents,
    /// ~/Downloads, ~/Desktop). A from-path target must live under one of
    /// these.
    allowed_upload_roots: Vec<PathBuf>,
}

/// Default max upload size when `kb.maxDocMb` is unset (50 MB).
const DEFAULT_MAX_DOC_BYTES: usize = 50 * 1024 * 1024;

/// Resolve `kb.allowedUploadRoots` into canonical absolute paths. Leading `~`
/// expands to the home dir; unset/empty falls back to ~/Documents, ~/Downloads,
/// ~/Desktop. Roots that don't exist are dropped (canonicalize fails) — an
/// empty result simply means every from-path request is rejected (fail-closed).
fn resolve_upload_roots(configured: Option<&Vec<String>>) -> Vec<PathBuf> {
    let home = dirs_next::home_dir();
    let raw: Vec<PathBuf> = match configured {
        Some(list) if !list.is_empty() => list
            .iter()
            .map(|s| match s.strip_prefix("~") {
                Some(rest) if home.is_some() => {
                    home.clone().unwrap().join(rest.trim_start_matches('/'))
                }
                _ => PathBuf::from(s),
            })
            .collect(),
        _ => home
            .iter()
            .flat_map(|h| {
                ["Documents", "Downloads", "Desktop"]
                    .iter()
                    .map(move |d| h.join(d))
            })
            .collect(),
    };
    raw.into_iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect()
}

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
        // queryInstruction comes from the SAME effective embed config the
        // embedder was resolved from (`kb.embed` override, else `memorySearch`),
        // so a KB-specific asymmetric model uses its own instruction.
        let query_instruction =
            crate::kb::embedder::effective_embed_config().and_then(|m| m.query_instruction);
        // kb.maxDocMb → bytes; default 50 MB, clamp negatives/zero to default.
        let max_doc_bytes = cfg
            .as_ref()
            .and_then(|c| c.raw.kb.as_ref())
            .and_then(|k| k.max_doc_mb)
            .filter(|mb| *mb > 0)
            .map(|mb| mb as usize * 1024 * 1024)
            .unwrap_or(DEFAULT_MAX_DOC_BYTES);
        let allowed_upload_roots = resolve_upload_roots(
            cfg.as_ref()
                .and_then(|c| c.raw.kb.as_ref())
                .and_then(|k| k.allowed_upload_roots.as_ref()),
        );
        Ok(Self {
            store,
            paths,
            index,
            embedder,
            kb_root,
            events,
            query_instruction,
            max_doc_bytes,
            allowed_upload_roots,
        })
    }

    /// Max accepted upload size in bytes (`kb.maxDocMb`, default 50 MB).
    pub fn max_doc_bytes(&self) -> usize {
        self.max_doc_bytes
    }

    /// Canonicalized allowed roots for `/docs/from-path` (see
    /// `kb.allowedUploadRoots`). A from-path target must live under one of
    /// these.
    pub fn allowed_upload_roots(&self) -> &[PathBuf] {
        &self.allowed_upload_roots
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
    ///
    /// The loop body is wrapped in `catch_unwind` so a panic in any
    /// non-essential call (reclaim_stale, emit_ready_transitions) does not
    /// kill the thread permanently — the inner state is reset and the loop
    /// restarts after a backoff.
    pub fn spawn_worker(self: &Arc<Self>) {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("kb-knowledge-worker".into())
            .spawn(move || {
                // Outer recovery loop: if the inner loop panics, restart it
                // with fresh state after a backoff.
                loop {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Self::worker_inner(&this);
                    }));
                    match outcome {
                        Ok(()) => {
                            // Normal exit should never happen; log and restart.
                            tracing::error!(
                                "kb knowledge worker: inner loop exited unexpectedly; restarting"
                            );
                            std::thread::sleep(Duration::from_secs(2));
                        }
                        Err(panic) => {
                            let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                                s.to_string()
                            } else if let Some(s) = panic.downcast_ref::<String>() {
                                s.clone()
                            } else {
                                "unknown panic".to_string()
                            };
                            tracing::error!(
                                "kb knowledge worker: thread panicked ({msg}); restarting in 5s"
                            );
                            std::thread::sleep(Duration::from_secs(5));
                        }
                    }
                }
            })
            .expect("spawn kb knowledge worker thread");
    }

    /// Inner worker loop. Extracted as a plain fn so `spawn_worker`'s
    /// `catch_unwind` boundary is clear — no mutable captures.
    fn worker_inner(this: &Arc<Self>) {
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
    }

    /// Ingest a document into a collection. Stores + tags it, enqueues the
    /// embed job, and drains it synchronously so the caller can search
    /// immediately after. Returns `(doc_id, noop)`; `noop` means identical
    /// content was already present. `mime` overrides MIME detection (the
    /// JSON upload path passes it explicitly).
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
        let ext = title
            .rsplit('.')
            .next()
            .filter(|e| *e != title)
            .unwrap_or("");
        let mut canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: &detected,
            hint_title: Some(title),
            logical_source_id_seed: None,
        })?
        .ok_or_else(|| {
            KnowledgeError::Internal(anyhow::anyhow!(
                "unsupported or empty content (mime={detected})"
            ))
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
        if !out.noop {
            // Drain the ChunkAndEmbed job synchronously so a subsequent
            // search (agent turn, REST API call) finds the chunks already
            // in HNSW + Tantivy. Multiple callers (agent thread, bg
            // worker) may race on claim_next, but the first to claim
            // processes it; the other sees "no job" and returns.
            let _ = self.drain_once();
        }
        Ok((out.doc_id, out.noop))
    }

    /// Ingest a document by fetching a URL server-side, tagged into the
    /// collection. Delegates to the KB `UrlSyncer` (GET → canonicalize →
    /// ingest → enqueue embed), which records `KbSource::Url` provenance and
    /// dedupes via ETag/content-hash. The caller is responsible for the
    /// collection-existence check and SSRF validation of `url`.
    pub async fn ingest_url(
        &self,
        collection_id: &str,
        url: &str,
    ) -> Result<crate::kb::sync::SyncOutcome, crate::kb::sync::SyncError> {
        use crate::kb::sync::{KbSourceSyncer, SyncContext, SyncReason, UrlSyncer};
        let syncer = UrlSyncer {
            url: url.to_string(),
            tags: vec![collection_tag(collection_id)],
        };
        let ctx = SyncContext {
            store: self.store.clone(),
            paths: self.paths.clone(),
            index: self.index.clone(),
            embedder: self.embedder.clone(),
        };
        syncer.sync(&ctx, SyncReason::Manual).await
    }

    /// Active documents in a collection, newest first, with chunk counts.
    pub fn list_docs(&self, collection_id: &str) -> KResult<Vec<DocInfo>> {
        self.get_collection(collection_id)?;
        let tag = collection_tag(collection_id);
        let rtx = self.store.begin_read()?;
        let counts = self.chunk_counts(&rtx)?;
        let failed_jobs = self.failed_index_jobs(&rtx)?;
        let docs = self.collect_active_docs(&rtx, &tag)?;
        let mut out: Vec<DocInfo> = docs
            .into_iter()
            .map(|d| self.doc_info(d, &counts, &failed_jobs))
            .collect();
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
        let failed_jobs = self.failed_index_jobs(&rtx)?;
        Ok(self.doc_info(d, &counts, &failed_jobs))
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
                filter: KbSearchFilter {
                    tags,
                    ..Default::default()
                },
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
        let docs = self.all_active_docs(&rtx)?;
        // Count only chunks whose parent doc is still active. `delete_doc`
        // tombstones the doc but leaves its chunks Active until the compactor
        // reaps them, so summing all active chunks would report chunk_count > 0
        // while doc_count == 0 during that window.
        let active_ids: HashSet<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        let chunk_count: usize = self
            .chunk_counts(&rtx)?
            .iter()
            .filter(|(doc_id, _)| active_ids.contains(doc_id.as_str()))
            .map(|(_, n)| *n)
            .sum();
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
            (
                "Qwen3-Embedding-0.6B",
                "Qwen3-Embedding-0.6B (remote)",
                1024,
            ),
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

    /// `(doc_id, doc_version)` of every `ChunkAndEmbed` job currently in
    /// `Failed` state — i.e. indexing that exhausted its retries. Used to
    /// surface a `failed` doc status instead of an indefinite `indexing`.
    fn failed_index_jobs(
        &self,
        rtx: &redb::ReadTransaction,
    ) -> anyhow::Result<HashSet<(String, u32)>> {
        let mut out = HashSet::new();
        for job in crate::kb::store::jobs::list_by_status(rtx, JobStatus::Failed)? {
            if let JobKind::ChunkAndEmbed {
                doc_id,
                doc_version,
            } = job.kind
            {
                out.insert((doc_id, doc_version));
            }
        }
        Ok(out)
    }

    fn doc_info(
        &self,
        d: KbDoc,
        counts: &HashMap<String, usize>,
        failed_jobs: &HashSet<(String, u32)>,
    ) -> DocInfo {
        let bytes = std::fs::metadata(self.paths.root.join(&d.markdown_path))
            .map(|m| m.len())
            .unwrap_or(0);
        let chunk_count = counts.get(&d.id).copied().unwrap_or(0);
        // Only treat as failed when no chunks landed AND this doc's current
        // version has a Failed indexing job (a stale failed job for an older
        // version must not mask a freshly re-ingested, still-indexing doc).
        let failed = chunk_count == 0 && failed_jobs.contains(&(d.id.clone(), d.version));
        DocInfo {
            chunk_count,
            failed,
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
    use tempfile::TempDir;

    use super::*;

    fn svc() -> (TempDir, KnowledgeService) {
        let tmp = TempDir::new().unwrap();
        let s = KnowledgeService::open(tmp.path().join("kb")).unwrap();
        (tmp, s)
    }

    #[test]
    fn create_then_list_and_get() {
        let (_t, s) = svc();
        let c = s
            .create_collection("产品手册", Some("v3".into()), None)
            .unwrap();
        assert!(c.id.starts_with("col_"));
        assert_eq!(s.get_collection(&c.id).unwrap().name, "产品手册");
        assert_eq!(s.list_collections().unwrap().len(), 1);
    }

    #[test]
    fn doc_status_failed_when_index_job_exhausted() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();

        // Use ingest_canonicalized directly so the job stays pending
        // (s.ingest() now drains synchronously).
        use crate::kb::{
            canonicalize::{CanonicalizeInput, canonicalize_by_mime},
            paths::KbPaths,
            pipeline::{IngestInput, ingest_canonicalized},
        };
        let mut canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"# A\n\nbody one here",
            mime: "text/markdown",
            hint_title: Some("a.md"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        canon
            .metadata
            .tags
            .push(crate::kb::model::collection_tag(&c.id));
        let paths = KbPaths::new(s.kb_root().to_path_buf());
        let out = ingest_canonicalized(
            s.store(),
            IngestInput {
                canon: &canon,
                raw_bytes: b"# A\n\nbody one here",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        let doc_id = out.doc_id;

        // Before the worker runs: still indexing.
        assert_eq!(s.get_doc(&c.id, &doc_id).unwrap().status(), "indexing");
        // Simulate the worker giving up: claim the enqueued ChunkAndEmbed job
        // and mark it Failed (what run_one_blocking does at max_attempts).
        {
            use crate::kb::store::jobs;
            let now = chrono::Utc::now().timestamp_millis();
            let wtx = s.store().begin_write().unwrap();
            let (job, token) = jobs::claim_next(&wtx, "test", now, 60_000)
                .unwrap()
                .unwrap();
            jobs::mark_failed(&wtx, &job.id, &token.token, "boom").unwrap();
            wtx.commit().unwrap();
        }
        // Now surfaced as failed (no chunks + Failed job for this version).
        assert_eq!(s.get_doc(&c.id, &doc_id).unwrap().status(), "failed");
        let docs = s.list_docs(&c.id).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].status(), "failed");
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
        s.update_collection(&c.id, Some("新名".into()), None)
            .unwrap();
        // old name is now reusable
        s.create_collection("旧名", None, None).unwrap();
        assert_eq!(s.get_collection(&c.id).unwrap().name, "新名");
    }

    #[test]
    fn ingest_enqueues_and_worker_drains() {
        let (_t, s) = svc();

        // Use ingest_canonicalized directly (s.ingest() now drains
        // synchronously) so we can verify drain_once processes the job.
        use crate::kb::{
            canonicalize::{CanonicalizeInput, canonicalize_by_mime},
            paths::KbPaths,
            pipeline::{IngestInput, ingest_canonicalized},
        };
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"# Title\n\nquantum entanglement is a phenomenon of two particles.",
            mime: "text/markdown",
            hint_title: Some("note.md"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        let paths = KbPaths::new(s.kb_root().to_path_buf());
        let out = ingest_canonicalized(
            s.store(),
            IngestInput {
                canon: &canon,
                raw_bytes: b"# Title\n\nquantum entanglement is a phenomenon of two particles.",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        assert!(!out.noop);

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
        s.ingest(
            &c.id,
            "a.md",
            b"# A\n\nquantum entanglement links two particles.",
            Some("text/markdown"),
        )
        .unwrap();
        s.ingest(
            &c.id,
            "b.md",
            b"# B\n\nthe capital of France is Paris.",
            Some("text/markdown"),
        )
        .unwrap();
        while s.drain_once().unwrap() {}
        // BM25 side of the hybrid search matches the lexical term regardless of
        // the (stub) embedder, so the doc is findable.
        let hits = s
            .search("two particles", std::slice::from_ref(&c.id), 5, 0.0)
            .unwrap();
        assert!(!hits.is_empty(), "expected a hit for 'two particles'");
        assert_eq!(hits[0].collection_id.as_deref(), Some(c.id.as_str()));
        let st = s.stats().unwrap();
        assert_eq!(st.collection_count, 1);
        assert_eq!(st.doc_count, 2);
        assert!(st.chunk_count >= 2);
    }

    #[test]
    fn stats_excludes_tombstoned_doc_chunks() {
        // `delete_doc` tombstones the doc but leaves its chunks Active until
        // the compactor reaps them. `stats()` must not report those orphan
        // chunks, or a fully-emptied KB shows chunk_count > 0 / doc_count == 0.
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let (doc_id, _) = s
            .ingest(
                &c.id,
                "a.md",
                b"# A\n\nquantum entanglement links two particles.",
                Some("text/markdown"),
            )
            .unwrap();
        while s.drain_once().unwrap() {}
        let before = s.stats().unwrap();
        assert_eq!(before.doc_count, 1);
        assert!(before.chunk_count >= 1);
        // Tombstone the only doc; chunks remain Active in redb (compactor lag).
        s.delete_doc(&c.id, &doc_id).unwrap();
        let after = s.stats().unwrap();
        assert_eq!(after.doc_count, 0);
        assert_eq!(
            after.chunk_count, 0,
            "tombstoned doc's chunks must not count"
        );
    }

    #[test]
    fn sse_emits_status_changed_when_doc_becomes_ready() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        let mut rx = s.subscribe();
        let (doc_id, _) = s
            .ingest(
                &c.id,
                "a.md",
                b"# A\n\nhello indexed body",
                Some("text/markdown"),
            )
            .unwrap();
        while s.drain_once().unwrap() {}
        let mut emitted = HashSet::new();
        s.emit_ready_transitions(&mut emitted);
        let msg = rx.try_recv().expect("expected an SSE status event");
        assert!(msg.contains("knowledge.doc.status_changed"), "got: {msg}");
        assert!(msg.contains(&doc_id), "got: {msg}");
        // idempotent: no duplicate for the same doc
        s.emit_ready_transitions(&mut emitted);
        assert!(
            rx.try_recv().is_err(),
            "should not re-emit for the same doc"
        );
    }

    #[test]
    fn delete_collection_cascades_docs() {
        let (_t, s) = svc();
        let c = s.create_collection("kb", None, None).unwrap();
        s.ingest(
            &c.id,
            "a.md",
            b"# A\n\nbody one here",
            Some("text/markdown"),
        )
        .unwrap();
        s.ingest(
            &c.id,
            "b.md",
            b"# B\n\nbody two here",
            Some("text/markdown"),
        )
        .unwrap();
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
            .ingest(
                &c.id,
                "a.md",
                b"# A\n\nhello particles here",
                Some("text/markdown"),
            )
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
