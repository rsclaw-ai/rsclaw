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

    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    /// Append a message to a session. Returns the new sequence number.
    pub fn append_message(&self, session_key: &str, message: &serde_json::Value) -> Result<u64> {
        let meta_opt = self.get_session_meta(session_key)?;
        let mut meta = meta_opt.unwrap_or_else(|| SessionMeta {
            session_key: session_key.to_owned(),
            message_count: 0,
            last_active: chrono::Utc::now().timestamp(),
            created_at: chrono::Utc::now().timestamp(),
        });

        let seq = meta.message_count;
        meta.message_count += 1;
        meta.last_active = chrono::Utc::now().timestamp();

        let msg_key = format!("{session_key}:{seq:016}");
        let msg_json = serde_json::to_string(message)?;

        let write = self.db.begin_write()?;
        {
            let mut msgs = write.open_table(MESSAGES)?;
            msgs.insert(msg_key.as_str(), msg_json.as_str())?;

            let meta_json = serde_json::to_string(&meta)?;
            let mut metas = write.open_table(SESSION_META)?;
            metas.insert(session_key, meta_json.as_str())?;
        }
        write.commit()?;

        Ok(seq)
    }

    /// Load all messages for a session, in order.
    pub fn load_messages(&self, session_key: &str) -> Result<Vec<serde_json::Value>> {
        let read = self.db.begin_read()?;
        let table = read.open_table(MESSAGES)?;
        let prefix = format!("{session_key}:");

        let messages: Vec<serde_json::Value> = table
            .range(prefix.as_str()..)?
            .take_while(|r| {
                r.as_ref()
                    .map(|(k, _)| k.value().starts_with(&prefix))
                    .unwrap_or(false)
            })
            .filter_map(|r| r.ok())
            .filter_map(|(_, v)| serde_json::from_str(v.value()).ok())
            .collect();

        Ok(messages)
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
