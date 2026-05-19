//! Tool-result artifact store.
//!
//! Every tool that produces a large `Value` funnels through `compact_value`,
//! which writes the full payload to `~/.rsclaw/artifacts/<session>/<id>.txt`
//! and returns a `{preview, tool_result_id, raw_chars, ...}` envelope. The
//! LLM sees the preview; if it needs the full output it calls the
//! `read_artifact` tool with the id.
//!
//! This replaces the per-tool tokenjuice rule library as the *primary*
//! compaction strategy. Rules can still layer on top for smarter inline
//! previews, but they're no longer load-bearing.

pub mod compact;
pub mod store;
pub mod text;

pub use compact::{compact_value, ARTIFACT_THRESHOLD_CHARS};
pub use store::{ArtifactId, ArtifactStore, ARTIFACT_SUBDIR};

use std::sync::OnceLock;

/// Process-wide default store (under `~/.rsclaw/artifacts`). Lazy-init so
/// `base_dir()` is resolved at first call, not at startup.
pub fn default_store() -> &'static ArtifactStore {
    static STORE: OnceLock<ArtifactStore> = OnceLock::new();
    STORE.get_or_init(ArtifactStore::default_store)
}
