//! KbSourceSyncer trait + sync result types. Week 4 ships two
//! implementations: ManualUploadSyncer (file → ingest) and UrlSyncer
//! (HTTP → ingest). Future syncers (LocalFolder, Mail, Chat) plug into
//! the same trait.

pub mod manual;
pub mod state;
pub mod url;

use std::sync::Arc;

pub use manual::ManualUploadSyncer;
use serde::{Deserialize, Serialize};
pub use state::SyncRegistry;
pub use url::UrlSyncer;

use crate::{
    embedder::KbEmbedder, index::KbIndex, model::KbSourceKind, paths::KbPaths, store::KbStore,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncReason {
    Periodic,
    Manual,
    OnEnable,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub docs_added: usize,
    pub docs_updated: usize,
    pub docs_skipped: usize,
    pub partial: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("auth failed: {0}")]
    AuthFailed(String),
    #[error("rate limited, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("budget exhausted")]
    BudgetExhausted,
    #[error("network: {0}")]
    Network(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("permanent: {0}")]
    Permanent(String),
    #[error("cancelled")]
    Cancelled,
}

pub struct SyncContext {
    pub store: Arc<KbStore>,
    pub paths: Arc<KbPaths>,
    pub index: Arc<KbIndex>,
    pub embedder: Arc<dyn KbEmbedder>,
}

#[async_trait::async_trait]
pub trait KbSourceSyncer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    fn source_id(&self) -> &str;
    fn sync_interval_secs(&self) -> Option<u64> {
        Some(20 * 60)
    }
    async fn sync(&self, ctx: &SyncContext, reason: SyncReason) -> Result<SyncOutcome, SyncError>;
    /// Called once when a syncer is enabled / registered. Default
    /// impl is a no-op; concrete syncers can use this for any
    /// one-shot setup (e.g. fetching feed metadata, registering a
    /// webhook) before the first periodic tick.
    async fn on_enable(&self, _ctx: &SyncContext) -> Result<(), SyncError> {
        Ok(())
    }
}
