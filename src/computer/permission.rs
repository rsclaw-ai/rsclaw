//! Permission gate — pre-execution consent flow.
//!
//! Protocol:
//!
//!   1. Before VlmDriver enters its loop, it calls
//!      `PermissionStore::check(...)`.
//!   2. If a persistent grant exists for this (agent_id, app) pair,
//!      return immediately.
//!   3. Else the caller emits a `PermissionRequest` event on the
//!      gateway's broadcast bus — the desktop UI subscribes via WS,
//!      surfaces a modal, and posts back a `PermissionResponse` on a
//!      new WS method which resolves a oneshot registered via
//!      `register_pending_request`.
//!   4. The driver awaits the oneshot, calls `record(...)`, and
//!      proceeds (or aborts on `Deny`).
//!
//! Bypass mode: a global `bypass_all` flag in the runtime config
//! short-circuits the check (returns `AllowAlways` immediately). Used
//! by power users / CI runs.
//!
//! Storage: redb table `computer_permissions`, keyed by
//! `{agent_id}\0{app_name}`, value JSON `{decision, granted_at}`.
//! Only `AllowAlways` writes through to redb; `AllowSession`, `Deny`,
//! and `AllowOnce` (for the duration of the call) live in the
//! in-memory session map.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, oneshot};
use tracing::{info, warn};

use crate::store::redb_store::RedbStore;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// Just for this single ui_tars run.
    Once,
    /// All ui_tars runs in this gateway session.
    Session,
    /// Persisted to redb; survives gateway restarts.
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub request_id: String,
    pub agent_id: String,
    /// Target app display name, e.g. "WeChat" / "Doubao". Empty when
    /// the operator is generic-desktop and no app is identified.
    pub app: String,
    /// Plain-language summary shown in the UI modal.
    pub reason: String,
    /// Estimate of action count (`max_loop`) so the user knows the
    /// scope of what they're approving.
    pub estimated_steps: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    AllowOnce,
    AllowSession,
    AllowAlways,
    Deny,
}

pub type CheckFut<'a> =
    Pin<Box<dyn Future<Output = Result<Option<PermissionDecision>>> + Send + 'a>>;
pub type RecordFut<'a> = Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

pub trait PermissionStore: Send + Sync {
    /// Returns the cached / persisted decision for this (agent, app)
    /// or `None` if the user has never decided. The driver then emits
    /// a request and awaits a response.
    fn check<'a>(&'a self, agent_id: &'a str, app: &'a str) -> CheckFut<'a>;

    /// Record a decision. `Once` is not persisted; `Session` is held
    /// in memory until the gateway restarts; `Always` writes to redb.
    fn record<'a>(
        &'a self,
        agent_id: &'a str,
        app: &'a str,
        decision: PermissionDecision,
    ) -> RecordFut<'a>;

    /// Revoke a persistent grant (UI "Forget this app").
    fn revoke<'a>(&'a self, agent_id: &'a str, app: &'a str) -> RecordFut<'a>;

    /// True when bypass-mode is active. Driver short-circuits when
    /// this is true.
    fn bypass_all(&self) -> bool;
}

// ---------------------------------------------------------------------------
// On-disk record (JSON-encoded value column of `computer_permissions`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedGrant {
    decision: PermissionDecision,
    granted_at: i64,
}

// ---------------------------------------------------------------------------
// In-memory cache entry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct CachedDecision {
    decision: PermissionDecision,
    #[allow(dead_code)] // surfaced via UI later (audit log)
    scope: GrantScope,
    #[allow(dead_code)] // surfaced via UI later (audit log)
    granted_at: i64,
}

// ---------------------------------------------------------------------------
// RedbPermissionStore — session map + redb-backed persistent grants
// ---------------------------------------------------------------------------

/// Compose the redb key. NUL is used as the separator because neither
/// agent_id nor app names contain it in practice.
fn compose_key(agent_id: &str, app: &str) -> String {
    format!("{agent_id}\0{app}")
}

/// In-memory + redb-backed `PermissionStore` implementation.
///
/// Cloneable via `Arc` because `RedbStore` is held behind one and the
/// session map is `RwLock<HashMap<...>>`.
pub struct RedbPermissionStore {
    /// Session-scoped cache. Cleared on gateway restart.
    sessions: RwLock<HashMap<(String, String), CachedDecision>>,
    /// Persistent backing store (shared with the rest of the gateway).
    redb: Arc<RedbStore>,
    /// Bypass switch — when true, every `check` returns `AllowAlways`.
    /// Wired from `crate::config::schema` runtime config.
    bypass_all: bool,
    /// Pending UI requests awaiting user decision. Keyed by
    /// `request_id` (the same value carried in `PermissionRequest`).
    /// The WS handler resolves these by calling
    /// `resolve_pending_request`.
    pending: RwLock<HashMap<String, oneshot::Sender<PermissionDecision>>>,
}

impl RedbPermissionStore {
    /// Build a new store. `bypass_all = true` short-circuits every
    /// permission check (used by `--allow-all` style power-user flags
    /// and CI).
    pub fn new(redb: Arc<RedbStore>, bypass_all: bool) -> Self {
        if bypass_all {
            warn!("computer-use permission gate: bypass_all = true (every action auto-approved)");
        }
        Self {
            sessions: RwLock::new(HashMap::new()),
            redb,
            bypass_all,
            pending: RwLock::new(HashMap::new()),
        }
    }

    /// Register a oneshot for a pending UI request. Called by the code
    /// that emits the `PermissionRequest` event right before awaiting
    /// the user's decision.
    ///
    /// TODO: wire this from the driver — the driver should:
    ///   1. mint a `request_id`
    ///   2. call `register_pending_request(request_id) -> Receiver`
    ///   3. emit the `PermissionRequest` event on the gateway bus
    ///   4. `.await` the receiver
    ///   5. call `record(...)` with the decision
    pub async fn register_pending_request(
        &self,
        request_id: &str,
    ) -> oneshot::Receiver<PermissionDecision> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.write().await;
        pending.insert(request_id.to_owned(), tx);
        rx
    }

    /// Resolve a pending UI request with the user's decision. Called
    /// by the WS handler that receives `chat.permission_response` from
    /// the desktop UI.
    ///
    /// Returns true if the request_id was found and resolved, false if
    /// it was unknown (race with timeout / duplicate response).
    ///
    /// TODO: wire this from the WS dispatcher in `src/ws/`.
    pub async fn resolve_pending_request(
        &self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> bool {
        let mut pending = self.pending.write().await;
        match pending.remove(request_id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Read-through: redb → session cache → return.
    async fn load_persistent(
        &self,
        agent_id: &str,
        app: &str,
    ) -> Result<Option<PermissionDecision>> {
        let key = compose_key(agent_id, app);
        let raw = self.redb.permission_get(&key)?;
        let Some(json) = raw else {
            return Ok(None);
        };
        let grant: PersistedGrant = match serde_json::from_str(&json) {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, key = %key, "corrupt permission grant in redb, ignoring");
                return Ok(None);
            }
        };
        // Cache it so subsequent checks skip redb.
        let mut sessions = self.sessions.write().await;
        sessions.insert(
            (agent_id.to_owned(), app.to_owned()),
            CachedDecision {
                decision: grant.decision,
                scope: GrantScope::Always,
                granted_at: grant.granted_at,
            },
        );
        Ok(Some(grant.decision))
    }
}

impl PermissionStore for RedbPermissionStore {
    fn check<'a>(&'a self, agent_id: &'a str, app: &'a str) -> CheckFut<'a> {
        Box::pin(async move {
            if self.bypass_all {
                return Ok(Some(PermissionDecision::AllowAlways));
            }

            // 1. Session cache hit?
            {
                let sessions = self.sessions.read().await;
                if let Some(cached) = sessions.get(&(agent_id.to_owned(), app.to_owned())) {
                    return Ok(Some(cached.decision));
                }
            }

            // 2. redb fallback (writes through to session cache on hit).
            self.load_persistent(agent_id, app).await
        })
    }

    fn record<'a>(
        &'a self,
        agent_id: &'a str,
        app: &'a str,
        decision: PermissionDecision,
    ) -> RecordFut<'a> {
        Box::pin(async move {
            let now = chrono::Utc::now().timestamp();
            match decision {
                PermissionDecision::AllowOnce => {
                    // Caller honors it for this single run; we keep no
                    // memory of it so the next call re-prompts.
                    info!(agent_id, app, "permission: allow_once (no cache)");
                }
                PermissionDecision::AllowSession => {
                    let mut sessions = self.sessions.write().await;
                    sessions.insert(
                        (agent_id.to_owned(), app.to_owned()),
                        CachedDecision {
                            decision,
                            scope: GrantScope::Session,
                            granted_at: now,
                        },
                    );
                    info!(agent_id, app, "permission: allow_session (cached)");
                }
                PermissionDecision::AllowAlways => {
                    let key = compose_key(agent_id, app);
                    let value = serde_json::to_string(&PersistedGrant {
                        decision,
                        granted_at: now,
                    })?;
                    self.redb.permission_put(&key, &value)?;
                    let mut sessions = self.sessions.write().await;
                    sessions.insert(
                        (agent_id.to_owned(), app.to_owned()),
                        CachedDecision {
                            decision,
                            scope: GrantScope::Always,
                            granted_at: now,
                        },
                    );
                    info!(agent_id, app, "permission: allow_always (persisted)");
                }
                PermissionDecision::Deny => {
                    // Cached so we don't keep re-prompting in this
                    // session, but NOT persisted — a fresh gateway
                    // process should re-ask.
                    let mut sessions = self.sessions.write().await;
                    sessions.insert(
                        (agent_id.to_owned(), app.to_owned()),
                        CachedDecision {
                            decision,
                            scope: GrantScope::Session,
                            granted_at: now,
                        },
                    );
                    info!(agent_id, app, "permission: deny (cached for session)");
                }
            }
            Ok(())
        })
    }

    fn revoke<'a>(&'a self, agent_id: &'a str, app: &'a str) -> RecordFut<'a> {
        Box::pin(async move {
            let key = compose_key(agent_id, app);
            self.redb.permission_delete(&key)?;
            let mut sessions = self.sessions.write().await;
            sessions.remove(&(agent_id.to_owned(), app.to_owned()));
            info!(agent_id, app, "permission: revoked");
            Ok(())
        })
    }

    fn bypass_all(&self) -> bool {
        self.bypass_all
    }
}

// ---------------------------------------------------------------------------
// UI specification (Tauri / Next.js half — tracked separately, not
// implemented in this file). See `src/computer/permission_ui.md` for
// the full spec consumed by the ui-dev role.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryTier;

    fn open_store(bypass: bool) -> (RedbPermissionStore, Arc<RedbStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let redb = Arc::new(
            RedbStore::open(&dir.path().join("perm.redb"), MemoryTier::Low).expect("open redb"),
        );
        let store = RedbPermissionStore::new(redb.clone(), bypass);
        (store, redb, dir)
    }

    #[tokio::test]
    async fn fresh_store_returns_none() {
        let (store, _redb, _dir) = open_store(false);
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn allow_session_is_cached_in_memory() {
        let (store, _redb, _dir) = open_store(false);
        store
            .record("agent:a", "WeChat", PermissionDecision::AllowSession)
            .await
            .expect("record");
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, Some(PermissionDecision::AllowSession));
    }

    #[tokio::test]
    async fn allow_once_is_not_cached() {
        let (store, _redb, _dir) = open_store(false);
        store
            .record("agent:a", "WeChat", PermissionDecision::AllowOnce)
            .await
            .expect("record");
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn allow_always_persists_across_store_instances() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("perm.redb");

        // First store — record AllowAlways.
        {
            let redb = Arc::new(RedbStore::open(&path, MemoryTier::Low).expect("open 1"));
            let store = RedbPermissionStore::new(redb, false);
            store
                .record("agent:a", "WeChat", PermissionDecision::AllowAlways)
                .await
                .expect("record");
        }

        // Second store — fresh process, must read it from disk.
        let redb = Arc::new(RedbStore::open(&path, MemoryTier::Low).expect("open 2"));
        let store = RedbPermissionStore::new(redb, false);
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, Some(PermissionDecision::AllowAlways));
    }

    #[tokio::test]
    async fn revoke_clears_session_and_persistent() {
        let (store, _redb, _dir) = open_store(false);
        store
            .record("agent:a", "WeChat", PermissionDecision::AllowAlways)
            .await
            .expect("record");
        store.revoke("agent:a", "WeChat").await.expect("revoke");
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn bypass_all_short_circuits() {
        let (store, _redb, _dir) = open_store(true);
        let got = store.check("agent:a", "WeChat").await.expect("check");
        assert_eq!(got, Some(PermissionDecision::AllowAlways));
        assert!(store.bypass_all());
    }

    #[tokio::test]
    async fn deny_is_cached_for_session_but_not_persisted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("perm.redb");

        {
            let redb = Arc::new(RedbStore::open(&path, MemoryTier::Low).expect("open 1"));
            let store = RedbPermissionStore::new(redb, false);
            store
                .record("agent:a", "WeChat", PermissionDecision::Deny)
                .await
                .expect("record");
            // Same instance: deny is cached.
            assert_eq!(
                store.check("agent:a", "WeChat").await.expect("check 1"),
                Some(PermissionDecision::Deny)
            );
        }

        // Fresh instance: deny was NOT persisted.
        let redb = Arc::new(RedbStore::open(&path, MemoryTier::Low).expect("open 2"));
        let store = RedbPermissionStore::new(redb, false);
        assert_eq!(
            store.check("agent:a", "WeChat").await.expect("check 2"),
            None
        );
    }

    #[tokio::test]
    async fn pending_request_round_trip() {
        let (store, _redb, _dir) = open_store(false);
        let req_id = "req-123";
        let rx = store.register_pending_request(req_id).await;
        let resolved = store
            .resolve_pending_request(req_id, PermissionDecision::AllowOnce)
            .await;
        assert!(resolved);
        let got = rx.await.expect("recv");
        assert_eq!(got, PermissionDecision::AllowOnce);

        // Second resolve is a no-op.
        let again = store
            .resolve_pending_request(req_id, PermissionDecision::Deny)
            .await;
        assert!(!again);
    }
}
