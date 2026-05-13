//! /watch — live event stream → chat slash command.

pub mod dedup;
pub mod parser;
pub mod rate_limit;
pub mod filter;
pub mod source;
pub mod delivery;
mod sse;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{info, warn};

use crate::channel::ChannelManager;
use crate::gateway::watch::dedup::{dedup_key, DedupKey};
use crate::gateway::watch::filter::Filter;
use crate::gateway::watch::parser::{ParsedCommand, SourceKind, StopTarget, WatchSpec};
use crate::gateway::watch::rate_limit::{DeliveryMsg, RateLimiter};
use crate::gateway::watch::source::{
    EventRecord, FileSource, ShellSource, SourceImpl, SseSource, WatchStartError,
};

pub const MAX_WATCHES_PER_PEER: usize = 5;
const PROCESSOR_BUFFER: usize = 256;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(600); // 10 min
const RATE_TICK: Duration = Duration::from_secs(2);

/// Unique watch identifier shown to users (`w_<8 hex>`).
pub type WatchId = String;

#[derive(Debug, Clone)]
pub struct WatchInfo {
    pub id: WatchId,
    pub kind: SourceKind,
    pub raw_source: String,
    pub started_at_ms: u64,
    pub event_count: u64,
    pub error_count: u64,
}

/// Single point of entry for `/watch` slash command.
pub enum WatchCommandReply {
    /// Send this string to chat.
    Reply(String),
    /// Watch was started/stopped silently — caller MUST NOT push to chat.
    Silent,
}

struct WatchTask {
    id: WatchId,
    raw_source: String,
    kind: SourceKind,
    started_at_ms: u64,
    event_count: Arc<std::sync::atomic::AtomicU64>,
    error_count: Arc<std::sync::atomic::AtomicU64>,
    stop_tx: Option<oneshot::Sender<()>>,
}

pub struct WatchRegistry {
    inner: Mutex<HashMap<DedupKey, WatchTask>>,
    channels: Arc<ChannelManager>,
}

static GLOBAL: OnceLock<Arc<WatchRegistry>> = OnceLock::new();

impl WatchRegistry {
    pub fn init(channels: Arc<ChannelManager>) {
        let registry = Arc::new(WatchRegistry {
            inner: Mutex::new(HashMap::new()),
            channels,
        });
        let _ = GLOBAL.set(registry);
    }

    pub fn global() -> Option<Arc<WatchRegistry>> {
        GLOBAL.get().cloned()
    }

    pub async fn handle_command(
        self: Arc<Self>,
        channel: &str,
        peer: &str,
        body: &str,
        origin: Origin,
    ) -> WatchCommandReply {
        match parser::parse(body) {
            Err(e) => WatchCommandReply::Reply(format!("/watch: {e}")),
            Ok(ParsedCommand::List) => WatchCommandReply::Reply(self.format_list(channel, peer).await),
            Ok(ParsedCommand::Stop(StopTarget::All)) => {
                let n = self.stop_all_for(channel, peer).await;
                WatchCommandReply::Reply(format!("Stopped {n} watch(es)."))
            }
            Ok(ParsedCommand::Stop(StopTarget::One(id))) => {
                let stopped = self.stop_one(channel, peer, &id).await;
                WatchCommandReply::Reply(if stopped {
                    format!("Stopped {id}.")
                } else {
                    format!("No active watch `{id}` for this channel/peer.")
                })
            }
            Ok(ParsedCommand::Start(spec)) => self.handle_start(channel, peer, spec, origin).await,
        }
    }

    async fn handle_start(
        self: Arc<Self>,
        channel: &str,
        peer: &str,
        spec: WatchSpec,
        origin: Origin,
    ) -> WatchCommandReply {
        let key = dedup_key(channel, peer, &spec.raw_source);

        // Single critical section: dedup check + cap + reserve slot + insert.
        // Holding the lock across the whole sequence prevents two concurrent
        // /watch <same-source> calls from both missing dedup and double-spawning.
        let mut inner = self.inner.lock().await;

        // Dedup.
        if let Some(existing) = inner.get(&key) {
            let id = existing.id.clone();
            let started = existing.started_at_ms;
            let count = existing.event_count.load(std::sync::atomic::Ordering::Relaxed);
            drop(inner);
            if origin == Origin::Cron {
                return WatchCommandReply::Silent;
            }
            let secs = now_ms().saturating_sub(started) / 1000;
            return WatchCommandReply::Reply(format!(
                "Watch {id} already running ({secs}s, {count} events). Stop with: /watch stop {id}"
            ));
        }

        // Concurrency cap.
        let count_for_peer = inner
            .keys()
            .filter(|(ch, pe, _)| ch == channel && pe == peer)
            .count();
        if count_for_peer >= MAX_WATCHES_PER_PEER {
            drop(inner);
            return WatchCommandReply::Reply(format!(
                "/watch failed: limit reached ({count_for_peer}/{MAX_WATCHES_PER_PEER}). Stop one with /watch stop <id>"
            ));
        }

        // Build source impl + filter (both sync — path existence check,
        // ${VAR} substitution, regex compile). Errors map back to user.
        let source_impl = match build_source_impl(&spec) {
            Ok(s) => s,
            Err(e) => {
                drop(inner);
                return WatchCommandReply::Reply(format!("/watch failed: {e}"));
            }
        };
        let filter = match Filter::from_spec(spec.grep.as_deref(), spec.jq.as_deref()) {
            Ok(f) => f,
            Err(e) => {
                drop(inner);
                return WatchCommandReply::Reply(format!("/watch failed: invalid filter: {e}"));
            }
        };

        // Reserve the slot BEFORE spawning tasks. A concurrent /watch on the
        // same source will now dedup-hit this entry.
        let id = generate_id();
        let event_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let error_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (stop_tx, stop_rx) = oneshot::channel();
        let (src_tx, src_rx) = mpsc::channel::<EventRecord>(PROCESSOR_BUFFER);
        let src_kind = source_impl.kind();

        inner.insert(
            key,
            WatchTask {
                id: id.clone(),
                raw_source: spec.raw_source.clone(),
                kind: src_kind,
                started_at_ms: now_ms(),
                event_count: event_count.clone(),
                error_count: error_count.clone(),
                stop_tx: Some(stop_tx),
            },
        );
        drop(inner); // Release the lock BEFORE we spawn long-running tasks.

        // Spawn the source + processor tasks.
        tokio::spawn(async move { source_impl.run(src_tx, stop_rx).await });

        let registry = self.clone();
        let channel_s = channel.to_owned();
        let peer_s = peer.to_owned();
        let id_clone = id.clone();
        let rate_ms = spec.rate_ms;
        tokio::spawn(async move {
            registry
                .processor_loop(
                    channel_s,
                    peer_s,
                    id_clone,
                    filter,
                    rate_ms,
                    src_rx,
                    event_count,
                    error_count,
                )
                .await;
        });

        info!(channel = %channel, peer = %peer, id = %id, "watch started");
        if origin == Origin::Cron {
            WatchCommandReply::Reply(format!("Watch (re)started: {id}"))
        } else {
            WatchCommandReply::Reply(format!("Watch started: {id}"))
        }
    }

    /// Signal every active watch to stop. Idempotent. Called by gateway
    /// shutdown so SSE / subprocess tasks get a clean exit instead of
    /// dangling until process termination.
    pub async fn shutdown_all(&self) {
        let mut inner = self.inner.lock().await;
        let count = inner.len();
        for (_, mut task) in inner.drain() {
            if let Some(tx) = task.stop_tx.take() {
                let _ = tx.send(());
            }
        }
        if count > 0 {
            info!(stopped = count, "watch registry shutdown");
        }
    }

    async fn processor_loop(
        self: Arc<Self>,
        channel: String,
        peer: String,
        id: WatchId,
        filter: Filter,
        rate_ms: u64,
        mut src_rx: mpsc::Receiver<EventRecord>,
        event_count: Arc<std::sync::atomic::AtomicU64>,
        _error_count: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut limiter = RateLimiter::new(rate_ms);
        let mut rate_tick = tokio::time::interval(RATE_TICK);
        rate_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut heartbeat_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_count_at_tick = 0u64;

        loop {
            tokio::select! {
                maybe_ev = src_rx.recv() => match maybe_ev {
                    Some(ev) => {
                        event_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Lifecycle events: pass through to chat as a plain message (no rate limit).
                        if ev.event.starts_with('_') {
                            let _ = delivery::deliver(
                                &self.channels,
                                &channel,
                                &peer,
                                format!("watch {id}: {} {}", ev.event, ev.data),
                            ).await;
                            if matches!(ev.event.as_str(), "_error" | "_disconnect") && ev.data.get("fatal").and_then(|v| v.as_bool()).unwrap_or(false) {
                                break;
                            }
                            continue;
                        }
                        if let Some(display) = filter.apply(&ev) {
                            if let Some(out) = limiter.admit(display, now_ms()) {
                                self.send_delivery(&channel, &peer, out).await;
                            }
                        }
                    }
                    None => break,
                },
                _ = rate_tick.tick() => {
                    if let Some(out) = limiter.flush_pending(now_ms()) {
                        self.send_delivery(&channel, &peer, out).await;
                    }
                }
                _ = heartbeat_tick.tick() => {
                    let now = event_count.load(std::sync::atomic::Ordering::Relaxed);
                    if now == last_count_at_tick {
                        let _ = delivery::deliver(
                            &self.channels,
                            &channel,
                            &peer,
                            format!("watch {id} active, 0 events in last 10m"),
                        ).await;
                    }
                    last_count_at_tick = now;
                }
            }
        }

        // Cleanup: drop the task from the registry HashMap so a future /watch
        // for the same source can spawn fresh.
        let mut inner = self.inner.lock().await;
        inner.retain(|_, t| t.id != id);
        info!(channel = %channel, peer = %peer, id = %id, "watch processor exited");
    }

    async fn send_delivery(&self, channel: &str, peer: &str, msg: DeliveryMsg) {
        let body = match msg {
            DeliveryMsg::Single(s) => s,
            DeliveryMsg::Batch { last, dropped } => {
                format!("{dropped} more events in 2s, last: {last}")
            }
        };
        if let Err(e) = delivery::deliver(&self.channels, channel, peer, body).await {
            warn!(channel = %channel, peer = %peer, "watch delivery failed: {e}");
        }
    }

    pub async fn stop_one(&self, channel: &str, peer: &str, id: &str) -> bool {
        let mut inner = self.inner.lock().await;
        let key_to_remove = inner
            .iter()
            .find(|((ch, pe, _), t)| ch == channel && pe == peer && t.id == id)
            .map(|(k, _)| k.clone());
        if let Some(k) = key_to_remove {
            if let Some(mut task) = inner.remove(&k) {
                if let Some(tx) = task.stop_tx.take() {
                    let _ = tx.send(());
                }
                return true;
            }
        }
        false
    }

    pub async fn stop_all_for(&self, channel: &str, peer: &str) -> usize {
        let mut inner = self.inner.lock().await;
        let keys: Vec<DedupKey> = inner
            .keys()
            .filter(|(ch, pe, _)| ch == channel && pe == peer)
            .cloned()
            .collect();
        let n = keys.len();
        for k in keys {
            if let Some(mut task) = inner.remove(&k) {
                if let Some(tx) = task.stop_tx.take() {
                    let _ = tx.send(());
                }
            }
        }
        n
    }

    async fn format_list(&self, channel: &str, peer: &str) -> String {
        let inner = self.inner.lock().await;
        let watches: Vec<WatchInfo> = inner
            .iter()
            .filter(|((ch, pe, _), _)| ch == channel && pe == peer)
            .map(|(_, t)| WatchInfo {
                id: t.id.clone(),
                kind: t.kind,
                raw_source: t.raw_source.clone(),
                started_at_ms: t.started_at_ms,
                event_count: t.event_count.load(std::sync::atomic::Ordering::Relaxed),
                error_count: t.error_count.load(std::sync::atomic::Ordering::Relaxed),
            })
            .collect();

        if watches.is_empty() {
            return "No active watches.".into();
        }
        let mut lines = vec![format!(
            "Active watches ({}/{}):",
            watches.len(),
            MAX_WATCHES_PER_PEER
        )];
        for w in &watches {
            let elapsed = now_ms().saturating_sub(w.started_at_ms) / 1000;
            let kind_str = match w.kind {
                SourceKind::File => "file",
                SourceKind::Sse => "sse",
                SourceKind::Shell => "shell",
            };
            lines.push(format!(
                "  {}  {}:{}  {}s  {} events",
                w.id, kind_str, truncate(&w.raw_source, 50), elapsed, w.event_count
            ));
        }
        lines.push(String::new());
        lines.push("Stop with: /watch stop <id>  or  /watch stop all".into());
        lines.join("\n")
    }
}

fn build_source_impl(spec: &WatchSpec) -> Result<SourceImpl, WatchStartError> {
    match spec.kind {
        SourceKind::File => {
            let path = std::path::PathBuf::from(&spec.raw_source);
            if !path.exists() {
                return Err(WatchStartError::InvalidPath(spec.raw_source.clone()));
            }
            Ok(SourceImpl::File(FileSource { path }))
        }
        SourceKind::Shell => Ok(SourceImpl::Shell(ShellSource {
            cmd: spec.raw_source.clone(),
        })),
        SourceKind::Sse => {
            let sse = SseSource::build(&spec.raw_source, &spec.headers)?;
            Ok(SourceImpl::Sse(sse))
        }
    }
}

fn generate_id() -> WatchId {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("w_{}", &id[..8])
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max - 3).collect();
        out.push_str("...");
        out
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Where the /watch text came from. Cron-origin dedup-hit replies are silent
/// to avoid spamming chat from `/loop 10m /watch ...` compositions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    User,
    Cron,
}
