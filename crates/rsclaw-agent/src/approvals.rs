//! Interactive approval for commands the exec safety layer would refuse.
//!
//! When a command hits a safety rule or reaches outside the workspace, the
//! exec tool asks the user instead of refusing outright: it publishes an
//! [`ApprovalRequest`], the desktop/mobile client shows it, and the user's
//! answer (`POST /api/v1/approvals/{id}`) resumes the waiting tool call. A
//! grant can cover just this call, the rest of the session, or (for normal
//! risk) every later session; the last kind is persisted.
//!
//! Requests are only raised when a client is listening on the approval
//! stream; channels without a UI (IM bots, cron, the one-shot CLI) keep the
//! old behaviour and refuse with an explanation.
//!
//! The broker is process-wide, like [`crate::trust`]: the exec tool and the
//! HTTP layer reach the same instance without extra wiring.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

/// How long a tool call waits for the user before treating it as a denial.
/// Stays below the five-minute slow-tool warning and the tool dispatch
/// timeout, so an unanswered prompt ends the call cleanly.
pub const APPROVAL_WAIT: Duration = Duration::from_secs(280);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Only the waiting call.
    Once,
    /// Every later call in this session.
    Session,
    /// Every later session; persisted.
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// A command matched a safety rule (`target` is the rule).
    Command,
    /// A command touches a path outside the workspace (`target` is the
    /// directory the grant would cover).
    Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub session_key: String,
    pub agent_id: String,
    pub kind: ApprovalKind,
    /// The full command being run.
    pub command: String,
    /// The rule (kind = command) or directory (kind = path) being approved.
    pub target: String,
    pub risk: Risk,
    /// Scopes the client may offer, narrowest first.
    pub scopes: Vec<Scope>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

/// Outcome of [`ApprovalBroker::request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow(Scope),
    Deny,
    /// No answer within [`APPROVAL_WAIT`].
    TimedOut,
    /// No client is listening, so nobody could be asked.
    NoClient,
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    rules: Vec<String>,
}

/// An answered (or expired) request, announced so every client can drop it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResolved {
    pub id: String,
    pub session_key: String,
    pub allow: bool,
}

pub struct ApprovalBroker {
    tx: broadcast::Sender<ApprovalRequest>,
    resolved_tx: broadcast::Sender<ApprovalResolved>,
    pending: Mutex<HashMap<String, (ApprovalRequest, oneshot::Sender<Decision>)>>,
    session: Mutex<HashMap<String, HashSet<String>>>,
    always: Mutex<HashSet<String>>,
    store: Option<PathBuf>,
}

/// The process-wide broker. Persisted grants live in
/// `<base_dir>/var/data/exec_approvals.json`.
pub fn broker() -> &'static ApprovalBroker {
    static BROKER: OnceLock<ApprovalBroker> = OnceLock::new();
    BROKER.get_or_init(|| {
        ApprovalBroker::new(Some(
            rsclaw_config::loader::base_dir().join("var/data/exec_approvals.json"),
        ))
    })
}

/// Grant key for a safety rule.
pub fn rule_key(pattern: &str) -> String {
    format!("rule:{pattern}")
}

/// Grant key for a directory.
pub fn path_key(dir: &Path) -> String {
    format!("path:{}", dir.display())
}

impl ApprovalBroker {
    pub fn new(store: Option<PathBuf>) -> Self {
        let (tx, _) = broadcast::channel(64);
        let (resolved_tx, _) = broadcast::channel(64);
        let always = store
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|raw| serde_json::from_str::<Persisted>(&raw).ok())
            .map(|p| {
                p.paths
                    .iter()
                    .map(|d| path_key(Path::new(d)))
                    .chain(p.rules.iter().map(|r| rule_key(r)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            tx,
            resolved_tx,
            pending: Mutex::new(HashMap::new()),
            session: Mutex::new(HashMap::new()),
            always: Mutex::new(always),
            store,
        }
    }

    /// New requests, for clients to show.
    pub fn subscribe(&self) -> broadcast::Receiver<ApprovalRequest> {
        self.tx.subscribe()
    }

    /// Answered and expired requests.
    pub fn subscribe_resolved(&self) -> broadcast::Receiver<ApprovalResolved> {
        self.resolved_tx.subscribe()
    }

    /// Whether anyone is listening for requests.
    pub fn has_listener(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Whether `rule` is already granted for `session`.
    pub fn rule_granted(&self, session: &str, rule: &str) -> bool {
        self.granted(session, &rule_key(rule))
    }

    /// Whether `path` lies under a directory granted for `session`.
    pub fn path_granted(&self, session: &str, path: &Path) -> bool {
        let covers = |key: &String| {
            key.strip_prefix("path:")
                .is_some_and(|dir| path.starts_with(Path::new(dir)))
        };
        self.always.lock().map(|a| a.iter().any(covers)).unwrap_or(false)
            || self
                .session
                .lock()
                .map(|s| s.get(session).is_some_and(|g| g.iter().any(covers)))
                .unwrap_or(false)
    }

    fn granted(&self, session: &str, key: &str) -> bool {
        self.always.lock().map(|a| a.contains(key)).unwrap_or(false)
            || self
                .session
                .lock()
                .map(|s| s.get(session).is_some_and(|g| g.contains(key)))
                .unwrap_or(false)
    }

    /// Ask the user and wait (at most `wait`) for the answer.
    pub async fn request(
        &self,
        session_key: &str,
        agent_id: &str,
        kind: ApprovalKind,
        command: &str,
        target: &str,
        risk: Risk,
        wait: Duration,
    ) -> Decision {
        if !self.has_listener() {
            return Decision::NoClient;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let scopes = match risk {
            Risk::Normal => vec![Scope::Once, Scope::Session, Scope::Always],
            // A high-risk grant never outlives the session.
            Risk::High => vec![Scope::Once, Scope::Session],
        };
        let req = ApprovalRequest {
            id: uuid::Uuid::new_v4().simple().to_string(),
            session_key: session_key.to_owned(),
            agent_id: agent_id.to_owned(),
            kind,
            command: command.to_owned(),
            target: target.to_owned(),
            risk,
            scopes,
            created_at_ms: now,
            expires_at_ms: now + wait.as_millis() as i64,
        };
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(req.id.clone(), (req.clone(), tx));
        }
        let id = req.id.clone();
        if self.tx.send(req).is_err() {
            let _ = self.take(&id);
            return Decision::NoClient;
        }
        // Clears the prompt however the wait ends, including when the turn
        // is aborted and this future is dropped.
        let _unanswered = Unanswered { broker: self, id, session_key: session_key.to_owned() };
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => Decision::Deny,
            Err(_) => Decision::TimedOut,
        }
    }

    fn take(&self, id: &str) -> Option<(ApprovalRequest, oneshot::Sender<Decision>)> {
        self.pending.lock().ok()?.remove(id)
    }

    /// Requests still waiting for an answer, optionally for one session
    /// (a client that reconnects mid-turn re-shows them).
    pub fn pending(&self, session: Option<&str>) -> Vec<ApprovalRequest> {
        self.pending
            .lock()
            .map(|p| {
                let mut list: Vec<_> = p
                    .values()
                    .map(|(req, _)| req.clone())
                    .filter(|req| session.is_none_or(|s| req.session_key == s))
                    .collect();
                list.sort_by_key(|r| r.created_at_ms);
                list
            })
            .unwrap_or_default()
    }

    /// Record the user's answer and resume the waiting call. Fails when the
    /// request is unknown (already answered or expired) or the scope is not
    /// one the request offered.
    pub fn respond(&self, id: &str, allow: bool, scope: Scope) -> Result<ApprovalRequest, String> {
        let (req, tx) = {
            let mut pending = self.pending.lock().map_err(|_| "approval state poisoned")?;
            let Some((req, _)) = pending.get(id) else {
                return Err("approval request not found or already answered".into());
            };
            if allow && !req.scopes.contains(&scope) {
                return Err(format!("scope {scope:?} is not offered for this request"));
            }
            pending.remove(id).expect("checked above")
        };
        if allow {
            let key = match req.kind {
                ApprovalKind::Command => rule_key(&req.target),
                ApprovalKind::Path => path_key(Path::new(&req.target)),
            };
            match scope {
                Scope::Once => {}
                Scope::Session => {
                    if let Ok(mut s) = self.session.lock() {
                        s.entry(req.session_key.clone()).or_default().insert(key);
                    }
                }
                Scope::Always => {
                    if let Ok(mut a) = self.always.lock() {
                        a.insert(key);
                    }
                    self.persist();
                }
            }
        }
        let _ = tx.send(if allow { Decision::Allow(scope) } else { Decision::Deny });
        let _ = self.resolved_tx.send(ApprovalResolved {
            id: req.id.clone(),
            session_key: req.session_key.clone(),
            allow,
        });
        Ok(req)
    }

    fn persist(&self) {
        let Some(path) = self.store.as_deref() else {
            return;
        };
        let Ok(always) = self.always.lock() else {
            return;
        };
        let mut data = Persisted::default();
        for key in always.iter() {
            if let Some(dir) = key.strip_prefix("path:") {
                data.paths.push(dir.to_owned());
            } else if let Some(rule) = key.strip_prefix("rule:") {
                data.rules.push(rule.to_owned());
            }
        }
        data.paths.sort();
        data.rules.sort();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(raw) = serde_json::to_string_pretty(&data) {
            let _ = rsclaw_config::loader::write_file_atomic(path, &raw);
        }
    }
}

/// Withdraws a request that is still pending when its wait ends.
struct Unanswered<'a> {
    broker: &'a ApprovalBroker,
    id: String,
    session_key: String,
}

impl Drop for Unanswered<'_> {
    fn drop(&mut self) {
        if self.broker.take(&self.id).is_some() {
            // Never answered: tell clients to drop the prompt.
            let _ = self.broker.resolved_tx.send(ApprovalResolved {
                id: std::mem::take(&mut self.id),
                session_key: std::mem::take(&mut self.session_key),
                allow: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker_with_listener() -> (ApprovalBroker, broadcast::Receiver<ApprovalRequest>) {
        let b = ApprovalBroker::new(None);
        let rx = b.subscribe();
        (b, rx)
    }

    #[tokio::test]
    async fn no_listener_means_no_request() {
        let b = ApprovalBroker::new(None);
        let d = b
            .request("s", "main", ApprovalKind::Command, "sudo ls", "\\bsudo\\b", Risk::High, APPROVAL_WAIT)
            .await;
        assert_eq!(d, Decision::NoClient);
    }

    #[tokio::test]
    async fn allow_session_grants_later_calls() {
        let (b, mut rx) = broker_with_listener();
        let b = std::sync::Arc::new(b);
        let waiter = {
            let b = b.clone();
            tokio::spawn(async move {
                b.request("s1", "main", ApprovalKind::Path, "ls ~/Desktop", "/Users/u/Desktop", Risk::Normal, APPROVAL_WAIT)
                    .await
            })
        };
        let req = rx.recv().await.expect("request published");
        assert_eq!(b.pending(Some("s1")).len(), 1);
        b.respond(&req.id, true, Scope::Session).expect("respond");
        assert_eq!(waiter.await.unwrap(), Decision::Allow(Scope::Session));
        assert!(b.path_granted("s1", Path::new("/Users/u/Desktop/case/a.pdf")));
        assert!(!b.path_granted("s2", Path::new("/Users/u/Desktop/case/a.pdf")));
        assert!(b.pending(None).is_empty());
    }

    #[tokio::test]
    async fn deny_and_unknown_and_disallowed_scope() {
        let (b, mut rx) = broker_with_listener();
        let b = std::sync::Arc::new(b);
        let waiter = {
            let b = b.clone();
            tokio::spawn(async move {
                b.request("s", "main", ApprovalKind::Command, "sudo ls", "\\bsudo\\b", Risk::High, APPROVAL_WAIT)
                    .await
            })
        };
        let req = rx.recv().await.unwrap();
        assert!(b.respond(&req.id, true, Scope::Always).is_err(), "high risk can't be always");
        b.respond(&req.id, false, Scope::Once).unwrap();
        assert_eq!(waiter.await.unwrap(), Decision::Deny);
        assert!(b.respond(&req.id, true, Scope::Once).is_err(), "already answered");
        assert!(!b.rule_granted("s", "\\bsudo\\b"));
    }

    #[tokio::test]
    async fn times_out() {
        let (b, _rx) = broker_with_listener();
        let d = b
            .request("s", "main", ApprovalKind::Command, "x", "r", Risk::Normal, Duration::from_millis(20))
            .await;
        assert_eq!(d, Decision::TimedOut);
        assert!(b.pending(None).is_empty());
    }

    #[tokio::test]
    async fn aborted_wait_withdraws_the_prompt() {
        let (b, mut rx) = broker_with_listener();
        let mut resolved = b.subscribe_resolved();
        let wait = b.request("s", "main", ApprovalKind::Command, "x", "r", Risk::Normal, APPROVAL_WAIT);
        tokio::select! {
            _ = wait => panic!("nobody answered"),
            r = rx.recv() => assert!(r.is_ok()),
        }
        assert!(b.pending(None).is_empty());
        assert!(!resolved.try_recv().unwrap().allow);
    }

    #[tokio::test]
    async fn always_grants_persist() {
        let dir = std::env::temp_dir().join(format!("rsclaw-approvals-{}", uuid::Uuid::new_v4()));
        let store = dir.join("exec_approvals.json");
        let b = std::sync::Arc::new(ApprovalBroker::new(Some(store.clone())));
        let mut rx = b.subscribe();
        let waiter = {
            let b = b.clone();
            tokio::spawn(async move {
                b.request("s", "main", ApprovalKind::Path, "ls /data/x", "/data", Risk::Normal, APPROVAL_WAIT)
                    .await
            })
        };
        let req = rx.recv().await.unwrap();
        b.respond(&req.id, true, Scope::Always).unwrap();
        waiter.await.unwrap();
        let reloaded = ApprovalBroker::new(Some(store));
        assert!(reloaded.path_granted("other", Path::new("/data/x/y")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
