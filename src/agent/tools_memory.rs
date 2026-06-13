//! Memory and knowledge-base related built-in tools.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::{
    context_mgr::estimate_tokens,
    memory::{MemDocTier, MemoryDoc, MemoryStore},
    prompt_builder::memory_age_label,
    runtime::{AgentRuntime, RunContext},
};
use crate::provider::{RecallBundle, RecallMetadata};

impl AgentRuntime {
    /// `knowledge_base` tool — read (hybrid search) plus user-directed write
    /// (build a collection / ingest text the agent prepared). Reads the
    /// process-global KnowledgeService (opened at gateway startup; redb is
    /// exclusively locked, so we never re-open it here). Write actions run in
    /// spawn_blocking (sync redb writes).
    pub(crate) async fn tool_knowledge_base(&self, args: Value) -> Result<Value> {
        let action = args["action"].as_str().unwrap_or("search").trim();
        let Some(kb) = crate::kb::global_service() else {
            return Ok(json!({"results": [], "note": "knowledge base not available"}));
        };
        match action {
            "search" | "" => self.kb_action_search(kb, args).await,
            "list_collections" => {
                let cols = tokio::task::spawn_blocking(move || kb.list_collections())
                    .await
                    .map_err(|e| anyhow::anyhow!("kb list task failed: {e}"))?
                    .map_err(|e| anyhow::anyhow!("kb list_collections failed: {e}"))?;
                let list: Vec<Value> = cols
                    .into_iter()
                    .map(|c| json!({"id": c.id, "name": c.name, "description": c.description}))
                    .collect();
                Ok(json!({ "collections": list }))
            }
            "create_collection" => {
                let name = args["name"].as_str().unwrap_or("").trim().to_owned();
                if name.is_empty() {
                    return Ok(json!({"error": "name required for create_collection"}));
                }
                let desc = args["description"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                let (id, name, created) = tokio::task::spawn_blocking(move || {
                    resolve_or_create_collection(&kb, &name, desc)
                })
                .await
                .map_err(|e| anyhow::anyhow!("kb create task failed: {e}"))??;
                Ok(json!({ "collection_id": id, "name": name, "created": created }))
            }
            "add" => {
                let coll = args["collection"].as_str().unwrap_or("").trim().to_owned();
                let title = args["title"].as_str().unwrap_or("").trim().to_owned();
                let content = args["content"].as_str().unwrap_or("").to_owned();
                let force = args["force"].as_bool().unwrap_or(false);
                if coll.is_empty() || title.is_empty() || content.trim().is_empty() {
                    return Ok(json!({"error": "add requires collection, title, and content"}));
                }
                // Agent-authored content is markdown/plain text; default to
                // markdown (the md canonicalizer handles plain text fine).
                let mime = args["mime"]
                    .as_str()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "text/markdown".to_owned());
                let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
                    let (cid, _name, created) =
                        resolve_or_create_collection(&kb, &coll, None)?;
                    // Pre-search dedup: a brand-new collection can't have
                    // duplicates; for existing ones, probe the top hit by
                    // title against this collection and surface anything
                    // within the near-duplicate band so the LLM can decide
                    // whether to merge, replace, or `force: true`.
                    if !created && !force {
                        let probe_query = format!("{title}\n{}", content.chars().take(400).collect::<String>());
                        let hits = kb
                            .search(&probe_query, &[cid.clone()], 3, 0.0)
                            .unwrap_or_default();
                        const NEAR_DUP_THRESHOLD: f32 = 0.85;
                        if let Some(top) = hits.iter().find(|h| h.score >= NEAR_DUP_THRESHOLD) {
                            return Ok(json!({
                                "status": "near_duplicate",
                                "existing_doc_id": top.doc_id,
                                "existing_title": top.source_title,
                                "score": top.score,
                                "collection_id": cid,
                                "hint": "A semantically similar doc already exists. Re-call with `force: true` to add anyway, or `action: delete` the existing doc first.",
                            }));
                        }
                    }
                    let (doc_id, noop) = kb
                        .ingest(&cid, &title, content.as_bytes(), Some(&mime))
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok(json!({
                        "doc_id": doc_id,
                        "collection_id": cid,
                        "status": if noop { "duplicate" } else { "pending" },
                    }))
                })
                .await
                .map_err(|e| anyhow::anyhow!("kb add task failed: {e}"))??;
                Ok(out)
            }
            "delete" => {
                let coll = args["collection"].as_str().unwrap_or("").trim().to_owned();
                let doc_id = args["doc_id"].as_str().unwrap_or("").trim().to_owned();
                if coll.is_empty() || doc_id.is_empty() {
                    return Ok(json!({"error": "delete requires both `collection` and `doc_id`"}));
                }
                let out = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
                    let cols = kb
                        .list_collections()
                        .map_err(|e| anyhow::anyhow!("kb list_collections failed: {e}"))?;
                    let cid = cols
                        .iter()
                        .find(|c| c.id == coll || c.name == coll)
                        .map(|c| c.id.clone())
                        .ok_or_else(|| anyhow::anyhow!("collection not found: {coll}"))?;
                    kb.delete_doc(&cid, &doc_id)
                        .map_err(|e| anyhow::anyhow!("kb delete_doc failed: {e}"))?;
                    Ok(json!({
                        "doc_id": doc_id,
                        "collection_id": cid,
                        "status": "tombstoned",
                    }))
                })
                .await
                .map_err(|e| anyhow::anyhow!("kb delete task failed: {e}"))??;
                Ok(out)
            }
            other => Ok(json!({
                "error": format!("unknown action '{other}' (use search, add, delete, create_collection, list_collections)")
            })),
        }
    }

    /// The read path of `knowledge_base` (action=search).
    async fn kb_action_search(
        &self,
        kb: std::sync::Arc<crate::kb::KnowledgeService>,
        args: Value,
    ) -> Result<Value> {
        // Trim query defensively (v1 tool-call protocol can leak a trailing
        // newline into string args).
        let query = args["query"].as_str().unwrap_or("").trim().to_owned();
        if query.is_empty() {
            return Ok(json!({"results": [], "note": "empty query"}));
        }
        let collection_ids: Vec<String> = args["collection_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let top_k = args["top_k"].as_u64().unwrap_or(5).clamp(1, 50) as usize;

        // KnowledgeService::search is synchronous and CPU-heavy (embed + HNSW +
        // tantivy); run it off the async executor so it doesn't stall the turn.
        let hits =
            tokio::task::spawn_blocking(move || kb.search(&query, &collection_ids, top_k, 0.0))
                .await
                .map_err(|e| anyhow::anyhow!("knowledge_base search task failed: {e}"))?
                .map_err(|e| anyhow::anyhow!("knowledge_base search failed: {e}"))?;

        let results: Vec<Value> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "doc_id": h.doc_id,
                    "collection": h.collection_name,
                    "source_title": h.source_title,
                    "text": h.chunk_text,
                    "score": h.score,
                })
            })
            .collect();
        Ok(json!({
            "count": results.len(),
            "results": results,
            "note": if results.is_empty() { "no matching content in the knowledge base — do NOT fabricate a citation" } else { "" },
        }))
    }

    pub(crate) async fn tool_memory_search(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let query = args["query"].as_str().unwrap_or("").trim().to_owned();
        if query.is_empty() {
            return Ok(json!({"results": [], "note": "empty query"}));
        }
        let default_scope = default_memory_scope(&ctx.agent_id, &ctx.channel);
        let scope = args["scope"]
            .as_str()
            .map(|s| normalize_memory_scope(s, &ctx.agent_id))
            .unwrap_or(default_scope);
        let top_k = args["top_k"].as_u64().unwrap_or(5).clamp(1, 25) as usize;

        let Some(ref mem) = self.memory else {
            return Ok(json!({"results": [], "note": "memory store not available"}));
        };
        let docs = self.search_memory_docs(mem, &query, &scope, top_k).await?;
        let results: Vec<Value> = docs
            .into_iter()
            .map(|d| {
                let age = memory_age_label(chrono::Utc::now().timestamp(), d.created_at);
                json!({
                    "id": d.id,
                    "kind": d.kind,
                    "content": d.text,
                    "summary": d.display_text(),
                    "age": age,
                    "importance": d.importance,
                    "access_count": d.access_count,
                })
            })
            .collect();
        Ok(json!({"count": results.len(), "results": results}))
    }

    async fn search_memory_docs(
        &self,
        mem: &Arc<Mutex<MemoryStore>>,
        query: &str,
        scope: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryDoc>> {
        let bm25_hits = match self.store.search.search(
            query,
            Some(scope),
            top_k.saturating_mul(4).max(16),
        ) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::debug!(query = %query, scope = %scope, "BM25 memory search skipped: {e:#}");
                Vec::new()
            }
        };
        let mut store = mem.lock().await;
        let vec_docs = store
            .search(query, Some(scope), top_k.saturating_mul(3).max(top_k))
            .await?;
        let bm25_docs: Vec<MemoryDoc> = bm25_hits
            .into_iter()
            .filter_map(|hit| store.get_sync(&hit.id).cloned())
            .collect();
        Ok(rrf_fuse(vec_docs, bm25_docs, top_k))
    }

    pub(crate) async fn build_auto_recall_bundle(
        &self,
        agent_id: &str,
        channel: &str,
        query: &str,
    ) -> Option<RecallBundle> {
        if query.trim().is_empty() || matches!(channel, "heartbeat" | "cron" | "system") {
            return None;
        }
        let enabled = self
            .config
            .agents
            .defaults
            .memory
            .as_ref()
            .and_then(|m| m.auto_recall)
            .unwrap_or(true);
        if !enabled {
            return None;
        }
        let Some(ref mem) = self.memory else {
            return None;
        };
        let memory_cfg = self.config.agents.defaults.memory.as_ref();
        let final_k = memory_cfg
            .and_then(|m| m.recall_final_k)
            .unwrap_or(5)
            .clamp(1, 12);
        let max_tokens = memory_cfg
            .and_then(|m| {
                m.retrieval
                    .as_ref()
                    .and_then(|v| v.get("maxTokens"))
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
            })
            .unwrap_or(1200)
            .clamp(128, 4096);
        let scope = default_memory_scope(agent_id, channel);
        let docs = match self.search_memory_docs(mem, query, &scope, final_k).await {
            Ok(docs) => docs,
            Err(e) => {
                tracing::debug!(error = %e, "auto recall search failed");
                return None;
            }
        };
        let trace_id = format!("recall_{}", Uuid::new_v4());
        let mut bundle = recall_bundle_from_docs(docs, max_tokens, &trace_id);

        // KB auto-recall: surface the user's own corpus without requiring
        // the model to think of calling `knowledge_base` — the model is
        // worst at knowing what it doesn't know. Bounded: top-3 hits,
        // 600-token cap, gated on the KB actually having content and the
        // query being non-trivial (skips greetings/acks). KB doc ids are
        // intentionally NOT added to metadata.doc_ids — that list feeds
        // the memory importance feedback loop (Loop A), not KB.
        const KB_RECALL_K: usize = 3;
        const KB_RECALL_MAX_TOKENS: usize = 600;
        let kb_enabled = memory_cfg.and_then(|m| m.kb_auto_recall).unwrap_or(true);
        if kb_enabled
            && query.trim().chars().count() >= 6
            && let Some(kb) = crate::kb::global_service()
            && kb.has_content()
        {
            let q = query.trim().to_owned();
            // KnowledgeService::search is synchronous and CPU-heavy (embed +
            // HNSW + tantivy) — same off-executor treatment as the kb tool.
            match tokio::task::spawn_blocking(move || kb.search(&q, &[], KB_RECALL_K, 0.0)).await {
                Ok(Ok(hits)) if !hits.is_empty() => {
                    let block = format_kb_recall_block(&hits, KB_RECALL_MAX_TOKENS);
                    if !block.is_empty() {
                        match bundle.as_mut() {
                            Some(b) => {
                                b.context.push_str("\n\n");
                                b.context.push_str(&block);
                                b.metadata.hash = recall_context_hash(&b.context);
                            }
                            None => {
                                let hash = recall_context_hash(&block);
                                bundle = Some(RecallBundle {
                                    context: block,
                                    metadata: crate::provider::RecallMetadata {
                                        source: "kb".to_owned(),
                                        trace_id: Some(trace_id.clone()),
                                        hash,
                                        ..Default::default()
                                    },
                                });
                            }
                        }
                    }
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::debug!(error = %e, "kb auto recall search failed"),
                Err(e) => tracing::debug!(error = %e, "kb auto recall task failed"),
            }
        }
        bundle
    }


    pub(crate) async fn tool_memory_get(&self, args: Value) -> Result<Value> {
        let id = args["id"].as_str().unwrap_or("").to_owned();
        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        let store = mem.lock().await;
        match store.get(&id).await? {
            Some(d) => Ok(json!({"id": d.id, "scope": d.scope, "kind": d.kind, "text": d.text})),
            None => Ok(json!({"error": "not found", "id": id})),
        }
    }


    pub(crate) async fn tool_memory_put(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        let text = args["text"].as_str().unwrap_or("").trim().to_owned();
        if text.is_empty() {
            return Ok(json!({"stored": false, "note": "empty memory text"}));
        }
        // Internal channels (heartbeat/cron/system) get a separate scope so
        // their memories don't pollute normal conversation auto-recall.
        let default_scope = default_memory_scope(&ctx.agent_id, &ctx.channel);
        let scope = args["scope"]
            .as_str()
            .map(|s| normalize_memory_scope(s, &ctx.agent_id))
            .unwrap_or(default_scope);
        let kind = normalize_memory_kind(args["kind"].as_str());
        let id = args["id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let mut importance = default_memory_importance(&kind);
        if let Some(v) = args["importance"].as_f64() {
            importance = (v as f32).clamp(0.01, 1.0);
        }
        let tier = default_memory_doc_tier(&kind);
        let pinned = kind == "entity" || args["pinned"].as_bool().unwrap_or(false);
        let tags = if pinned {
            vec!["pinned".to_owned()]
        } else {
            Vec::new()
        };

        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        // Off-lock add: BERT inference happens between two brief lock
        // windows so concurrent reads/writes don't stall on it.
        crate::agent::memory::add_off_lock(
            mem,
            MemoryDoc {
                id: id.clone(),
                scope: scope.clone(),
                kind: kind.clone(),
                text: text.clone(),
                vector: vec![],
                created_at: 0,
                accessed_at: 0,
                access_count: 0,
                importance,
                tier,
                abstract_text: None,
                overview_text: None,
                tags,
                pinned,
            },
        )
        .await?;
        let (effective_id, effective_scope, effective_kind, effective_text) = {
            let store = mem.lock().await;
            store
                .find_exact(&scope, &kind, &text)
                .map(|doc| {
                    (
                        doc.id.clone(),
                        doc.scope.clone(),
                        doc.kind.clone(),
                        doc.text.clone(),
                    )
                })
                .unwrap_or_else(|| (id.clone(), scope.clone(), kind.clone(), text.clone()))
        };
        // Also index in tantivy BM25 for hybrid search.
        if let Err(e) = self.store.search.index_memory_doc(
            &effective_id,
            &effective_scope,
            &effective_kind,
            &effective_text,
        ) {
            tracing::warn!("BM25 index failed for memory_put doc: {e:#}");
        }
        // Only append to MEMORY.md for user-initiated /remember commands,
        // not for automatic memory_put calls by the model.
        if kind != "remember" {
            return Ok(
                json!({"stored": true, "id": effective_id, "scope": effective_scope, "kind": effective_kind}),
            );
        }
        // Snapshot defaults.workspace before the chain — `.or_else` takes a
        // sync closure so we can't await inside it.
        let default_workspace = self.live.agents.read().await.defaults.workspace.clone();
        let ws_str = self
            .handle
            .config
            .workspace
            .clone()
            .or(default_workspace)
            .unwrap_or_else(|| {
                crate::config::loader::base_dir()
                    .join("workspace")
                    .to_string_lossy()
                    .into_owned()
            });
        let ws = if ws_str.starts_with('~') {
            dirs_next::home_dir().unwrap_or_default().join(&ws_str[2..])
        } else {
            std::path::PathBuf::from(&ws_str)
        };
        let memory_path = ws.join("MEMORY.md");
        let entry = format!(
            "\n## {}\n{}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M"),
            text
        );
        if let Err(e) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&memory_path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, entry.as_bytes()))
        {
            tracing::warn!("failed to append to MEMORY.md: {e:#}");
        }
        Ok(
            json!({"stored": true, "id": effective_id, "scope": effective_scope, "kind": effective_kind}),
        )
    }

    pub(crate) async fn tool_memory_delete(&self, args: Value) -> Result<Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow!("memory_delete: `id` required"))?
            .to_owned();
        let Some(ref mem) = self.memory else {
            return Ok(json!({"error": "memory store not available"}));
        };
        mem.lock().await.delete(&id).await?;
        // Also remove from tantivy BM25 index.
        if let Err(e) = self
            .store
            .search
            .delete_document(&id)
            .and_then(|_| self.store.search.commit())
        {
            tracing::warn!("BM25 delete failed for doc {id}: {e:#}");
        }
        Ok(json!({"deleted": true, "id": id}))
    }


}

// ---------------------------------------------------------------------------
// Hybrid memory retrieval — Reciprocal Rank Fusion
// ---------------------------------------------------------------------------

/// Merge vector-search hits and BM25 hits using Reciprocal Rank Fusion (k=60).
///
/// Documents appearing in both lists get a higher combined score.
/// Documents only in one list still contribute their single-list score.
/// Returns the top `top_k` results as `MemoryDoc`s.
#[allow(dead_code)]
fn rrf_fuse(
    vec_hits: Vec<crate::agent::memory::MemoryDoc>,
    bm25_hits: Vec<crate::agent::memory::MemoryDoc>,
    top_k: usize,
) -> Vec<crate::agent::memory::MemoryDoc> {
    use std::collections::HashMap;

    const K: f32 = 60.0;

    // score_map: doc_id → (rrf_score, MemoryDoc)
    let mut scores: HashMap<String, (f32, crate::agent::memory::MemoryDoc)> = HashMap::new();

    // Vector hits — rank 1-based.
    for (rank, doc) in vec_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, doc));
    }

    // BM25 hits — rank 1-based.
    for (rank, doc) in bm25_hits.into_iter().enumerate() {
        let rrf = 1.0 / (K + (rank + 1) as f32);
        scores
            .entry(doc.id.clone())
            .and_modify(|(s, _)| *s += rrf)
            .or_insert((rrf, doc));
    }

    // Apply lifecycle + quality multiplier. Pinned/Core/important docs should
    // beat low-value notes when both are plausible matches.
    let mut ranked: Vec<(f32, MemoryDoc)> = scores
        .into_values()
        .map(|(score, doc)| {
            let quality = if doc.pinned { 1.25 } else { 1.0 }
                * match doc.tier {
                    MemDocTier::Core => 1.15,
                    MemDocTier::Working => 1.0,
                    MemDocTier::Peripheral => 0.8,
                }
                * (0.75 + doc.importance.clamp(0.01, 1.0) * 0.5);
            (score * doc.decay_multiplier() * quality, doc)
        })
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().take(top_k).map(|(_, doc)| doc).collect()
}


pub(crate) fn default_memory_scope(agent_id: &str, channel: &str) -> String {
    if matches!(channel, "heartbeat" | "cron" | "system") {
        format!("agent:{agent_id}:{channel}")
    } else {
        format!("agent:{agent_id}")
    }
}


/// Resolve a KB collection by name, creating it if absent. Returns
/// `(id, name, created)`. Used by the `knowledge_base` write actions so the
/// agent can target a collection by human name ("会议记录") without first
/// looking up its id. Name match is case-insensitive; a create racing with a
/// concurrent create falls back to the existing one.
pub(crate) fn resolve_or_create_collection(
    kb: &crate::kb::KnowledgeService,
    name: &str,
    desc: Option<String>,
) -> anyhow::Result<(String, String, bool)> {
    let find = || -> anyhow::Result<Option<crate::kb::model::KbCollection>> {
        Ok(kb
            .list_collections()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .into_iter()
            .find(|c| c.name.eq_ignore_ascii_case(name)))
    };
    if let Some(c) = find()? {
        return Ok((c.id, c.name, false));
    }
    match kb.create_collection(name, desc, None) {
        Ok(c) => Ok((c.id, c.name, true)),
        Err(crate::kb::KnowledgeError::DuplicateName) => {
            let c =
                find()?.ok_or_else(|| anyhow::anyhow!("collection vanished after duplicate"))?;
            Ok((c.id, c.name, false))
        }
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

pub(crate) fn normalize_memory_scope(scope: &str, agent_id: &str) -> String {
    let scope = scope.trim();
    if scope.is_empty() {
        return format!("agent:{agent_id}");
    }
    if scope == agent_id {
        return format!("agent:{agent_id}");
    }
    scope.to_owned()
}

fn normalize_memory_kind(kind: Option<&str>) -> String {
    match kind.unwrap_or("fact").trim() {
        "remember" => "remember",
        "entity" => "entity",
        "preference" => "preference",
        "procedure" => "procedure",
        "summary" => "summary",
        "note" => "note",
        "fact" | "" => "fact",
        _ => "fact",
    }
    .to_owned()
}

fn default_memory_importance(kind: &str) -> f32 {
    match kind {
        "entity" => 0.95,
        "remember" => 0.8,
        "preference" => 0.75,
        "procedure" => 0.65,
        "summary" => 0.55,
        "note" => 0.3,
        _ => 0.65,
    }
}

fn default_memory_doc_tier(kind: &str) -> MemDocTier {
    match kind {
        "entity" => MemDocTier::Core,
        "note" => MemDocTier::Peripheral,
        _ => MemDocTier::Working,
    }
}

/// sha256 of a recall context in the canonical `sha256:<hex>` wire form.
fn recall_context_hash(context: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(context.as_bytes()))
}

/// Render KB auto-recall hits as a compact, token-budgeted block the model
/// can cite from. Returns an empty string when nothing fits.
pub(crate) fn format_kb_recall_block(hits: &[crate::kb::service::SearchHit], max_tokens: usize) -> String {
    let header = "[Knowledge base — excerpts that MAY be relevant. Use only what actually \
                  answers the question and cite the source title; ignore the rest.]";
    let mut lines: Vec<String> = vec![header.to_owned()];
    let mut used_tokens = estimate_tokens(header);
    for h in hits {
        let title = h.source_title.trim();
        let title = if title.is_empty() { "untitled" } else { title };
        let line = format!("- ({title}) {}", h.chunk_text.trim());
        let line_tokens = estimate_tokens(&line);
        if used_tokens + line_tokens > max_tokens {
            // Always keep at least one (clipped) hit so a single long chunk
            // can't blank the whole block.
            if lines.len() == 1 {
                let char_limit = max_tokens.saturating_mul(2).max(64);
                lines.push(line.chars().take(char_limit).collect());
            }
            break;
        }
        used_tokens += line_tokens;
        lines.push(line);
    }
    if lines.len() == 1 {
        return String::new();
    }
    lines.join("\n")
}

pub(crate) fn recall_bundle_from_docs(
    docs: Vec<MemoryDoc>,
    max_tokens: usize,
    trace_id: &str,
) -> Option<RecallBundle> {
    let mut lines = Vec::new();
    let mut doc_ids = Vec::new();
    let mut used_tokens = 0usize;
    let mut truncated = false;

    for doc in docs {
        if doc.kind == "note" {
            continue;
        }
        let text = doc.display_text().trim();
        if text.is_empty() {
            continue;
        }
        let line = format!("- {text}");
        let line_tokens = estimate_tokens(&line);
        if used_tokens + line_tokens > max_tokens {
            truncated = true;
            if lines.is_empty() {
                let char_limit = max_tokens.saturating_mul(4).max(64);
                let clipped: String = line.chars().take(char_limit).collect();
                lines.push(clipped);
                doc_ids.push(doc.id);
            }
            break;
        }
        used_tokens += line_tokens;
        lines.push(line);
        doc_ids.push(doc.id);
    }

    if lines.is_empty() {
        return None;
    }
    let context = lines.join("\n");
    let hash = {
        use sha2::{Digest, Sha256};
        format!("sha256:{:x}", Sha256::digest(context.as_bytes()))
    };
    Some(RecallBundle {
        context,
        metadata: RecallMetadata {
            mode: "committed".to_owned(),
            format: "xml".to_owned(),
            source: "server".to_owned(),
            trace_id: Some(trace_id.to_owned()),
            max_tokens: Some(max_tokens as u32),
            doc_ids,
            hash,
            truncated,
        },
    })
}

