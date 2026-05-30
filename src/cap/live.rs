//! `CapLiveManager` — interactive multi-instance cap sessions.
//!
//! This sits next to `CapAgentManager` (fire-and-forget task mode) and
//! supports a different shape: the LLM (or a direct UI/CLI client) opens
//! one or more long-lived cap driver sessions, keyed by a returned
//! `session_id`, then sends a series of prompts and synchronously
//! receives each response. The driver subprocess stays warm between
//! turns so its internal context accumulates (claudecode/openclaude
//! both retain conversation memory across `Prompt` frames).
//!
//! Designed for two callers:
//!   - **Orchestration mode (LLM):** the agent calls a `cap_live` tool
//!     with `(agent, task, session_id?)`. First call returns a fresh
//!     session_id; subsequent calls pass it back to continue the same
//!     subagent. Typical pattern: codex designs → claude implements →
//!     opencode reviews, all in one IM turn, with the main LLM acting
//!     as conductor.
//!   - **Direct mode (IM `/cap` command, CLI, UI panel):** later phases
//!     bind an IM session sticky to a live session_id so user messages
//!     bypass the main LLM and flow straight to the driver.
//!
//! Resource governance:
//!   - Global cap (`max_sessions`, default 8) — over-spawn returns an
//!     error so a runaway LLM can't drown the host in driver processes.
//!   - Per-session idle GC (`idle_timeout`, default 10 min) — sessions
//!     that haven't received a prompt are torn down on the next
//!     allocation attempt.
//!
//! Lifecycle: `dispatch_sync(.., session_id=None)` spawns + returns
//! id; `dispatch_sync(.., session_id=Some)` reuses; `end_session(id)`
//! force-closes; idle GC closes silently.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};

use cap_rs::core::{ClientFrame, Content};
use cap_rs::driver::Driver;

use super::AgentKind;
use super::runtime::{NotifTarget, run_turn, spawn_driver};

const DEFAULT_MAX_SESSIONS: usize = 8;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 min
/// Per-prompt timeout, matching cap task mode (`runtime.rs` actor).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

pub(crate) struct CapLiveManager {
    sessions: Arc<RwLock<HashMap<String, LiveSessionHandle>>>,
    bus: broadcast::Sender<crate::events::AgentEvent>,
    max_sessions: usize,
    idle_timeout: Duration,
}

#[derive(Clone)]
struct LiveSessionHandle {
    agent_kind: AgentKind,
    tx: mpsc::Sender<LiveRequest>,
    /// Bumped to `Instant::now()` every time a prompt arrives. The GC
    /// reads this to decide whether a session is idle-eligible.
    last_active: Arc<StdMutex<Instant>>,
}

enum LiveRequest {
    Prompt {
        task: String,
        /// Optional IM notification target. When set, the driver's
        /// inner tool-call progress + completion summary is pushed live
        /// via `run_turn` exactly as in task mode.
        notif: Option<NotifTarget>,
        /// Resolved with the driver's accumulated text reply when
        /// `run_turn` returns Ok, or with an error if anything fails.
        reply: oneshot::Sender<Result<String>>,
    },
    Shutdown,
}

pub(crate) struct LiveDispatchResult {
    pub session_id: String,
    pub agent_kind: AgentKind,
    pub output: String,
}

impl CapLiveManager {
    pub(crate) fn new(bus: broadcast::Sender<crate::events::AgentEvent>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            bus,
            max_sessions: DEFAULT_MAX_SESSIONS,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// Send a prompt to a live cap session. If `session_id` is `None`, a
    /// new session is spawned; if `Some`, the existing session is reused
    /// (and must match `kind` — agents are not interchangeable mid-thread).
    /// The call awaits the driver's full response before returning.
    pub(crate) async fn dispatch_sync(
        &self,
        kind: AgentKind,
        session_id: Option<String>,
        task: String,
        cwd: std::path::PathBuf,
        notif: Option<NotifTarget>,
    ) -> Result<LiveDispatchResult> {
        let sid = match session_id {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                let new_id = uuid::Uuid::new_v4().simple().to_string();
                self.spawn_session(&new_id, kind, &cwd).await?;
                new_id
            }
        };

        let handle = {
            let g = self.sessions.read().await;
            g.get(&sid).cloned().ok_or_else(|| {
                anyhow!(
                    "live session `{sid}` not found (expired by idle GC, ended, \
                     or never created — start a new one by omitting session_id)"
                )
            })?
        };

        if handle.agent_kind != kind {
            return Err(anyhow!(
                "live session `{sid}` is bound to `{}`, cannot route a `{}` prompt to it",
                handle.agent_kind.as_str(),
                kind.as_str()
            ));
        }

        if let Ok(mut g) = handle.last_active.lock() {
            *g = Instant::now();
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .tx
            .send(LiveRequest::Prompt {
                task,
                notif,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("live session `{sid}` actor closed unexpectedly"))?;

        let output = tokio::time::timeout(PROMPT_TIMEOUT, reply_rx)
            .await
            .map_err(|_| {
                anyhow!(
                    "live session `{sid}`: turn timed out after {}s",
                    PROMPT_TIMEOUT.as_secs()
                )
            })?
            .map_err(|_| anyhow!("live session `{sid}`: actor dropped reply"))??;

        Ok(LiveDispatchResult {
            session_id: sid,
            agent_kind: kind,
            output,
        })
    }

    /// Force-close a live session. Idempotent — returns Ok even if the
    /// session was already gone.
    pub(crate) async fn end_session(&self, session_id: &str) -> Result<()> {
        let handle = {
            let mut g = self.sessions.write().await;
            g.remove(session_id)
        };
        if let Some(h) = handle {
            let _ = h.tx.send(LiveRequest::Shutdown).await;
        }
        Ok(())
    }

    /// Enumerate currently-active sessions (id → kind). Useful for IM
    /// `/cap-list` UIs and debug surfaces. Phase 1 has no caller yet —
    /// wired up in Phase 2 along with the `/cap` IM sticky command.
    #[allow(dead_code)]
    pub(crate) async fn list(&self) -> Vec<(String, AgentKind)> {
        let g = self.sessions.read().await;
        g.iter().map(|(k, h)| (k.clone(), h.agent_kind)).collect()
    }

    async fn spawn_session(
        &self,
        session_id: &str,
        kind: AgentKind,
        cwd: &std::path::Path,
    ) -> Result<()> {
        // Reap idle sessions BEFORE checking the limit so capacity
        // pressure doesn't lock callers out behind stale sessions.
        self.gc_idle().await;

        {
            let g = self.sessions.read().await;
            if g.len() >= self.max_sessions {
                return Err(anyhow!(
                    "live session limit reached ({} active); end one via `cap_live_end` first",
                    self.max_sessions
                ));
            }
        }

        let driver = spawn_driver(kind, cwd).await?;
        let (tx, rx) = mpsc::channel::<LiveRequest>(4);
        let last_active = Arc::new(StdMutex::new(Instant::now()));
        let bus = self.bus.clone();
        let sessions_for_gc = Arc::clone(&self.sessions);
        let sid_owned = session_id.to_owned();
        tokio::spawn(actor_loop(
            sid_owned,
            kind,
            driver,
            rx,
            bus,
            sessions_for_gc,
        ));
        let handle = LiveSessionHandle {
            agent_kind: kind,
            tx,
            last_active,
        };
        let mut g = self.sessions.write().await;
        g.insert(session_id.to_owned(), handle);
        Ok(())
    }

    async fn gc_idle(&self) {
        let now = Instant::now();
        let idle = self.idle_timeout;
        let mut to_remove: Vec<String> = Vec::new();
        {
            let g = self.sessions.read().await;
            for (sid, handle) in g.iter() {
                if let Ok(last) = handle.last_active.lock() {
                    if now.duration_since(*last) > idle {
                        to_remove.push(sid.clone());
                    }
                }
            }
        }
        if to_remove.is_empty() {
            return;
        }
        let mut g = self.sessions.write().await;
        for sid in &to_remove {
            if let Some(h) = g.remove(sid) {
                tracing::info!(
                    target: "cap",
                    session_id = %sid,
                    "live session reaped (idle > {}s)",
                    idle.as_secs()
                );
                let _ = h.tx.send(LiveRequest::Shutdown).await;
            }
        }
    }
}

/// Actor loop for one live session: serially process Prompt requests
/// against the held driver, exit on Shutdown or driver failure.
/// On exit, removes its own entry from the sessions map.
async fn actor_loop(
    sid: String,
    kind: AgentKind,
    mut driver: Box<dyn Driver>,
    mut rx: mpsc::Receiver<LiveRequest>,
    bus: broadcast::Sender<crate::events::AgentEvent>,
    sessions: Arc<RwLock<HashMap<String, LiveSessionHandle>>>,
) {
    tracing::info!(
        target: "cap",
        session_id = %sid,
        agent = kind.as_str(),
        "cap_live actor started"
    );
    let pseudo_session_id = format!("cap-live-{}-{sid}", kind.as_str());
    while let Some(req) = rx.recv().await {
        match req {
            LiveRequest::Prompt { task, notif, reply } => {
                if let Err(e) = driver
                    .send(ClientFrame::Prompt {
                        content: vec![Content::text(task)],
                    })
                    .await
                {
                    let _ = reply.send(Err(anyhow!("cap_live driver send: {e}")));
                    break;
                }
                let mut reply_buf = String::new();
                let outcome = run_turn(
                    driver.as_mut(),
                    &bus,
                    &pseudo_session_id,
                    "cap-live",
                    notif.as_ref(),
                    &mut reply_buf,
                )
                .await;
                match outcome {
                    Ok(()) => {
                        let _ = reply.send(Ok(reply_buf));
                    }
                    Err(e) => {
                        // Driver died mid-turn → propagate error, exit
                        // the actor so the manager's GC entry cleanup
                        // below runs; caller must open a new session.
                        let _ = reply.send(Err(anyhow!("cap_live driver: {e}")));
                        break;
                    }
                }
            }
            LiveRequest::Shutdown => break,
        }
    }
    let _ = driver.shutdown().await;
    let mut g = sessions.write().await;
    g.remove(&sid);
    tracing::info!(
        target: "cap",
        session_id = %sid,
        "cap_live actor exited"
    );
}
