pub mod chunk_embed;

use std::sync::Arc;

use anyhow::Result;

use crate::{
    embedder::KbEmbedder, index::KbIndex, jobs::JobKind, paths::KbPaths, store::KbStore,
};

/// Job handler. One impl per `JobKind` variant. Handlers must be
/// **idempotent** — the worker reclaim path will re-run them after
/// stalled claims.
pub trait JobHandler: Send + Sync {
    fn handle(&self, ctx: &HandlerCtx, kind: &JobKind) -> Result<()>;
}

pub struct HandlerCtx {
    pub store: Arc<KbStore>,
    pub paths: Arc<KbPaths>,
    pub embedder: Arc<dyn KbEmbedder>,
    pub index: Arc<KbIndex>,
}

/// Default dispatcher: matches on `JobKind` and delegates.
pub struct DefaultDispatcher;

impl JobHandler for DefaultDispatcher {
    fn handle(&self, ctx: &HandlerCtx, kind: &JobKind) -> Result<()> {
        match kind {
            JobKind::ChunkAndEmbed {
                doc_id,
                doc_version,
            } => chunk_embed::run(ctx, doc_id, *doc_version),
            JobKind::RebuildHnsw => {
                tracing::warn!("kb worker: RebuildHnsw handler not implemented in Week 2");
                Ok(())
            }
            JobKind::RunCompactor => {
                tracing::warn!("kb worker: RunCompactor handler not implemented in Week 2");
                Ok(())
            }
        }
    }
}
