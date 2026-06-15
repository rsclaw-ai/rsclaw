//! ChunkAndEmbed handler: read staged markdown, chunk, embed, write
//! chunks to redb, mark ledger IndexingComplete. Idempotent —
//! deterministic chunk_ids mean re-running produces identical rows.

use anyhow::{Context, Result};

use crate::{
    chunker::{ChunkerInput, LocatorKind, chunk_markdown},
    content_store::read::read_doc_body,
    entities::extract::{canonical_id, extract_entities},
    ledger::LedgerStatus,
    model::{KbChunk, KbEntity, KbEntityIndex, LogicalSourceId},
    store::{chunks, docs, entities, ledger},
    worker::handlers::HandlerCtx,
};

pub fn run(ctx: &HandlerCtx, doc_id: &str, doc_version: u32) -> Result<()> {
    // 1. Load doc + body.
    let doc = {
        let rtx = ctx.store.begin_read()?;
        docs::get(&rtx, doc_id)?
            .ok_or_else(|| anyhow::anyhow!("chunk_embed: doc {doc_id} not found"))?
    };
    if doc.version != doc_version {
        tracing::warn!(
            doc = %crate::redact(doc_id),
            "kb worker: doc version mismatch (job v{doc_version} vs current v{}); skipping",
            doc.version
        );
        return Ok(());
    }
    let abs = ctx.paths.root.join(&doc.markdown_path);
    let body = read_doc_body(&abs).with_context(|| format!("read body {}", abs.display()))?;

    // 2. Chunk.
    let lsid = LogicalSourceId(doc.logical_source_id.clone());
    let chunks_vec: Vec<KbChunk> = chunk_markdown(ChunkerInput {
        logical_source_id: &lsid,
        doc_id: &doc.id,
        doc_version: doc.version,
        doc_title: &doc.title,
        markdown_body: &body,
        default_locator_kind: LocatorKind::MdSection,
    });

    // 3. Embed.
    let texts: Vec<String> = chunks_vec.iter().map(|c| c.indexed_text.clone()).collect();
    let vectors = ctx.embedder.embed_batch(&texts)?;
    if vectors.len() != chunks_vec.len() {
        return Err(anyhow::anyhow!(
            "embedder returned {} vectors for {} chunks",
            vectors.len(),
            chunks_vec.len()
        ));
    }
    let embedder_id = ctx.embedder.embedder_id().to_string();
    let chunks_with_vec: Vec<KbChunk> = chunks_vec
        .into_iter()
        .zip(vectors)
        .map(|(mut c, v)| {
            c.vector = v;
            c.embedder_id = embedder_id.clone();
            c
        })
        .collect();

    // 4. Persist chunks + advance ledger in one tx.
    {
        let wtx = ctx.store.begin_write()?;
        let now_ms = chrono::Utc::now().timestamp_millis();

        // 4a. Drop chunks from prior doc_versions.
        let removed =
            chunks::delete_for_doc_version_below(&wtx, &doc.logical_source_id, doc.version)?;
        if removed > 0 {
            tracing::info!(
                doc = %crate::redact(doc_id),
                old_chunks = removed,
                "kb worker: removed stale chunks from prior version"
            );
        }

        // 4b. Insert new chunks + per-chunk entity edges (regex
        //     extractor; v2 NER lands behind the same call site).
        for c in &chunks_with_vec {
            chunks::put(&wtx, c)?;
            for mention in extract_entities(&c.indexed_text) {
                let canonical = canonical_id(mention.kind, &mention.surface);
                entities::put_entity(
                    &wtx,
                    &KbEntity {
                        canonical_id: canonical.clone(),
                        surface_forms: vec![mention.surface.clone()],
                        kind: mention.kind,
                        created_at: now_ms,
                    },
                )?;
                entities::put_index(
                    &wtx,
                    &KbEntityIndex {
                        entity_id: canonical,
                        chunk_id: c.id.clone(),
                        doc_id: c.doc_id.clone(),
                        mention_count: 1,
                        score: 1.0,
                    },
                )?;
            }
        }

        // 4c. Advance the ledger.
        match ledger::find_pending_by_doc_in_wtx(&wtx, &doc.id)? {
            Some(entry) => {
                ledger::update_status(&wtx, &entry.id, LedgerStatus::IndexingComplete, now_ms)?;
            }
            None => {
                tracing::debug!(
                    doc = %crate::redact(&doc.id),
                    "kb worker: no Pending ledger row for doc — treating as idempotent rerun"
                );
            }
        }

        wtx.commit()?;
    }

    // 5. Update indexes (caches over redb; failures propagate so the worker retries
    //    the job — chunks are already durable in redb).
    for c in &chunks_with_vec {
        ctx.index.upsert_chunk(c)?;
    }
    ctx.index.commit()?;

    tracing::info!(
        doc = %crate::redact(doc_id),
        n_chunks = chunks_with_vec.len(),
        "kb worker: chunk_embed + index update complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        canonicalize::{CanonicalizeInput, canonicalize_by_mime},
        embedder::{KbEmbedder, StubEmbedder},
        paths::KbPaths,
        pipeline::{IngestInput, ingest_canonicalized},
        store::KbStore,
    };

    fn fixture() -> (TempDir, HandlerCtx, String) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let index = Arc::new(crate::index::KbIndex::open(&paths).unwrap());

        let bytes = b"# Hi\n\nbody one.\n\nbody two.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon,
                raw_bytes: bytes,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        let ctx = HandlerCtx {
            store,
            paths,
            embedder,
            index,
        };
        (tmp, ctx, out.doc_id)
    }

    #[test]
    fn writes_chunks_with_vectors() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let cs = chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap();
        assert!(!cs.is_empty());
        for c in &cs {
            assert_eq!(c.vector.len(), 1024);
            assert_eq!(c.embedder_id, "stub-sha256-1024");
        }
    }

    #[test]
    fn idempotent_rerun_produces_same_chunks() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let before = {
            let rtx = ctx.store.begin_read().unwrap();
            chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let after = {
            let rtx = ctx.store.begin_read().unwrap();
            chunks::chunks_for_logical(&rtx, &doc.logical_source_id).unwrap()
        };
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.vector, b.vector);
        }
    }

    #[test]
    fn ledger_advances_to_indexing_complete() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let done = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].doc_id, doc_id);
    }

    #[test]
    fn rerun_after_ledger_advanced_does_not_error() {
        let (_tmp, ctx, doc_id) = fixture();
        let doc = {
            let rtx = ctx.store.begin_read().unwrap();
            docs::get(&rtx, &doc_id).unwrap().unwrap()
        };
        run(&ctx, &doc_id, doc.version).unwrap();
        run(&ctx, &doc_id, doc.version).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let done = ledger::list_by_status(&rtx, LedgerStatus::IndexingComplete).unwrap();
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn new_version_drops_old_chunks() {
        use crate::{canonicalize::CanonicalizeInput, model::LogicalSourceId};

        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());
        let index = Arc::new(crate::index::KbIndex::open(&paths).unwrap());
        let ctx = HandlerCtx {
            store: store.clone(),
            paths: paths.clone(),
            embedder,
            index,
        };

        let lsid = LogicalSourceId("file:custom:rotate".into());
        let mk = |bytes: &[u8]| {
            crate::canonicalize::canonicalize_by_mime(CanonicalizeInput {
                bytes,
                mime: "text/markdown",
                hint_title: Some("rotate"),
                logical_source_id_seed: Some(lsid.clone()),
            })
            .unwrap()
            .unwrap()
        };

        let v1 = mk(b"# v1\n\nfirst body very different.");
        let v1_out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &v1,
                raw_bytes: b"# v1\n\nfirst body very different.",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        run(&ctx, &v1_out.doc_id, 1).unwrap();
        let v1_chunks_before = {
            let rtx = store.begin_read().unwrap();
            chunks::chunks_for_logical(&rtx, &lsid.0).unwrap()
        };
        assert!(!v1_chunks_before.is_empty());

        let v2 = mk(b"# v2\n\nentirely different body content here.");
        let v2_out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &v2,
                raw_bytes: b"# v2\n\nentirely different body content here.",
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();
        run(&ctx, &v2_out.doc_id, 2).unwrap();

        let rtx = store.begin_read().unwrap();
        let chunks_after = chunks::chunks_for_logical(&rtx, &lsid.0).unwrap();
        for c in &chunks_after {
            assert_eq!(c.doc_version, 2);
        }
    }
}
