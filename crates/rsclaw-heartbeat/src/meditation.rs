//! Meditation engine -- periodic memory maintenance triggered by heartbeat.
//!
//! Phases (each returns its own count in [`MeditationReport`]):
//! 1. Dedup: merge near-duplicate Core/Working memories (cosine sim > 0.92)
//! 2. Crystallize: distill Core un-crystallized clusters into SKILL.md files
//!    (only runs when crystallization deps are provided)
//! 3. Cleanup: demote "crystallized"-tagged memories to Peripheral after
//!    [`MeditationConfig::crystallized_ttl_days`]

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use rsclaw_memory::{MemDocTier, MemoryStore};
use rsclaw_provider::registry::ProviderRegistry;

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
        let evo = rsclaw_evolution::evolution_config();
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
    /// Lessons published to the workspace `memory/lessons.md` (0 also when
    /// the file was removed because no lesson qualified).
    pub lessons_published: usize,
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

    // Phase isolation (R2 review I2): one failing phase must not skip
    // the others. Previously `?` propagated dedup errors and skipped
    // cleanup entirely — that left stale records around longer than
    // necessary and silently dropped the cleanup-counter from the
    // report. Match-and-log so each phase runs independently.
    let merged = match dedup_phase(store, scope, config).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(scope, "meditate dedup phase failed: {e:#}");
            0
        }
    };
    report.duplicates_merged = merged;

    let cleaned = match cleanup_phase(store, scope, config).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(scope, "meditate cleanup phase failed: {e:#}");
            0
        }
    };
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
    primary_model: &str,
    skills_dir: &std::path::Path,
) -> Result<usize> {
    let max_per_cycle = rsclaw_evolution::evolution_config()
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
        match rsclaw_skill::crystallizer::crystallize_one(
            store,
            &doc_id,
            scope,
            providers,
            primary_model,
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

/// Lessons phase: publish the top user-correction lessons into the agent's
/// workspace as `memory/lessons.md`, which the workspace context loads into
/// the system prompt's "Learned Rules" section every turn. This is the
/// always-visible channel for behavioural corrections — vector recall only
/// surfaces a lesson when the query happens to be near it; rules like
/// "don't add comments unless asked" must not depend on that luck.
///
/// Deterministic and idempotent: top `max` lessons by importance (floor
/// 0.6), rendered as bullets. No qualifying lessons → the file is removed
/// so stale rules never linger.
pub fn lessons_phase(
    store: &rsclaw_memory::MemoryStore,
    scope: &str,
    workspace: &std::path::Path,
    max: usize,
) -> Result<usize> {
    const MIN_IMPORTANCE: f32 = 0.6;
    let lessons: Vec<String> = store
        .find_by_kind(scope, "lesson")
        .into_iter()
        .filter(|d| d.importance >= MIN_IMPORTANCE)
        .take(max)
        .map(|d| format!("- {}", d.text.replace('\n', " ")))
        .collect();

    let path = workspace.join("memory").join("lessons.md");
    if lessons.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(0);
    }
    std::fs::create_dir_all(workspace.join("memory"))?;
    let body = format!(
        "<!-- Auto-maintained by the meditation cycle from kind=lesson \
memories. Edit the memories, not this file. -->\n\
These are standing behavioural rules learned from the user's corrections. \
Follow them unless the user says otherwise.\n\n{}\n",
        lessons.join("\n")
    );
    std::fs::write(&path, body)?;
    Ok(lessons.len())
}

#[cfg(test)]
mod lessons_tests {
    use rsclaw_memory::{MemDocTier, MemoryDoc, MemoryStore};

    use super::*;
    use crate::MemoryTier;

    fn lesson(id: &str, text: &str, importance: f32) -> MemoryDoc {
        MemoryDoc {
            id: id.into(),
            scope: "agent:t".into(),
            kind: "lesson".into(),
            text: text.into(),
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance,
            tier: MemDocTier::Core,
            abstract_text: None,
            overview_text: None,
            tags: vec![],
            pinned: false,
        }
    }

    #[tokio::test]
    async fn lessons_phase_publishes_and_retracts() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let mut store = MemoryStore::open(&tmp.path().join("mem"), None, MemoryTier::High, None)
            .await
            .unwrap();
        store
            .add(lesson("l1", "回答代码问题时不要加解释性注释", 0.85))
            .await
            .unwrap();
        store
            .add(lesson("l2", "提交信息永远不要加 Co-Authored-By", 0.9))
            .await
            .unwrap();
        store
            .add(lesson("l3", "低置信教训不该出现", 0.3))
            .await
            .unwrap();

        let n = lessons_phase(&store, "agent:t", &ws, 8).unwrap();
        assert_eq!(n, 2);
        let body = std::fs::read_to_string(ws.join("memory").join("lessons.md")).unwrap();
        assert!(body.contains("Co-Authored-By"));
        assert!(body.contains("解释性注释"));
        assert!(!body.contains("低置信教训"));
        // importance 排序：0.9 在前
        assert!(body.find("Co-Authored-By").unwrap() < body.find("解释性注释").unwrap());

        // 全部教训失格 → 文件撤下
        store.adjust_importance("l1", -0.8).await.unwrap();
        store.adjust_importance("l2", -0.8).await.unwrap();
        let n = lessons_phase(&store, "agent:t", &ws, 8).unwrap();
        assert_eq!(n, 0);
        assert!(!ws.join("memory").join("lessons.md").exists());
    }
}
