//! redb KV store — hot data (session history, pairing state, agent metadata).
//!
//! Memory limits (AGENTS.md §18 "Iron Rules"):
//!   Low tier    →  16 MB cache
//!   Standard    →  32 MB cache
//!   High        →  64 MB cache
//!
//! Tables:
//!   SESSION_META  — session_key → JSON metadata (last_active, message_count …)
//!   MESSAGES      — session_key:seq_no → JSON message
//!   PAIRING       — channel:peer_id → pairing_state JSON
//!   KV            — generic string key → string value (for agent scratch
//! storage)

use std::path::Path;

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, TableDefinition};
#[allow(unused_imports)]
use serde::{Serialize, de::DeserializeOwned};
use tracing::debug;

use crate::MemoryTier;

// ---------------------------------------------------------------------------
// Table definitions
// ---------------------------------------------------------------------------

/// Session metadata: session_key → JSON string.
const SESSION_META: TableDefinition<&str, &str> = TableDefinition::new("session_meta");

/// Message store: "<session_key>:<seq>" → JSON string.
const MESSAGES: TableDefinition<&str, &str> = TableDefinition::new("messages");

/// Pairing state: "<channel>:<peer_id>" → JSON string.
const PAIRING: TableDefinition<&str, &str> = TableDefinition::new("pairing");

/// Generic KV scratch space for agents/skills.
const KV: TableDefinition<&str, &str> = TableDefinition::new("kv");

/// Session alias table: alias_key → canonical session_key.
/// Used for migration compatibility (OpenClaw keys, format upgrades).
const SESSION_ALIASES: TableDefinition<&str, &str> = TableDefinition::new("session_aliases");

/// Task queue: task_id → JSON-serialized QueuedTask.
const TASK_QUEUE: TableDefinition<&str, &str> = TableDefinition::new("task_queue");

// ---------------------------------------------------------------------------
// RedbStore
// ---------------------------------------------------------------------------

pub struct RedbStore {
    db: Database,
}

impl std::fmt::Debug for RedbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedbStore").finish_non_exhaustive()
    }
}

impl RedbStore {
    /// Open (or create) the redb database at `path`.
    pub fn open(path: &Path, tier: MemoryTier) -> Result<Self> {
        let cache_bytes: usize = match tier {
            MemoryTier::Low => 16 * 1024 * 1024,      // 16 MB
            MemoryTier::Standard => 32 * 1024 * 1024, // 32 MB
            MemoryTier::High => 64 * 1024 * 1024,     // 64 MB
        };

        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)
            .with_context(|| format!("open redb at {}", path.display()))?;

        // Ensure all tables exist.
        let write = db.begin_write().context("begin write (init tables)")?;
        {
            write
                .open_table(SESSION_META)
                .context("init SESSION_META")?;
            write.open_table(MESSAGES).context("init MESSAGES")?;
            write.open_table(PAIRING).context("init PAIRING")?;
            write.open_table(KV).context("init KV")?;
            write
                .open_table(SESSION_ALIASES)
                .context("init SESSION_ALIASES")?;
            write
                .open_table(TASK_QUEUE)
                .context("init TASK_QUEUE")?;
        }
        write.commit().context("commit init")?;

        debug!(path = %path.display(), cache_mb = cache_bytes / (1024*1024), "redb opened");
        Ok(Self { db })
    }

    // -----------------------------------------------------------------------
    // Session metadata
    // -----------------------------------------------------------------------

    pub fn get_session_meta(&self, session_key: &str) -> Result<Option<SessionMeta>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSION_META)?;
        match table.get(session_key)? {
            Some(guard) => {
                let v: SessionMeta = serde_json::from_str(guard.value())?;
                Ok(Some(v))
            }
            None => Ok(None),
        }
    }

    pub fn put_session_meta(&self, session_key: &str, meta: &SessionMeta) -> Result<()> {
        let json = serde_json::to_string(meta)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SESSION_META)?;
            table.insert(session_key, json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete_session(&self, session_key: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut meta = write.open_table(SESSION_META)?;
            meta.remove(session_key)?;
            // Also remove all messages for this session.
            let mut msgs = write.open_table(MESSAGES)?;
            let prefix = format!("{session_key}:");
            let keys: Vec<String> = msgs
                .range(prefix.as_str()..)?
                .take_while(|r| {
                    r.as_ref()
                        .map(|(k, _)| k.value().starts_with(&prefix))
                        .unwrap_or(false)
                })
                .filter_map(|r| r.ok())
                .map(|(k, _)| k.value().to_owned())
                .collect();
            for key in &keys {
                msgs.remove(key.as_str())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// List all session keys.
    pub fn list_sessions(&self) -> Result<Vec<String>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSION_META)?;
        let keys = table
            .range::<&str>(..)?
            .filter_map(|r| r.ok())
            .map(|(k, _)| k.value().to_owned())
            .collect();
        Ok(keys)
    }

    /// Increment the generation counter for a session and reset message_count.
    /// Called by `/new` to start a fresh conversation on the same session key.
    /// Active messages are deleted; archive is untouched.
    pub fn new_generation(&self, session_key: &str) -> Result<u32> {
        let meta_opt = self.get_session_meta(session_key)?;
        let mut meta = meta_opt.unwrap_or_else(|| SessionMeta {
            session_key: session_key.to_owned(),
            message_count: 0,
            last_active: chrono::Utc::now().timestamp(),
            created_at: chrono::Utc::now().timestamp(),
            generation: 1,
        });

        meta.generation += 1;
        meta.message_count = 0;
        meta.last_active = chrono::Utc::now().timestamp();

        // Delete active messages (not archive).
        let write = self.db.begin_write()?;
        {
            let mut msgs = write.open_table(MESSAGES)?;
            let prefix = format!("{session_key}:");
            let keys: Vec<String> = msgs
                .range(prefix.as_str()..)?
                .take_while(|r| {
                    r.as_ref()
                        .map(|(k, _)| k.value().starts_with(&prefix))
                        .unwrap_or(false)
                })
                .filter_map(|r| r.ok())
                .map(|(k, _)| k.value().to_owned())
                .collect();
            for key in &keys {
                msgs.remove(key.as_str())?;
            }

            let meta_json = serde_json::to_string(&meta)?;
            let mut metas = write.open_table(SESSION_META)?;
            metas.insert(session_key, meta_json.as_str())?;
        }
        write.commit()?;

        Ok(meta.generation)
    }

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// Append a message to a session. Returns the new sequence number.
    ///
    /// Double-writes: the message is stored under the active session key
    /// (compaction may delete these) AND under an `archive:` prefixed key
    /// (never deleted, preserves complete conversation history).
    pub fn append_message(&self, session_key: &str, message: &serde_json::Value) -> Result<u64> {
        let meta_opt = self.get_session_meta(session_key)?;
        let mut meta = meta_opt.unwrap_or_else(|| SessionMeta {
            session_key: session_key.to_owned(),
            message_count: 0,
            last_active: chrono::Utc::now().timestamp(),
            created_at: chrono::Utc::now().timestamp(),
            generation: 1,
        });

        let seq = meta.message_count;
        meta.message_count += 1;
        meta.last_active = chrono::Utc::now().timestamp();

        let msg_key = format!("{session_key}:{seq:016}");
        let generation = meta.generation;
        let archive_key = format!("archive:{session_key}:gen{generation}:{seq:016}");
        let msg_json = serde_json::to_string(message)?;

        let write = self.db.begin_write()?;
        {
            let mut msgs = write.open_table(MESSAGES)?;
            msgs.insert(msg_key.as_str(), msg_json.as_str())?;
            // Archive: complete history, never deleted by compaction.
            msgs.insert(archive_key.as_str(), msg_json.as_str())?;

            let meta_json = serde_json::to_string(&meta)?;
            let mut metas = write.open_table(SESSION_META)?;
            metas.insert(session_key, meta_json.as_str())?;
        }
        write.commit()?;

        Ok(seq)
    }

    /// Load all messages for a session, in order.
    ///
    /// On first load, if no `archive:` copy exists yet (pre-upgrade sessions),
    /// backfills the archive so complete history is preserved going forward.
    pub fn load_messages(&self, session_key: &str) -> Result<Vec<serde_json::Value>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(MESSAGES)?;
        let prefix = format!("{session_key}:");

        let messages: Vec<(String, serde_json::Value)> = table
            .range(prefix.as_str()..)?
            .take_while(|r| {
                r.as_ref()
                    .map(|(k, _)| k.value().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .filter_map(|r| r.ok())
            .filter_map(|(k, v)| {
                let val: serde_json::Value = serde_json::from_str(v.value()).ok()?;
                Some((k.value().to_owned(), val))
            })
            .collect();

        if messages.is_empty() {
            return Ok(vec![]);
        }

        // Backfill archive for pre-upgrade sessions: if no archive entries
        // exist yet, copy all active messages to archive:...:gen1:... keys.
        let archive_prefix = format!("archive:{session_key}:");
        let has_archive = table
            .range(archive_prefix.as_str()..)?
            .next()
            .is_some_and(|r| {
                r.as_ref()
                    .map(|(k, _)| k.value().starts_with(&archive_prefix))
                    .unwrap_or(false)
            });

        if !has_archive {
            drop(table);
            drop(read);
            if let Ok(write) = self.db.begin_write() {
                if let Ok(mut msgs_table) = write.open_table(MESSAGES) {
                    for (key, val) in &messages {
                        // Pre-upgrade: no generation info, default to gen1.
                        let suffix = key.strip_prefix(&format!("{session_key}:")).unwrap_or("0");
                        let archive_key = format!("archive:{session_key}:gen1:{suffix}");
                        let json_str = serde_json::to_string(val).unwrap_or_default();
                        if let Err(e) = msgs_table.insert(archive_key.as_str(), json_str.as_str()) {
                            tracing::error!(error = %e, key = %archive_key, "failed to insert archive entry");
                        }
                    }
                }
                if let Err(e) = write.commit() {
                    tracing::error!(error = %e, "failed to commit archive backfill transaction");
                }
                debug!("backfilled {} archive entries for session {session_key}", messages.len());
            }
        }

        Ok(messages.into_iter().map(|(_, v)| v).collect())
    }

    // -----------------------------------------------------------------------
    // Pairing state
    // -----------------------------------------------------------------------

    pub fn get_pairing(&self, channel: &str, peer_id: &str) -> Result<Option<PairingState>> {
        let key = format!("{channel}:{peer_id}");
        let read = self.db.begin_read()?;
        let table = read.open_table(PAIRING)?;
        match table.get(key.as_str())? {
            Some(g) => Ok(Some(serde_json::from_str(g.value())?)),
            None => Ok(None),
        }
    }

    pub fn put_pairing(&self, channel: &str, peer_id: &str, state: &PairingState) -> Result<()> {
        let key = format!("{channel}:{peer_id}");
        let json = serde_json::to_string(state)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(PAIRING)?;
            table.insert(key.as_str(), json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// List all approved peer IDs for a channel.
    // TODO: use prefix range query (range(prefix..prefix_end)) instead of full table scan
    pub fn list_pairings(&self, channel: &str) -> Result<Vec<String>> {
        let prefix = format!("{channel}:");
        let read = self.db.begin_read()?;
        let table = read.open_table(PAIRING)?;
        let mut peers = Vec::new();
        for entry in table.iter()? {
            let (key, val) = entry?;
            let k = key.value();
            if k.starts_with(&prefix) {
                if let Ok(state) = serde_json::from_str::<PairingState>(val.value()) {
                    if matches!(state, PairingState::Approved) {
                        peers.push(k[prefix.len()..].to_owned());
                    }
                }
            }
        }
        Ok(peers)
    }

    /// Delete a pairing entry.
    pub fn delete_pairing(&self, channel: &str, peer_id: &str) -> Result<()> {
        let key = format!("{channel}:{peer_id}");
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(PAIRING)?;
            table.remove(key.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Generic KV
    // -----------------------------------------------------------------------

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(KV)?;
        Ok(table.get(key)?.map(|g| g.value().to_owned()))
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(KV)?;
            table.insert(key, value)?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn kv_delete(&self, key: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(KV)?;
            table.remove(key)?;
        }
        write.commit()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Session aliases (migration compatibility)
    // -----------------------------------------------------------------------

    /// Resolve a session key through the alias table.
    /// Returns the canonical key if an alias exists, otherwise None.
    pub fn resolve_session_alias(&self, alias_key: &str) -> Result<Option<String>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSION_ALIASES)?;
        Ok(table.get(alias_key)?.map(|g| g.value().to_owned()))
    }

    /// Add a session alias: alias_key → canonical_key.
    pub fn put_session_alias(&self, alias_key: &str, canonical_key: &str) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SESSION_ALIASES)?;
            table.insert(alias_key, canonical_key)?;
        }
        write.commit()?;
        Ok(())
    }

    /// Batch-insert session aliases.
    pub fn put_session_aliases(&self, aliases: &[(&str, &str)]) -> Result<()> {
        if aliases.is_empty() {
            return Ok(());
        }
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(SESSION_ALIASES)?;
            for (alias_key, canonical_key) in aliases {
                table.insert(*alias_key, *canonical_key)?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Load all session aliases into a HashMap (for in-memory caching).
    pub fn load_all_aliases(&self) -> Result<std::collections::HashMap<String, String>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(SESSION_ALIASES)?;
        let mut map = std::collections::HashMap::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            map.insert(k.value().to_owned(), v.value().to_owned());
        }
        Ok(map)
    }

    // -----------------------------------------------------------------------
    // Task queue
    // -----------------------------------------------------------------------

    /// Enqueue a task. Returns `Ok(())` on success.
    pub fn enqueue_task(&self, task: &crate::gateway::task_queue::QueuedTask) -> Result<()> {
        let json = serde_json::to_string(task)?;
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(TASK_QUEUE)?;
            table.insert(task.id.as_str(), json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Dequeue the highest-priority pending task (lowest priority number,
    /// oldest first). Atomically changes status from Pending to Running.
    pub fn dequeue_task(&self) -> Result<Option<crate::gateway::task_queue::QueuedTask>> {
        use crate::gateway::task_queue::TaskStatus;

        let write = self.db.begin_write()?;
        let result = {
            let mut table = write.open_table(TASK_QUEUE)?;
            let mut best: Option<crate::gateway::task_queue::QueuedTask> = None;

            // Scan all tasks to find the best candidate.
            for entry in table.iter()? {
                let (_k, v) = entry?;
                let task: crate::gateway::task_queue::QueuedTask =
                    serde_json::from_str(v.value())?;
                if task.status != TaskStatus::Pending {
                    continue;
                }
                let dominated = match &best {
                    None => true,
                    Some(b) => {
                        (task.priority, task.created_at) < (b.priority, b.created_at)
                    }
                };
                if dominated {
                    best = Some(task);
                }
            }

            if let Some(mut task) = best {
                task.status = TaskStatus::Running;
                task.updated_at = chrono::Utc::now().timestamp();
                let json = serde_json::to_string(&task)?;
                table.insert(task.id.as_str(), json.as_str())?;
                Some(task)
            } else {
                None
            }
        };
        write.commit()?;
        Ok(result)
    }

    /// Update task status.
    pub fn update_task_status(
        &self,
        task_id: &str,
        status: crate::gateway::task_queue::TaskStatus,
    ) -> Result<()> {
        let write = self.db.begin_write()?;
        {
            let mut table = write.open_table(TASK_QUEUE)?;
            let guard = table
                .get(task_id)?
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            let mut task: crate::gateway::task_queue::QueuedTask =
                serde_json::from_str(guard.value())?;
            drop(guard);
            task.status = status;
            task.updated_at = chrono::Utc::now().timestamp();
            let json = serde_json::to_string(&task)?;
            table.insert(task_id, json.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Mark a task as failed, increment retry count. If retries >= max,
    /// mark as Dead. Returns the resulting status.
    pub fn fail_task(
        &self,
        task_id: &str,
        max_retries: u32,
    ) -> Result<crate::gateway::task_queue::TaskStatus> {
        use crate::gateway::task_queue::TaskStatus;

        let write = self.db.begin_write()?;
        let status = {
            let mut table = write.open_table(TASK_QUEUE)?;
            let guard = table
                .get(task_id)?
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            let mut task: crate::gateway::task_queue::QueuedTask =
                serde_json::from_str(guard.value())?;
            drop(guard);
            task.retries += 1;
            task.updated_at = chrono::Utc::now().timestamp();
            if task.retries >= max_retries {
                task.status = TaskStatus::Dead;
            } else {
                task.status = TaskStatus::Failed;
            }
            let new_status = task.status;
            let json = serde_json::to_string(&task)?;
            table.insert(task_id, json.as_str())?;
            new_status
        };
        write.commit()?;
        Ok(status)
    }

    /// Get a task by ID.
    pub fn get_task(
        &self,
        task_id: &str,
    ) -> Result<Option<crate::gateway::task_queue::QueuedTask>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TASK_QUEUE)?;
        match table.get(task_id)? {
            Some(guard) => {
                let task = serde_json::from_str(guard.value())?;
                Ok(Some(task))
            }
            None => Ok(None),
        }
    }

    /// List tasks, optionally filtered by status.
    pub fn list_tasks(
        &self,
        status: Option<crate::gateway::task_queue::TaskStatus>,
    ) -> Result<Vec<crate::gateway::task_queue::QueuedTask>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(TASK_QUEUE)?;
        let mut tasks = Vec::new();
        for entry in table.iter()? {
            let (_k, v) = entry?;
            let task: crate::gateway::task_queue::QueuedTask =
                serde_json::from_str(v.value())?;
            if let Some(ref s) = status {
                if task.status != *s {
                    continue;
                }
            }
            tasks.push(task);
        }
        Ok(tasks)
    }

    /// Remove expired tasks (past TTL). Returns count removed.
    pub fn cleanup_expired_tasks(&self) -> Result<usize> {
        let write = self.db.begin_write()?;
        let count = {
            let mut table = write.open_table(TASK_QUEUE)?;
            let mut expired_ids = Vec::new();
            for entry in table.iter()? {
                let (_k, v) = entry?;
                let task: crate::gateway::task_queue::QueuedTask =
                    serde_json::from_str(v.value())?;
                if task.is_expired() {
                    expired_ids.push(task.id);
                }
            }
            let count = expired_ids.len();
            for id in &expired_ids {
                table.remove(id.as_str())?;
            }
            count
        };
        write.commit()?;
        Ok(count)
    }

    /// Check if there is a pending task for the same session_key with the
    /// same content hash (dedup guard).
    pub fn has_duplicate(&self, session_key: &str, content_hash: &str) -> Result<bool> {
        use crate::gateway::task_queue::TaskStatus;

        let read = self.db.begin_read()?;
        let table = read.open_table(TASK_QUEUE)?;
        for entry in table.iter()? {
            let (_k, v) = entry?;
            let task: crate::gateway::task_queue::QueuedTask =
                serde_json::from_str(v.value())?;
            if task.session_key == session_key
                && task.content_hash == content_hash
                && task.status == TaskStatus::Pending
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Merge a message into an existing pending task for the same
    /// session_key. Returns `true` if a merge happened.
    pub fn merge_into_pending(
        &self,
        session_key: &str,
        message: &crate::gateway::task_queue::QueuedMessage,
    ) -> Result<bool> {
        use crate::gateway::task_queue::TaskStatus;

        let write = self.db.begin_write()?;
        let merged = {
            let mut table = write.open_table(TASK_QUEUE)?;
            let mut target_id: Option<String> = None;

            for entry in table.iter()? {
                let (_k, v) = entry?;
                let task: crate::gateway::task_queue::QueuedTask =
                    serde_json::from_str(v.value())?;
                if task.session_key == session_key && task.status == TaskStatus::Pending {
                    target_id = Some(task.id);
                    break;
                }
            }

            if let Some(id) = target_id {
                let guard = table
                    .get(id.as_str())?
                    .ok_or_else(|| anyhow::anyhow!("task disappeared: {id}"))?;
                let mut task: crate::gateway::task_queue::QueuedTask =
                    serde_json::from_str(guard.value())?;
                drop(guard);
                task.messages.push(message.clone());
                task.updated_at = chrono::Utc::now().timestamp();
                let json = serde_json::to_string(&task)?;
                table.insert(id.as_str(), json.as_str())?;
                true
            } else {
                false
            }
        };
        write.commit()?;
        Ok(merged)
    }
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMeta {
    pub session_key: String,
    pub message_count: u64,
    pub last_active: i64, // Unix timestamp
    pub created_at: i64,
    /// Archive generation counter. Incremented on `/new` to separate
    /// distinct conversations on the same session key.
    /// Defaults to 1 for new sessions and pre-upgrade sessions (missing field).
    #[serde(default = "default_generation")]
    pub generation: u32,
}

fn default_generation() -> u32 {
    1
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum PairingState {
    Approved,
    Pending { code: String, expires_at: i64 },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (RedbStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            RedbStore::open(&dir.path().join("test.redb"), MemoryTier::Low).expect("open redb");
        (store, dir)
    }

    #[test]
    fn session_meta_round_trip() {
        let (store, _dir) = open_tmp();
        let meta = SessionMeta {
            session_key: "agent:main:telegram:direct:u1".to_owned(),
            message_count: 5,
            last_active: 1_700_000_000,
            created_at: 1_699_000_000,
            generation: 1,
        };
        store
            .put_session_meta(&meta.session_key, &meta)
            .expect("put");
        let got = store.get_session_meta(&meta.session_key).expect("get");
        assert!(got.is_some());
        assert_eq!(got.unwrap().message_count, 5);
    }

    #[test]
    fn append_and_load_messages() {
        let (store, _dir) = open_tmp();
        let sk = "agent:main:cli:direct:user";

        let msg1 = serde_json::json!({"role": "user", "content": "hello"});
        let msg2 = serde_json::json!({"role": "assistant", "content": "hi there"});

        store.append_message(sk, &msg1).expect("append 1");
        store.append_message(sk, &msg2).expect("append 2");

        let msgs = store.load_messages(sk).expect("load");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn delete_session_removes_messages() {
        let (store, _dir) = open_tmp();
        let sk = "agent:main:cli:direct:del_user";

        store
            .append_message(sk, &serde_json::json!({"role": "user", "content": "x"}))
            .expect("append");
        store.delete_session(sk).expect("delete");

        let msgs = store.load_messages(sk).expect("load after delete");
        assert!(msgs.is_empty());
        assert!(store.get_session_meta(sk).expect("meta").is_none());
    }

    #[test]
    fn kv_set_get_delete() {
        let (store, _dir) = open_tmp();
        store.kv_set("my_key", "my_value").expect("set");
        assert_eq!(
            store.kv_get("my_key").expect("get").as_deref(),
            Some("my_value")
        );
        store.kv_delete("my_key").expect("delete");
        assert!(store.kv_get("my_key").expect("get after delete").is_none());
    }

    #[test]
    fn list_sessions() {
        let (store, _dir) = open_tmp();
        let keys = ["sess:a", "sess:b", "sess:c"];
        for k in &keys {
            store
                .append_message(k, &serde_json::json!({}))
                .expect("append");
        }
        let listed = store.list_sessions().expect("list");
        for k in &keys {
            assert!(listed.contains(&k.to_string()), "missing {k}");
        }
    }
}
