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
use super::runtime::{
    NotifTarget, run_turn, spawn_driver, spawn_driver_continue_last, spawn_driver_resume,
};

const DEFAULT_MAX_SESSIONS: usize = 8;
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(600); // 10 min
/// Per-prompt timeout, matching cap task mode (`runtime.rs` actor).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// How a new live session should be opened — fresh, or resuming a
/// prior agent session. Owned-string variant for the by-id case so
/// the value can be threaded through async without lifetime gymnastics.
#[derive(Clone, Debug)]
enum ResumeMode {
    None,
    ById(String),
    ContinueLast,
}

pub struct CapLiveManager {
    sessions: Arc<RwLock<HashMap<String, LiveSessionHandle>>>,
    /// Sticky bindings: IM session_key → (live session_id, agent_kind).
    /// Populated by `/cap <agent>`, read inside `AgentRuntime::run_turn`
    /// to bypass the main LLM for plain-text messages. The map is swept
    /// whenever a live session ends (actor exit, idle GC) so a stale
    /// binding never points at a dead driver.
    sticky: Arc<RwLock<HashMap<String, (String, AgentKind)>>>,
    /// Suspended driver pool: (IM session_key, AgentKind) → live_sid.
    ///
    /// When the user switches `/cap claudecode → /cap codex → /cap
    /// claudecode`, the FIRST claudecode driver isn't torn down —
    /// it's parked here while codex takes the wheel. Coming back to
    /// claudecode pulls the same warm driver out of the pool so the
    /// conversation context (everything the agent saw in turn 1) is
    /// preserved.
    ///
    /// Capacity: one entry per (im_key, kind). Re-binding the same
    /// pair tears down the older parked driver (you can't have two
    /// claudecode subprocesses for the same chat).
    ///
    /// Lifetime: idle GC sweeps pool entries the same way it sweeps
    /// active sessions; the pool only holds live_sids, never owns
    /// the driver subprocess directly (the actor in `sessions` does).
    suspended: Arc<RwLock<HashMap<(String, AgentKind), String>>>,
    bus: broadcast::Sender<rsclaw_events::AgentEvent>,
    /// Outbound notification channel — when set, preparse can post
    /// asynchronous follow-up messages to IM after `/cap` returns its
    /// initial bind reply (e.g. surface the agent's native session_id
    /// once `AgentEvent::Ready` lands in the actor). `None` in test /
    /// embedded contexts that don't have a channel layer.
    notification_tx:
        Option<broadcast::Sender<rsclaw_types::OutboundMessage>>,
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
    /// Agent's NATIVE session id (claudecode UUID, opencode session
    /// id, codex thread_id). Populated by the actor's first event
    /// drain — `AgentEvent::Ready` carries it. Mutex'd so the actor
    /// can write it asynchronously without blocking `spawn_session`.
    ///
    /// `Arc<Mutex<Option<...>>>` instead of letting capture block
    /// the spawn: claudecode's `system/init` frame can land 10-20s
    /// after process spawn (the SessionStart hook runs first and
    /// fires a huge `hook_response` frame), so blocking the
    /// `/cap <agent>` reply on capture made the user wait 10+s for
    /// EVERY bind. With the async slot, `/cap` returns instantly
    /// and `/status` (or a later `get_agent_session_id`) picks up
    /// the captured id once the actor's startup drain finishes.
    agent_session_id: Arc<StdMutex<Option<String>>>,
    /// One-shot flag: when `true`, the runtime's sticky-bypass path
    /// should prepend a memory recall bundle to the first user
    /// message of this binding (giving the cap subagent context
    /// rsclaw already knows — user preferences, prior facts).
    /// Flipped to `false` after the first injection so subsequent
    /// turns don't duplicate the context (the driver process keeps
    /// its own history). Initialised `true` for fresh `/cap <agent>`,
    /// `false` for `/cap-resume` / `--continue-last` (the resumed
    /// session already has whatever context it ended on).
    pending_memory_inject: Arc<std::sync::atomic::AtomicBool>,
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

pub struct LiveDispatchResult {
    pub session_id: String,
    pub agent_kind: AgentKind,
    pub output: String,
}

impl CapLiveManager {
    pub fn new(bus: broadcast::Sender<rsclaw_events::AgentEvent>) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            sticky: Arc::new(RwLock::new(HashMap::new())),
            suspended: Arc::new(RwLock::new(HashMap::new())),
            bus,
            notification_tx: None,
            max_sessions: DEFAULT_MAX_SESSIONS,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
        }
    }

    /// Late-set the outbound notification channel. Called once from
    /// `gateway::startup` after the broadcast channel is created.
    pub fn set_notification_tx(
        &mut self,
        tx: broadcast::Sender<rsclaw_types::OutboundMessage>,
    ) {
        self.notification_tx = Some(tx);
    }

    /// Cheap clone of the outbound notification sender, for preparse-
    /// side follow-up dispatches (e.g. resume-id hint after /cap).
    pub fn notification_tx(
        &self,
    ) -> Option<broadcast::Sender<rsclaw_types::OutboundMessage>> {
        self.notification_tx.clone()
    }

    /// Pull a warm driver out of the suspended pool for (`im_key`, `kind`),
    /// OR spawn a fresh one if the pool is empty (or the entry's driver
    /// died while parked). The caller is expected to follow with
    /// `bind_sticky(im_key, returned_sid, kind)` to wire it up; the
    /// "resume warm driver" semantic is what makes
    /// `/cap claudecode → /cap codex → /cap claudecode` preserve the
    /// first claudecode session's conversation context.
    pub async fn acquire_session(
        &self,
        im_session_key: &str,
        kind: AgentKind,
        cwd: std::path::PathBuf,
    ) -> Result<String> {
        // Pool fast-path: take the parked sid out of the map.
        let resumed = {
            let mut pool = self.suspended.write().await;
            pool.remove(&(im_session_key.to_owned(), kind))
        };
        if let Some(sid) = resumed {
            // Verify the parked driver actually survived (idle GC could
            // have reaped it while it was suspended). If it did, perfect
            // — return its sid for `bind_sticky` to use.
            let alive = self.sessions.read().await.contains_key(&sid);
            if alive {
                tracing::info!(
                    target: "cap",
                    im_session_key,
                    agent = kind.as_str(),
                    session_id = %sid,
                    "cap_live resumed driver from suspended pool"
                );
                return Ok(sid);
            }
            tracing::debug!(
                target: "cap",
                im_session_key,
                agent = kind.as_str(),
                stale_sid = %sid,
                "cap_live suspended pool entry expired; spawning fresh"
            );
        }
        // Cold path: nothing parked, or the parked driver died → fresh.
        self.open_session(kind, cwd).await
    }

    /// Spawn a new live session WITHOUT sending an initial prompt.
    /// Used by the IM `/cap <agent>` slash command which wants to open
    /// a session and bind it sticky before any user message arrives.
    /// Returns the freshly minted `session_id`.
    pub async fn open_session(
        &self,
        kind: AgentKind,
        cwd: std::path::PathBuf,
    ) -> Result<String> {
        let sid = uuid::Uuid::new_v4().simple().to_string();
        self.spawn_session(&sid, kind, &cwd, ResumeMode::None).await?;
        Ok(sid)
    }

    /// Spawn a new live session that RESUMES an existing on-disk
    /// session by the agent's native session id. Like `open_session`
    /// but passes `--resume <id>` (or its agent-specific equivalent)
    /// to the driver subprocess.
    ///
    /// Use case: `/cap-resume claudecode <uuid>` — the user knows the
    /// uuid of an earlier conversation (from `claude /sessions` or the
    /// last-bind reply) and wants to continue it as if no time had
    /// passed.
    pub async fn open_session_resume(
        &self,
        kind: AgentKind,
        cwd: std::path::PathBuf,
        agent_session_id: String,
    ) -> Result<String> {
        let sid = uuid::Uuid::new_v4().simple().to_string();
        self.spawn_session(&sid, kind, &cwd, ResumeMode::ById(agent_session_id))
            .await?;
        Ok(sid)
    }

    /// Spawn a new live session that resumes the MOST RECENT saved
    /// session for this agent + cwd. Equivalent to `claude --continue`
    /// / `opencode --continue` / `codex exec resume --last`.
    /// Use case: `/cap-resume claudecode` (no id) — user wants to
    /// pick up where they left off without typing the uuid.
    pub async fn open_session_continue_last(
        &self,
        kind: AgentKind,
        cwd: std::path::PathBuf,
    ) -> Result<String> {
        let sid = uuid::Uuid::new_v4().simple().to_string();
        self.spawn_session(&sid, kind, &cwd, ResumeMode::ContinueLast)
            .await?;
        Ok(sid)
    }

    /// Bind an IM session key to a live cap session, **parking** any
    /// prior binding's driver in the suspended pool (not tearing it
    /// down). Resume semantics: a later `acquire_session` for the
    /// parked `(im_key, old_kind)` pulls the same warm driver back.
    ///
    /// Pool-collision: if there's already a parked driver for the
    /// SAME `(im_key, old_kind)` we're about to park, the older one
    /// is torn down (one slot per agent per IM session — can't keep
    /// an infinite history).
    pub async fn bind_sticky(
        &self,
        im_session_key: String,
        live_session_id: String,
        kind: AgentKind,
    ) -> Option<(String, AgentKind)> {
        let prior = {
            let mut g = self.sticky.write().await;
            g.insert(im_session_key.clone(), (live_session_id, kind))
        };
        if let Some((old_sid, old_kind)) = &prior {
            // Same-agent rebind is a noop on the pool — the new sid
            // already takes the active slot, no parking needed.
            // (Callers SHOULD detect this case BEFORE bind_sticky and
            // short-circuit, but we defend here too.)
            if *old_kind == kind {
                tracing::info!(
                    target: "cap",
                    old_session_id = %old_sid,
                    new_session_id = ?prior.as_ref().map(|p| &p.0),
                    agent = kind.as_str(),
                    "cap_live same-agent rebind — ending prior driver"
                );
                let _ = self.end_session(old_sid).await;
            } else {
                // Different agent → park the prior driver for resume.
                let park_key = (im_session_key, *old_kind);
                let evicted = {
                    let mut pool = self.suspended.write().await;
                    pool.insert(park_key, old_sid.clone())
                };
                tracing::info!(
                    target: "cap",
                    old_session_id = %old_sid,
                    old_agent = old_kind.as_str(),
                    new_agent = kind.as_str(),
                    "cap_live sticky rebind — parked prior driver in suspended pool"
                );
                // If the pool already had a parked driver for this
                // (im_key, old_kind), we just orphaned its sid by
                // overwriting the map entry — tear it down now so we
                // don't leak it. (Realistically rare: would require
                // /cap A → /cap B → /cap A → /cap B → /cap A within
                // 10 min.)
                if let Some(evicted_sid) = evicted {
                    if evicted_sid != *old_sid {
                        tracing::info!(
                            target: "cap",
                            evicted_session_id = %evicted_sid,
                            "cap_live pool collision — ending evicted driver"
                        );
                        let _ = self.end_session(&evicted_sid).await;
                    }
                }
            }
        }
        prior
    }

    /// Remove a sticky binding. Returns the previously bound
    /// `(live_session_id, kind)` if there was one.
    pub async fn unbind_sticky(
        &self,
        im_session_key: &str,
    ) -> Option<(String, AgentKind)> {
        let mut g = self.sticky.write().await;
        g.remove(im_session_key)
    }

    /// Look up the live session bound to this IM key. Returns
    /// `None` if the binding doesn't exist OR if the underlying live
    /// session is gone (in which case the stale entry is also evicted).
    pub async fn resolve_sticky(
        &self,
        im_session_key: &str,
    ) -> Option<(String, AgentKind)> {
        let entry = {
            let g = self.sticky.read().await;
            g.get(im_session_key).cloned()
        }?;
        let (sid, _kind) = &entry;
        let alive = {
            let g = self.sessions.read().await;
            g.contains_key(sid)
        };
        if !alive {
            // Self-heal: stale binding (actor died / idle-reaped). Drop it
            // so the next /cap re-bind succeeds and run_turn never tries
            // to dispatch to a ghost.
            let mut g = self.sticky.write().await;
            g.remove(im_session_key);
            return None;
        }
        Some(entry)
    }

    /// Send a prompt to a live cap session. If `session_id` is `None`, a
    /// new session is spawned; if `Some`, the existing session is reused
    /// (and must match `kind` — agents are not interchangeable mid-thread).
    /// The call awaits the driver's full response before returning.
    pub async fn dispatch_sync(
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
                self.spawn_session(&new_id, kind, &cwd, ResumeMode::None)
                    .await?;
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

    /// Best-effort SYNC snapshot of all sticky bindings.
    /// Used by `/status` (which is async-unaware — it runs on
    /// `AgentHandle::format_status`) to render the user's current cap
    /// session bindings without having to refactor the entire status
    /// path through async.
    ///
    /// Returns an empty vec when the sticky lock is held by another
    /// task (rare; we'd rather render an empty section than block
    /// the status reply).
    pub fn snapshot_sticky_blocking(&self) -> Vec<(String, String, AgentKind)> {
        match self.sticky.try_read() {
            Ok(g) => g
                .iter()
                .map(|(im_key, (sid, kind))| (im_key.clone(), sid.clone(), *kind))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Atomically take the "inject memory on next turn" flag for this
    /// live session. Returns `true` exactly once per binding (the first
    /// caller wins), `false` thereafter. The runtime's sticky-bypass
    /// path uses this to decide whether to prepend a memory recall
    /// bundle to the outgoing prompt — only on the first turn, since
    /// the driver process keeps its own history across turns.
    pub async fn try_take_pending_memory_inject(&self, sid: &str) -> bool {
        let g = self.sessions.read().await;
        match g.get(sid) {
            Some(h) => h
                .pending_memory_inject
                .swap(false, std::sync::atomic::Ordering::SeqCst),
            None => false,
        }
    }

    /// Best-effort SYNC lookup of a cap session's agent-native id.
    /// Pair with `snapshot_sticky_blocking` to render `/status`
    /// entries that include the resume id (when the actor has
    /// finished capturing Ready). Returns None for a live session
    /// whose Ready event hasn't landed yet OR if the sessions lock
    /// is contended.
    pub fn agent_session_id_blocking(&self, sid: &str) -> Option<String> {
        let g = self.sessions.try_read().ok()?;
        let h = g.get(sid)?;
        h.agent_session_id.lock().ok().and_then(|s| s.clone())
    }

    /// Same idea as `snapshot_sticky_blocking` but for the suspended
    /// pool. Returns `(im_key, agent_kind, parked_sid)` tuples so
    /// `/status` can render "Suspended cap: feishu:… → codex (abc)".
    /// Caller doesn't need to coordinate with `snapshot_sticky_blocking`
    /// — they read different maps and a tiny inconsistency window
    /// (entry just moved from active → suspended) is harmless for a
    /// human-readable status line.
    pub fn snapshot_suspended_blocking(&self) -> Vec<(String, AgentKind, String)> {
        match self.suspended.try_read() {
            Ok(g) => g
                .iter()
                .map(|((im_key, kind), sid)| (im_key.clone(), *kind, sid.clone()))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Force-close a live session. Idempotent — returns Ok even if the
    /// session was already gone.
    pub async fn end_session(&self, session_id: &str) -> Result<()> {
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
    pub async fn list(&self) -> Vec<(String, AgentKind)> {
        let g = self.sessions.read().await;
        g.iter().map(|(k, h)| (k.clone(), h.agent_kind)).collect()
    }

    async fn spawn_session(
        &self,
        session_id: &str,
        kind: AgentKind,
        cwd: &std::path::Path,
        resume_mode: ResumeMode,
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

        let driver = match &resume_mode {
            ResumeMode::ById(rid) => spawn_driver_resume(kind, cwd, rid).await?,
            ResumeMode::ContinueLast => spawn_driver_continue_last(kind, cwd).await?,
            ResumeMode::None => spawn_driver(kind, cwd).await?,
        };
        let (tx, rx) = mpsc::channel::<LiveRequest>(4);
        let last_active = Arc::new(StdMutex::new(Instant::now()));
        // Async slot for the agent's native session id. Actor_loop's
        // startup drain populates it; `/cap` returns immediately
        // without blocking on Ready.
        let agent_session_id = Arc::new(StdMutex::new(None::<String>));
        let bus = self.bus.clone();
        let sessions_for_gc = Arc::clone(&self.sessions);
        let sticky_for_gc = Arc::clone(&self.sticky);
        let suspended_for_gc = Arc::clone(&self.suspended);
        let sid_owned = session_id.to_owned();
        let agent_sid_slot = Arc::clone(&agent_session_id);
        tokio::spawn(actor_loop(
            sid_owned,
            kind,
            cwd.to_path_buf(),
            driver,
            rx,
            bus,
            sessions_for_gc,
            sticky_for_gc,
            suspended_for_gc,
            agent_sid_slot,
        ));
        // Only inject memory on a FRESH bind. `/cap-resume` and
        // `--continue-last` reattach to a driver session whose process
        // already remembers its prior turns — re-injecting rsclaw's
        // memory bundle there would just burn tokens duplicating context
        // the driver already has.
        let inject = matches!(resume_mode, ResumeMode::None);
        let handle = LiveSessionHandle {
            agent_kind: kind,
            tx,
            last_active,
            agent_session_id,
            pending_memory_inject: Arc::new(std::sync::atomic::AtomicBool::new(inject)),
        };
        let mut g = self.sessions.write().await;
        g.insert(session_id.to_owned(), handle);
        Ok(())
    }

    /// Look up the captured native session_id for a given cap-internal sid.
    /// Returns `None` if the actor hasn't drained the Ready event yet
    /// (claudecode/opencode/codex with heavy startup hooks can take
    /// 10-30s to emit init). Callers SHOULD retry / poll if they need
    /// the id immediately after spawn.
    pub async fn get_agent_session_id(&self, sid: &str) -> Option<String> {
        let g = self.sessions.read().await;
        g.get(sid).and_then(|h| h.agent_session_id.lock().ok().and_then(|s| s.clone()))
    }

    /// Same as `get_agent_session_id` but polls for up to `timeout`
    /// duration so callers that want the id immediately at `/cap` bind
    /// time (and are willing to wait a bit) get a result. Returns
    /// `None` if the timeout elapses.
    #[allow(dead_code)]
    pub async fn wait_agent_session_id(
        &self,
        sid: &str,
        timeout: std::time::Duration,
    ) -> Option<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(s) = self.get_agent_session_id(sid).await {
                return Some(s);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
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
        {
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
        // Sweep sticky bindings that point at reaped sessions so a
        // subsequent inbound IM message doesn't try to dispatch into a
        // ghost (resolve_sticky also self-heals, but doing it here keeps
        // the map size bounded under high churn).
        let mut sg = self.sticky.write().await;
        sg.retain(|_, (sid, _)| !to_remove.contains(sid));
        drop(sg);
        // Sweep suspended-pool entries that point at reaped drivers.
        // Without this the pool would hand out dead sids; acquire_session
        // has a self-heal fast-path but the map would still bloat.
        let mut pg = self.suspended.write().await;
        pg.retain(|_, sid| !to_remove.contains(sid));
    }
}

/// Drain driver events until a `Ready` arrives (or timeout). Returns
/// `Ready.session_id` so callers can surface the agent's NATIVE
/// session id (claudecode UUID, opencode `ses_…`, codex thread_id)
/// to the user — that's what they later type into `/cap-resume`.
///
/// Non-Ready events that arrive before Ready are dropped on the floor.
/// In practice each stream-json driver emits Ready as its FIRST
/// AgentEvent (mapped from the agent's `system/init` frame), so we
/// expect to consume exactly one event here. The loop is defensive in
/// case a future driver emits earlier non-Ready frames.
#[allow(dead_code)]
async fn capture_ready_session_id(
    driver: &mut dyn Driver,
    timeout: std::time::Duration,
) -> Option<String> {
    use cap_rs::core::AgentEvent;
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                tracing::warn!(
                    target: "cap",
                    "no Ready event within {}s; agent_session_id will be unknown",
                    timeout.as_secs()
                );
                return None;
            }
            ev = driver.next_event() => match ev {
                Some(AgentEvent::Ready { session_id, .. }) => {
                    tracing::info!(
                        target: "cap",
                        agent_session_id = ?session_id,
                        "cap_live captured Ready"
                    );
                    return session_id;
                }
                Some(_) => continue,
                None => {
                    tracing::warn!(
                        target: "cap",
                        "driver event stream ended before Ready"
                    );
                    return None;
                }
            }
        }
    }
}

/// Actor loop for one live session: serially process Prompt requests
/// against the held driver, exit on Shutdown or driver failure.
/// On exit, removes its own entry from the sessions map.
/// Replace a dead driver in-place with a freshly spawned one of the same
/// `kind`. Best-effort: shuts down the old driver, spawns a fresh
/// (ResumeMode::None) replacement, and on success swaps it into `*driver`.
/// Returns `true` when the swap happened (caller may retry the prompt),
/// `false` when the respawn failed (caller surfaces the original error).
async fn respawn_driver(
    kind: &AgentKind,
    cwd: &std::path::Path,
    driver: &mut Box<dyn Driver>,
    sid: &str,
    reason: &str,
    agent_sid_slot: &Arc<StdMutex<Option<String>>>,
) -> bool {
    match spawn_driver(*kind, cwd).await {
        Ok(fresh) => {
            if let Err(e) = driver.shutdown().await {
                tracing::debug!(target: "cap", error = %e, "best-effort shutdown of dead driver");
            }
            *driver = fresh;
            // Re-capture the fresh driver's native session id. The new driver
            // emits a Ready (carrying its session id) during its init
            // handshake, BEFORE it consumes the replayed prompt — so drain
            // briefly for it here. The main loop's startup capture runs only
            // once (before the loop) and never sees a respawn's Ready, so
            // without this the slot would stay stale/None for the rest of the
            // actor's life and get_agent_session_id / resume-by-id would break
            // after any respawn. Bounded so a driver that never emits Ready
            // can't stall the retry; on timeout the slot is left None.
            {
                use cap_rs::core::AgentEvent;
                let mut captured: Option<String> = None;
                let recapture_deadline =
                    tokio::time::sleep(std::time::Duration::from_secs(8));
                tokio::pin!(recapture_deadline);
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut recapture_deadline => break,
                        ev = driver.next_event() => match ev {
                            Some(AgentEvent::Ready { session_id, .. }) => {
                                captured = session_id;
                                break;
                            }
                            Some(_) => continue,
                            None => break,
                        }
                    }
                }
                if captured.is_none() {
                    tracing::warn!(
                        target: "cap",
                        session_id = %sid,
                        "no Ready captured after respawn; resume id unavailable until next bind"
                    );
                }
                if let Ok(mut g) = agent_sid_slot.lock() {
                    *g = captured;
                }
            }
            tracing::info!(
                target: "cap",
                session_id = %sid,
                agent = kind.as_str(),
                reason,
                "cap_live respawned driver after death; retrying prompt once"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                target: "cap",
                session_id = %sid,
                agent = kind.as_str(),
                reason,
                error = %e,
                "cap_live driver respawn failed; surfacing error"
            );
            false
        }
    }
}

async fn actor_loop(
    sid: String,
    kind: AgentKind,
    cwd: std::path::PathBuf,
    mut driver: Box<dyn Driver>,
    mut rx: mpsc::Receiver<LiveRequest>,
    bus: broadcast::Sender<rsclaw_events::AgentEvent>,
    sessions: Arc<RwLock<HashMap<String, LiveSessionHandle>>>,
    sticky: Arc<RwLock<HashMap<String, (String, AgentKind)>>>,
    suspended: Arc<RwLock<HashMap<(String, AgentKind), String>>>,
    agent_sid_slot: Arc<StdMutex<Option<String>>>,
) {
    tracing::info!(
        target: "cap",
        session_id = %sid,
        agent = kind.as_str(),
        "cap_live actor started"
    );
    // Race the first events against the first user prompt. EITHER
    // path advances the actor: if Ready arrives first we capture the
    // native session id and proceed to the main loop, if a Prompt
    // arrives first we hold it in `prebuf_request` so the main loop
    // handles it without re-waiting on rx.
    //
    // Why this matters: blocking on Ready (the prior implementation)
    // stalled `/cap codex → 你好` for 30s because the Prompt sat in
    // the mpsc while the actor was draining events looking for Ready.
    // The user-visible cost — 30s of dead silence after the bind
    // reply — was way worse than the capture's value.
    let mut prebuf_request: Option<LiveRequest> = None;
    {
        use cap_rs::core::AgentEvent;
        let capture_deadline = tokio::time::sleep(std::time::Duration::from_secs(30));
        tokio::pin!(capture_deadline);
        loop {
            tokio::select! {
                biased;
                first_req = rx.recv() => {
                    prebuf_request = first_req;
                    break;
                }
                _ = &mut capture_deadline => {
                    tracing::warn!(
                        target: "cap",
                        session_id = %sid,
                        agent = kind.as_str(),
                        "no Ready captured in 30s; resume id unavailable until next bind"
                    );
                    break;
                }
                ev = driver.next_event() => match ev {
                    Some(AgentEvent::Ready { session_id, .. }) => {
                        if let Ok(mut g) = agent_sid_slot.lock() {
                            *g = session_id.clone();
                        }
                        tracing::info!(
                            target: "cap",
                            session_id = %sid,
                            agent_session_id = ?session_id,
                            "cap_live captured Ready"
                        );
                        break;
                    }
                    Some(_) => continue,
                    None => {
                        tracing::warn!(
                            target: "cap",
                            session_id = %sid,
                            "driver event stream ended before Ready"
                        );
                        break;
                    }
                }
            }
        }
    }
    let pseudo_session_id = format!("cap-live-{}-{sid}", kind.as_str());
    // Main loop: process the pre-buffered request first (if any),
    // then drain rx as usual.
    loop {
        let req = match prebuf_request.take() {
            Some(r) => r,
            None => match rx.recv().await {
                Some(r) => r,
                None => break,
            },
        };
        match req {
            LiveRequest::Prompt { task, notif, reply } => {
                // One automatic retry on driver death (send failure or
                // mid-turn exit). The motivating case is the FIRST
                // `opencode acp` launch on a cold machine: the handshake
                // succeeds, then the server exits while handling the
                // first prompt, so `run_turn` returns "exited mid-turn".
                // Respawning a fresh driver and replaying the same prompt
                // hides that one-time flake (and any transient CLI crash)
                // from the user. Fresh respawn = lost in-process context,
                // which is correct here: the dead driver already lost it,
                // and the cold-start case has no prior turns to preserve.
                let mut attempt = 0u8;
                let outcome = loop {
                    let send_res = driver
                        .send(ClientFrame::Prompt {
                            content: vec![Content::text(task.clone())],
                        })
                        .await;
                    if let Err(e) = send_res {
                        if attempt == 0 {
                            if respawn_driver(&kind, &cwd, &mut driver, &sid, "send failed", &agent_sid_slot).await {
                                attempt += 1;
                                continue;
                            }
                        }
                        break Err(anyhow!("cap_live driver send: {e}"));
                    }
                    let mut reply_buf = String::new();
                    let turn = match tokio::time::timeout(
                        super::runtime::TURN_TIMEOUT,
                        run_turn(
                            driver.as_mut(),
                            &bus,
                            &pseudo_session_id,
                            "cap-live",
                            notif.as_ref(),
                            &mut reply_buf,
                        ),
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(_) => Err(anyhow!(
                            "cap_live: turn timed out after {}s (driver hang?)",
                            super::runtime::TURN_TIMEOUT.as_secs()
                        )),
                    };
                    match turn {
                        Ok(()) => break Ok(reply_buf),
                        Err(e) => {
                            if attempt == 0
                                && respawn_driver(
                                    &kind,
                                    &cwd,
                                    &mut driver,
                                    &sid,
                                    "exited mid-turn",
                                    &agent_sid_slot,
                                )
                                .await
                            {
                                attempt += 1;
                                continue;
                            }
                            break Err(anyhow!("cap_live driver: {e}"));
                        }
                    }
                };
                match outcome {
                    Ok(reply_buf) => {
                        let _ = reply.send(Ok(reply_buf));
                    }
                    Err(e) => {
                        // Retry exhausted (or respawn failed) → propagate
                        // error, exit the actor so the manager's GC entry
                        // cleanup below runs; caller opens a new session.
                        let _ = reply.send(Err(e));
                        break;
                    }
                }
            }
            LiveRequest::Shutdown => break,
        }
    }
    if let Err(e) = driver.shutdown().await {
        tracing::debug!(target: "cap", error = %e, "best-effort shutdown of dead driver");
    }
    {
        let mut g = sessions.write().await;
        g.remove(&sid);
    }
    // Drop any sticky bindings that still point at this session — driver
    // is gone (clean shutdown or mid-turn death), the binding is dead.
    {
        let mut sg = sticky.write().await;
        sg.retain(|_, (s, _)| s != &sid);
    }
    // Also drop any suspended-pool entry pointing at this session so a
    // subsequent /cap <same-agent> doesn't try to resume a dead driver
    // (acquire_session has a self-heal, but cleaning the source is
    // tidier).
    {
        let mut pg = suspended.write().await;
        pg.retain(|_, s| s != &sid);
    }
    tracing::info!(
        target: "cap",
        session_id = %sid,
        "cap_live actor exited"
    );
}
