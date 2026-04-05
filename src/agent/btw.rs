//! Background context injection (/btw command).
//!
//! Unlike memory (relevance-based recall), btw entries are ALWAYS injected
//! into the system prompt for all subsequent LLM calls.  TTL-based entries
//! auto-expire after N turns.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::store::redb_store::RedbStore;

/// Redb KV key for persisted btw entries.
const BTW_STORE_KEY: &str = "btw_entries";

// -------------------------------------------------------------------------
// Data types
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwEntry {
    pub id: u32,
    pub content: String,
    pub created_at: i64,
    /// `None` = permanent.
    pub ttl_turns: Option<u32>,
    /// Decremented each turn; entry removed when it reaches 0.
    pub remaining_turns: Option<u32>,
    pub scope: BtwScope,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum BtwScope {
    /// Specific session only.
    Session(String),
    /// All sessions on this channel.
    Channel(String),
    /// All sessions everywhere.
    Global,
}

// -------------------------------------------------------------------------
// Manager
// -------------------------------------------------------------------------

pub struct BtwManager {
    entries: Arc<RwLock<Vec<BtwEntry>>>,
    next_id: Arc<RwLock<u32>>,
    store: Option<Arc<RedbStore>>,
}

impl BtwManager {
    /// Create a new manager, optionally loading persisted entries from redb.
    pub fn new(store: Option<Arc<RedbStore>>) -> Self {
        let mut mgr = Self {
            entries: Arc::new(RwLock::new(Vec::new())),
            next_id: Arc::new(RwLock::new(1)),
            store,
        };
        mgr.load_from_redb();
        mgr
    }

    // -- mutating operations -----------------------------------------------

    /// Add a new entry.  Returns the assigned id.
    pub async fn add(&self, content: &str, scope: BtwScope, ttl_turns: Option<u32>) -> u32 {
        let mut id_guard = self.next_id.write().await;
        let id = *id_guard;
        *id_guard = id + 1;
        drop(id_guard);

        let entry = BtwEntry {
            id,
            content: content.to_owned(),
            created_at: chrono::Utc::now().timestamp(),
            ttl_turns,
            remaining_turns: ttl_turns,
            scope,
        };

        self.entries.write().await.push(entry);
        self.save_to_redb().await;
        id
    }

    /// Remove an entry by id.  Returns `true` if found and removed.
    pub async fn remove(&self, id: u32) -> bool {
        let mut guard = self.entries.write().await;
        let before = guard.len();
        guard.retain(|e| e.id != id);
        let removed = guard.len() < before;
        drop(guard);
        if removed {
            self.save_to_redb().await;
        }
        removed
    }

    /// Clear entries.  If `scope_filter` is provided, only clear entries whose
    /// scope matches (session key or "global").
    pub async fn clear(&self, scope_filter: Option<&str>) {
        let mut guard = self.entries.write().await;
        if let Some(filter) = scope_filter {
            guard.retain(|e| !scope_matches(e, filter, ""));
        } else {
            guard.clear();
        }
        drop(guard);
        self.save_to_redb().await;
    }

    // -- read operations ---------------------------------------------------

    /// List entries matching the given session/channel/global.
    pub async fn list(&self, session_key: &str, channel: &str) -> Vec<BtwEntry> {
        let guard = self.entries.read().await;
        guard
            .iter()
            .filter(|e| scope_matches(e, session_key, channel))
            .cloned()
            .collect()
    }

    /// Render matching entries as an XML block for the system prompt.
    /// Returns empty string when no entries match.
    pub async fn to_prompt_block(&self, session_key: &str, channel: &str) -> String {
        let entries = self.list(session_key, channel).await;
        if entries.is_empty() {
            return String::new();
        }
        format_prompt_block(&entries)
    }

    /// Like `to_prompt_block` but with V3 relevance filtering.
    /// When > 5 matching entries, score by keyword overlap with the user
    /// message and return top 5 + all TTL entries.
    pub async fn to_prompt_block_relevant(
        &self,
        session_key: &str,
        channel: &str,
        user_message: &str,
    ) -> String {
        let entries = self.list(session_key, channel).await;
        if entries.is_empty() {
            return String::new();
        }
        if entries.len() <= 5 {
            return format_prompt_block(&entries);
        }

        // TTL entries are always included.
        let mut selected: Vec<BtwEntry> = entries
            .iter()
            .filter(|e| e.remaining_turns.is_some())
            .cloned()
            .collect();
        let ttl_ids: HashSet<u32> = selected.iter().map(|e| e.id).collect();

        // Score non-TTL entries by relevance.
        let mut scored: Vec<(f32, &BtwEntry)> = entries
            .iter()
            .filter(|e| !ttl_ids.contains(&e.id))
            .map(|e| (relevance_score(&e.content, user_message), e))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        let remaining_slots = 5usize.saturating_sub(selected.len());
        for (_, entry) in scored.into_iter().take(remaining_slots) {
            selected.push(entry.clone());
        }

        // Sort by id for stable ordering.
        selected.sort_by_key(|e| e.id);
        format_prompt_block(&selected)
    }

    // -- TTL management ----------------------------------------------------

    /// Decrement `remaining_turns` for entries matching the session.
    /// Removes entries that reach 0.
    pub async fn tick_turn(&self, session_key: &str) {
        let mut guard = self.entries.write().await;
        let mut changed = false;
        for entry in guard.iter_mut() {
            if !scope_matches(entry, session_key, "") {
                continue;
            }
            if let Some(ref mut remaining) = entry.remaining_turns {
                if *remaining > 0 {
                    *remaining -= 1;
                    changed = true;
                }
            }
        }
        guard.retain(|e| {
            if let Some(remaining) = e.remaining_turns {
                remaining > 0
            } else {
                true // permanent entries never expire
            }
        });
        drop(guard);
        if changed {
            self.save_to_redb().await;
        }
    }

    // -- persistence -------------------------------------------------------

    async fn save_to_redb(&self) {
        let Some(ref store) = self.store else { return };
        let guard = self.entries.read().await;
        let json = match serde_json::to_string(&*guard) {
            Ok(j) => j,
            Err(e) => {
                warn!("btw: failed to serialize entries: {e}");
                return;
            }
        };
        if let Err(e) = store.kv_set(BTW_STORE_KEY, &json) {
            warn!("btw: failed to persist to redb: {e}");
        }
    }

    fn load_from_redb(&mut self) {
        let Some(ref store) = self.store else { return };
        let json_str = match store.kv_get(BTW_STORE_KEY) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                warn!("btw: failed to load from redb: {e}");
                return;
            }
        };
        let entries: Vec<BtwEntry> = match serde_json::from_str(&json_str) {
            Ok(v) => v,
            Err(e) => {
                warn!("btw: failed to deserialize entries: {e}");
                return;
            }
        };
        let max_id = entries.iter().map(|e| e.id).max().unwrap_or(0);
        // Blocking writes are fine during init (no contention yet).
        // Use try_write during init -- safe because no contention at startup.
        if let Ok(mut guard) = self.entries.try_write() {
            *guard = entries;
        }
        if let Ok(mut guard) = self.next_id.try_write() {
            *guard = max_id + 1;
        }
    }
}

// -------------------------------------------------------------------------
// Helpers
// -------------------------------------------------------------------------

/// Check whether an entry's scope matches the given session/channel.
fn scope_matches(entry: &BtwEntry, session_key: &str, channel: &str) -> bool {
    match &entry.scope {
        BtwScope::Session(s) => s == session_key,
        BtwScope::Channel(c) => c == channel,
        BtwScope::Global => true,
    }
}

/// Format a list of entries into the XML prompt block.
fn format_prompt_block(entries: &[BtwEntry]) -> String {
    let mut lines = Vec::with_capacity(entries.len() + 2);
    lines.push("<background_context>".to_owned());
    for entry in entries {
        let scope_tag = match &entry.scope {
            BtwScope::Session(_) => "",
            BtwScope::Channel(_) => "/channel",
            BtwScope::Global => "/global",
        };
        let ttl_tag = if let Some(remaining) = entry.remaining_turns {
            format!("{remaining} turns left")
        } else {
            "permanent".to_owned()
        };
        let scope_suffix = if scope_tag.is_empty() {
            String::new()
        } else {
            format!("{scope_tag}")
        };
        lines.push(format!(
            "[{}] ({}{}) {}",
            entry.id, ttl_tag, scope_suffix, entry.content
        ));
    }
    lines.push("</background_context>".to_owned());
    lines.join("\n")
}

/// Simple keyword overlap relevance score (V3).
fn relevance_score(entry_content: &str, user_message: &str) -> f32 {
    let entry_words: HashSet<&str> = entry_content
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    let msg_words: HashSet<&str> = user_message
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    let overlap = entry_words.intersection(&msg_words).count();
    overlap as f32 / entry_words.len().max(1) as f32
}
