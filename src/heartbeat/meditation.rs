//! Meditation engine -- periodic memory maintenance triggered by heartbeat.
//!
//! Phases (each returns its own count in [`MeditationReport`]):
//! 1. Dedup: merge near-duplicate Core/Working memories (cosine sim > 0.92)
//! 2. Crystallize: distill Core un-crystallized clusters into SKILL.md
//!    files (only runs when crystallization deps are provided)
//! 3. Cleanup: demote "crystallized"-tagged memories to Peripheral after
//!    [`MeditationConfig::crystallized_ttl_days`]

use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;

use crate::agent::memory::{MemDocTier, MemoryStore};
use crate::provider::registry::ProviderRegistry;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for a meditation cycle.
pub struct MeditationConfig {
    /// Cosine similarity threshold for dedup (default: 0.92).
    pub dedup_threshold: f32,
    /// Max docs to process per cycle (default: 50).
    pub batch_size: usize,
    /// Days after crystallization before demoting to Peripheral.
    pub crystallized_ttl_days: u32,
}

impl Default for MeditationConfig {
    fn default() -> Self {
        // Pick up live values from the evolution config (matches the
        // previous hardcoded constants when no override is set).
        let evo = crate::agent::evolution::evolution_config();
        Self {
            dedup_threshold: evo.meditation.dedup_threshold,
            batch_size: 50,
            crystallized_ttl_days: evo.meditation.crystallized_ttl_days,
        }
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// Summary of actions taken during a meditation cycle.
#[derive(Debug, Default)]
pub struct MeditationReport {
    /// Number of duplicate documents merged (deleted).
    pub duplicates_merged: usize,
    /// Number of crystallized documents cleaned (demoted).
    pub crystallized_cleaned: usize,
    /// Number of new SKILL.md files written by the crystallize phase.
    pub skills_crystallized: usize,
    /// Total documents inspected across all phases.
    pub total_processed: usize,
}

// Crystallize phase cap is read from the live evolution config —
// `meditation.max_per_cycle`.

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run a full meditation cycle: dedup then cleanup.
///
/// Returns a report summarising what was changed.
pub async fn meditate(
    store: &mut MemoryStore,
    scope: &str,
    config: &MeditationConfig,
) -> Result<MeditationReport> {
    let mut report = MeditationReport::default();

    let merged = dedup_phase(store, scope, config).await?;
    report.duplicates_merged = merged;

    let cleaned = cleanup_phase(store, scope, config).await?;
    report.crystallized_cleaned = cleaned;

    report.total_processed = merged + cleaned;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Dedup phase
// ---------------------------------------------------------------------------

/// Merge near-duplicate Core-tier documents.
///
/// For each pair above the similarity threshold, the document with the lower
/// importance score is deleted, keeping the stronger memory.
async fn dedup_phase(
    store: &mut MemoryStore,
    scope: &str,
    config: &MeditationConfig,
) -> Result<usize> {
    // Collect IDs of Core docs up-front (borrow released before mutation).
    let candidate_ids: Vec<String> = store
        .find_by_tier(&MemDocTier::Core, Some(scope))
        .into_iter()
        .take(config.batch_size)
        .map(|d| d.id.clone())
        .collect();

    let mut merged: usize = 0;
    let mut seen: HashSet<String> = HashSet::new();

    for doc_id in &candidate_ids {
        if seen.contains(doc_id) {
            continue;
        }

        let duplicates = store.find_near_duplicates(doc_id, Some(scope), config.dedup_threshold)?;

        for (dup_doc, _sim) in &duplicates {
            if seen.contains(&dup_doc.id) {
                continue;
            }

            // Decide which doc to keep: higher importance wins.
            let src = store.get_sync(doc_id);
            let src_importance = src.map(|d| d.importance).unwrap_or(0.0);

            let (keep_id, remove_id) = if src_importance >= dup_doc.importance {
                (doc_id.as_str(), dup_doc.id.as_str())
            } else {
                (dup_doc.id.as_str(), doc_id.as_str())
            };

            // Mark both as seen so neither is re-processed.
            seen.insert(keep_id.to_owned());
            seen.insert(remove_id.to_owned());

            store.delete(remove_id).await?;
            merged += 1;
        }

        seen.insert(doc_id.clone());
    }

    Ok(merged)
}

// ---------------------------------------------------------------------------
// Cleanup phase
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Crystallize phase (optional)
// ---------------------------------------------------------------------------

/// Scan Core memories that haven't been crystallized yet and try to distill
/// each into a `SKILL.md` file. Caps work at the live evolution config's
/// `meditation.max_per_cycle` to bound LLM cost per cycle.
///
/// This is the bottom-line path that catches Core memories the runtime
/// online trigger missed (e.g. promoted but never recalled again).
///
/// Takes `Arc<Mutex<MemoryStore>>` (not `&mut MemoryStore`) because
/// `crystallize_one` releases the lock during the LLM call to avoid
/// blocking other memory consumers for tens of seconds.
pub async fn crystallize_phase(
    store: &Arc<tokio::sync::Mutex<MemoryStore>>,
    scope: &str,
    providers: &Arc<ProviderRegistry>,
    flash_model: &str,
    skills_dir: &std::path::Path,
) -> Result<usize> {
    let max_per_cycle = crate::agent::evolution::evolution_config()
        .meditation
        .max_per_cycle;

    // 1. Collect candidate doc IDs (brief lock).
    let candidates: Vec<String> = {
        let s = store.lock().await;
        s.find_by_tier(&MemDocTier::Core, Some(scope))
            .into_iter()
            .filter(|d| !d.tags.iter().any(|t| t == "crystallized"))
            .take(max_per_cycle)
            .map(|d| d.id.clone())
            .collect()
    };

    let mut written = 0usize;
    for doc_id in candidates {
        match crate::skill::crystallizer::crystallize_one(
            store,
            &doc_id,
            scope,
            providers,
            flash_model,
            skills_dir,
        )
        .await
        {
            Ok(Some(_path)) => written += 1,
            Ok(None) => {} // no cluster / in-flight / model issue — already logged
            Err(e) => {
                tracing::warn!(doc_id, "crystallize_phase hard failure: {e:#}");
            }
        }
    }

    Ok(written)
}

/// Demote crystallized memories that have exceeded their TTL.
///
/// Documents tagged "crystallized" whose age exceeds `crystallized_ttl_days`
/// have their importance set to 0.01, which triggers demotion to Peripheral
/// via `evaluate_tier_transition`.
async fn cleanup_phase(
    store: &mut MemoryStore,
    scope: &str,
    config: &MeditationConfig,
) -> Result<usize> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let ttl_secs = i64::from(config.crystallized_ttl_days) * 86_400;

    // Collect IDs of crystallized Core/Working docs whose age exceeds TTL.
    let mut to_demote: Vec<String> = Vec::new();

    for tier in &[MemDocTier::Core, MemDocTier::Working] {
        let docs = store.find_by_tier(tier, Some(scope));
        for doc in docs {
            if doc.tags.iter().any(|t| t == "crystallized") {
                let age_secs = now_secs - doc.created_at;
                if age_secs > ttl_secs {
                    to_demote.push(doc.id.clone());
                }
            }
        }
    }

    let mut cleaned: usize = 0;

    for id in &to_demote {
        // Read current importance so we can compute the delta to reach 0.01.
        let current = store.get_sync(id).map(|d| d.importance).unwrap_or(0.5);
        let delta = 0.01 - current;
        store.adjust_importance(id, delta).await?;
        cleaned += 1;
    }

    Ok(cleaned)
}
