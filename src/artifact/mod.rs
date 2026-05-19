//! Tool-result artifact store.
//!
//! Every tool that produces a large `Value` funnels through `compact_value`,
//! which writes the full payload to `~/.rsclaw/artifacts/<session>/<id>.txt`
//! and returns a `{preview, tool_result_id, raw_chars, ...}` envelope. The
//! LLM sees the preview; if it needs the full output it calls the
//! `read_artifact` tool with the id.
//!
//! This is the sole compaction pipeline — there are no per-tool rules.
//! A prior tokenjuice port was deleted in favour of this design.

pub mod compact;
pub mod store;
pub mod text;

pub use compact::{compact_value, PreviewBudget, ARTIFACT_THRESHOLD_CHARS};
pub use store::{ArtifactId, ArtifactStore, ARTIFACT_SUBDIR};

use std::sync::OnceLock;
use std::time::Duration;

/// Housekeep cadence — every 6h we walk the artifact dir and delete files
/// past the 7-day TTL. Cheap (one `read_dir` per session + mtime check per
/// file) and bounded by `SESSION_FILE_CAP * num_sessions`.
const HOUSEKEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Process-wide default store (under `~/.rsclaw/artifacts`). Lazy-init so
/// `base_dir()` is resolved at first call, not at startup. The first call
/// also spawns a background housekeeping loop — without this, the artifact
/// dir grows unbounded across sessions (per-session caps don't help once
/// sessions go away).
pub fn default_store() -> &'static ArtifactStore {
    static STORE: OnceLock<ArtifactStore> = OnceLock::new();
    STORE.get_or_init(|| {
        let store = ArtifactStore::default_store();
        // Best-effort initial sweep, then a long-running loop. We only spawn
        // when a tokio runtime is available — unit tests and CLI one-shots
        // skip the loop and rely on the next process to clean up.
        store.housekeep();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let bg = store.clone();
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(HOUSEKEEP_INTERVAL).await;
                    bg.housekeep();
                }
            });
        }
        store
    })
}
