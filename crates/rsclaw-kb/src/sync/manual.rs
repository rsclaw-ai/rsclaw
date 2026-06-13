//! ManualUploadSyncer: file path → bytes → canonicalize_by_mime →
//! ingest_canonicalized. Used by `rsclaw kb add <path>` and the
//! future UI drag-drop flow. No HTTP, no cursor.

use std::path::PathBuf;

use super::{KbSourceSyncer, SyncContext, SyncError, SyncOutcome, SyncReason};
use crate::kb::{
    canonicalize::{CanonicalizeInput, canonicalize_by_mime, detect_mime},
    model::KbSourceKind,
    pipeline::{IngestInput, ingest_canonicalized},
};

pub struct ManualUploadSyncer {
    pub source_id: String,
    pub file_path: PathBuf,
    pub tags: Vec<String>,
}

#[async_trait::async_trait]
impl KbSourceSyncer for ManualUploadSyncer {
    fn source_kind(&self) -> KbSourceKind {
        KbSourceKind::Doc
    }
    fn source_id(&self) -> &str {
        &self.source_id
    }
    fn sync_interval_secs(&self) -> Option<u64> {
        None
    }

    async fn sync(&self, ctx: &SyncContext, _reason: SyncReason) -> Result<SyncOutcome, SyncError> {
        let bytes = std::fs::read(&self.file_path)
            .map_err(|e| SyncError::Permanent(format!("read {}: {e}", self.file_path.display())))?;
        let filename = self
            .file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        let mime = detect_mime(&bytes, Some(filename));
        let ext = self
            .file_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes: &bytes,
            mime: &mime,
            hint_title: Some(filename),
            logical_source_id_seed: None,
        })
        .map_err(|e| SyncError::Parse(format!("canonicalize: {e}")))?
        .ok_or_else(|| SyncError::Parse(format!("no canonical output for mime={mime}")))?;
        let mut canon = canon;
        canon.metadata.tags.extend(self.tags.iter().cloned());
        let lsid_str = canon.metadata.logical_source_id.as_str().to_string();
        let out = ingest_canonicalized(
            &ctx.store,
            IngestInput {
                canon: &canon,
                raw_bytes: &bytes,
                raw_ext: ext,
                visibility: None,
                owner_user_id: None,
                seen_key: Some(("manual", &lsid_str)),
                source: None,
                paths: &ctx.paths,
            },
        )
        .map_err(|e| SyncError::Permanent(format!("ingest: {e}")))?;
        Ok(SyncOutcome {
            docs_added: if out.noop { 0 } else { 1 },
            docs_skipped: if out.noop { 1 } else { 0 },
            ..Default::default()
        })
    }
}
