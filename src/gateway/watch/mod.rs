//! /watch — live event stream → chat slash command.

pub mod dedup;
pub mod parser;
pub mod rate_limit;
pub mod filter;
pub mod jq;
pub mod source;
pub mod delivery;
mod sse;
pub mod template;

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

/// Per (channel, peer) concurrent watch cap. Spec §"并发上限". Prevents a
/// user from spawning enough watches to overwhelm the chat or the gateway.
pub const MAX_WATCHES_PER_PEER: usize = 5;

/// Per-watch source→processor mpsc buffer. Bigger than a typical SSE event
/// rate so bursts don't block the source reader; if it fills, the source
/// `try_send` drops events and bumps a counter (visible in /watch list).
const PROCESSOR_BUFFER: usize = 256;

/// How often the processor checks "no events seen in this window — still alive?"
/// Spec §"心跳": 10 min silent ⇒ emit a heartbeat note.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(600);

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
    /// Account label this watch's deliveries are pinned to (Feishu /
    /// WeCom multi-tenant routing). Surfaced in `/watch list` so an
    /// operator can attribute a long-running watch to the app it's
    /// posting under. R4 review I1.
    pub account: Option<String>,
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
    /// Originating account for multi-account channels (e.g. feishu with
    /// two `appId`s). Stored so subsequent event deliveries route via the
    /// same app that received the `/watch` command. `None` means "use the
    /// bare channel name", which is the right default for single-account
    /// channels and the safe fallback when the caller can't supply it.
    account: Option<String>,
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

    /// Build a fresh, non-global registry for integration tests. Real callers
    /// always use `init()` + `global()` — but tests can't share the OnceLock
    /// (any test that lands first would freeze the state for the rest).
    #[doc(hidden)]
    pub fn init_for_test() -> Arc<Self> {
        let channels = Arc::new(crate::channel::ChannelManager::new(
            crate::sys::MemoryTier::Standard,
        ));
        Arc::new(WatchRegistry {
            inner: Mutex::new(HashMap::new()),
            channels,
        })
    }

    pub async fn handle_command(
        self: Arc<Self>,
        channel: &str,
        peer: &str,
        account: Option<String>,
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
            Ok(ParsedCommand::Start(spec)) => self.handle_start(channel, peer, account, spec, origin).await,
        }
    }

    async fn handle_start(
        self: Arc<Self>,
        channel: &str,
        peer: &str,
        account: Option<String>,
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
        // Resolve `--template <name>` defaults into any flag slots the
        // user left empty. User flags always win — the template only
        // fills holes — so `--template astock --grep ERR` keeps the
        // template's jq/event-filter while overriding nothing.
        let (effective_grep, effective_jq, effective_event_filter) =
            resolve_template_defaults(&spec);
        let filter = match Filter::from_spec(
            effective_grep.as_deref(),
            effective_jq.as_deref(),
            effective_event_filter,
        ) {
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
                account: account.clone(),
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
        let account_for_loop = account.clone();
        tokio::spawn(async move {
            registry
                .processor_loop(
                    channel_s,
                    peer_s,
                    account_for_loop,
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
        account: Option<String>,
        id: WatchId,
        filter: Filter,
        rate_ms: u64,
        mut src_rx: mpsc::Receiver<EventRecord>,
        event_count: Arc<std::sync::atomic::AtomicU64>,
        _error_count: Arc<std::sync::atomic::AtomicU64>,
    ) {
        let mut limiter = RateLimiter::new(rate_ms);
        // Flush-pending tick rides on the user's --rate window. With rate=0
        // (unlimited) every admit() short-circuits to Single, so the tick has
        // nothing to do — back off to once per minute to keep the task quiet.
        let tick_interval = if rate_ms == 0 {
            Duration::from_secs(60)
        } else {
            Duration::from_millis(rate_ms.max(100))
        };
        let mut rate_tick = tokio::time::interval(tick_interval);
        rate_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skew the first heartbeat tick out by one full interval. The
        // default `tokio::time::interval(...)` fires immediately on the
        // first tick, which would send a spurious "watch w_xxx active,
        // 0 events in last 10m" to chat right after /watch start —
        // confusing and noisy. interval_at(now + interval, interval)
        // shifts the first fire to the natural cadence.
        let mut heartbeat_tick = tokio::time::interval_at(
            tokio::time::Instant::now() + HEARTBEAT_INTERVAL,
            HEARTBEAT_INTERVAL,
        );
        heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_count_at_tick = 0u64;
        // Deduplicate consecutive identical lifecycle events so a SSE
        // source that's stuck in connect-refused reconnect-loop (e.g.
        // upstream's intraday_heal cron just bounced it) doesn't spam
        // chat with `_disconnect ... _disconnect ... _disconnect ...`
        // every backoff cycle (2s→4s→8s→...→30s). The first occurrence
        // is informative; identical repeats are noise. Reset on the
        // next non-lifecycle event (proves connection is healthy again).
        // Lifecycle signature = `event_type|reason`; carrying `reason`
        // lets a DIFFERENT failure mode (e.g. heartbeat_timeout after
        // a streak of connect errors) still surface as fresh.
        let mut last_lifecycle_sig: Option<String> = None;

        loop {
            tokio::select! {
                maybe_ev = src_rx.recv() => match maybe_ev {
                    Some(ev) => {
                        event_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Lifecycle events: pass through to chat as a plain message (no rate limit).
                        if ev.event.starts_with('_') {
                            let reason = ev
                                .data
                                .get("reason")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let sig = format!("{}|{}", ev.event, reason);
                            let is_new = last_lifecycle_sig.as_deref() != Some(&sig);
                            if is_new {
                                last_lifecycle_sig = Some(sig);
                                let _ = delivery::deliver(
                                    &self.channels,
                                    &channel,
                                    account.as_deref(),
                                    &peer,
                                    format!("watch {id}: {} {}", ev.event, ev.data),
                                ).await;
                            }
                            if matches!(ev.event.as_str(), "_error" | "_disconnect") && ev.data.get("fatal").and_then(|v| v.as_bool()).unwrap_or(false) {
                                break;
                            }
                            continue;
                        }
                        // Real data event arrived — connection is healthy
                        // again. Clear the lifecycle dedup so any future
                        // disconnect surfaces as fresh (we want users to
                        // see "stream just dropped" if it happens after
                        // working normally for a while).
                        last_lifecycle_sig = None;
                        // jq with array expansion (e.g. `.codes[]`)
                        // produces multiple lines from one event; each
                        // goes through the rate limiter independently.
                        for display in filter.apply(&ev) {
                            if let Some(out) = limiter.admit(display, now_ms()) {
                                self.send_delivery(&channel, account.as_deref(), &peer, out).await;
                            }
                        }
                    }
                    None => break,
                },
                _ = rate_tick.tick() => {
                    if let Some(out) = limiter.flush_pending(now_ms()) {
                        self.send_delivery(&channel, account.as_deref(), &peer, out).await;
                    }
                }
                _ = heartbeat_tick.tick() => {
                    let now = event_count.load(std::sync::atomic::Ordering::Relaxed);
                    if now == last_count_at_tick {
                        let _ = delivery::deliver(
                            &self.channels,
                            &channel,
                            account.as_deref(),
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

    async fn send_delivery(&self, channel: &str, account: Option<&str>, peer: &str, msg: DeliveryMsg) {
        let body = match msg {
            DeliveryMsg::Single(s) => s,
            DeliveryMsg::Batch { last, dropped } => {
                format!("{dropped} more events in 2s, last: {last}")
            }
        };
        if let Err(e) = delivery::deliver(&self.channels, channel, account, peer, body).await {
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
                account: t.account.clone(),
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
            // Annotate with `@<account>` when present so multi-tenant
            // Feishu / WeCom operators can tell which app a watch
            // posts under without grepping logs (R4 review I1).
            let account_tag = w
                .account
                .as_deref()
                .map(|a| format!("@{a} "))
                .unwrap_or_default();
            lines.push(format!(
                "  {}  {}{}:{}  {}s  {} events",
                w.id,
                account_tag,
                kind_str,
                truncate(&w.raw_source, 50),
                elapsed,
                w.event_count
            ));
        }
        lines.push(String::new());
        lines.push("Stop with: /watch stop <id>  or  /watch stop all".into());
        lines.join("\n")
    }
}

/// Public alias used by the `rsclaw watch` CLI (`src/cmd/watch.rs`) so
/// it can share the same template-resolution logic as the chat-side
/// `/watch` slash command.
pub fn resolve_template_defaults_for_cli(
    spec: &WatchSpec,
) -> (Option<String>, Option<String>, Option<parser::EventFilter>) {
    resolve_template_defaults(spec)
}

/// Resolve `--template <name>` against a spec, returning the effective
/// `(grep, jq, event_filter)` triple that should drive the filter
/// pipeline. User-supplied flags always win — the template only fills
/// slots the user left empty. The parser already validated the
/// template name, so an unknown name here is a programming error.
fn resolve_template_defaults(
    spec: &WatchSpec,
) -> (Option<String>, Option<String>, Option<parser::EventFilter>) {
    let template = spec.template.as_deref().and_then(|name| template::lookup(name).ok());
    let jq = spec.jq.clone().or_else(|| {
        template
            .and_then(|t| t.jq)
            .map(|s| s.to_owned())
    });
    let event_filter = spec.event_filter.clone().or_else(|| {
        template
            .and_then(|t| t.event_filter)
            .and_then(|raw| parser::EventFilter::parse(raw).ok())
    });
    (spec.grep.clone(), jq, event_filter)
}

pub(crate) fn build_source_impl(spec: &WatchSpec) -> Result<SourceImpl, WatchStartError> {
    // Resolve `${VAR}` references in raw_source up-front so every source kind
    // gets consistent env-var support. Without this, file paths would be
    // taken literally and shell commands would depend on the platform's
    // shell to expand vars (works in `sh -c` on Unix, breaks on Windows
    // `powershell -Command` which uses `$env:VAR` syntax instead).
    // SSE headers are still substituted inside SseSource::build because
    // they live outside raw_source.
    let resolved_source = crate::gateway::watch::sse::substitute_env_vars(&spec.raw_source)
        .map_err(|e| WatchStartError::UnresolvedEnv(e.to_string()))?;

    match spec.kind {
        SourceKind::File => {
            // Also expand a leading `~` / `~/` / `~\` so users can write paths
            // the way they'd type them in a shell.
            let path = crate::config::loader::expand_tilde_path_pub(&resolved_source);
            if !path.exists() {
                return Err(WatchStartError::InvalidPath(resolved_source));
            }
            Ok(SourceImpl::File(FileSource { path }))
        }
        SourceKind::Shell => Ok(SourceImpl::Shell(ShellSource {
            cmd: resolved_source,
        })),
        SourceKind::Sse => {
            // SseSource::build runs substitute_env_vars on the URL again, which
            // is a no-op (the regex finds no remaining `${...}` after our pass)
            // but keeps the SSE-headers substitution logic local to that path.
            let sse = SseSource::build(&resolved_source, &spec.headers)?;
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

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::gateway::watch::parser::SourceKind;

    fn spec_file(raw: &str) -> WatchSpec {
        WatchSpec {
            kind: SourceKind::File,
            raw_source: raw.to_owned(),
            headers: vec![],
            grep: None,
            jq: None,
            rate_ms: 0,
            event_filter: None,
            template: None,
        }
    }

    #[test]
    fn file_source_expands_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("app.log");
        std::fs::write(&log_path, b"line\n").unwrap();

        // Unique env var name so other tests don't race.
        unsafe { std::env::set_var("WATCH_BUILD_TEST_DIR", dir.path().to_str().unwrap()) };
        let impl_ = build_source_impl(&spec_file("${WATCH_BUILD_TEST_DIR}/app.log"))
            .expect("env var should resolve and path should exist");
        unsafe { std::env::remove_var("WATCH_BUILD_TEST_DIR") };

        match impl_ {
            SourceImpl::File(f) => assert_eq!(f.path, log_path),
            _ => panic!("expected File source"),
        }
    }

    #[test]
    fn file_source_unset_env_var_errors() {
        unsafe { std::env::remove_var("WATCH_BUILD_TEST_MISSING") };
        match build_source_impl(&spec_file("${WATCH_BUILD_TEST_MISSING}/x.log")) {
            Ok(_) => panic!("missing env var should error"),
            Err(WatchStartError::UnresolvedEnv(name)) => {
                assert!(name.contains("WATCH_BUILD_TEST_MISSING"), "got: {name}");
            }
            Err(other) => panic!("expected UnresolvedEnv, got {other:?}"),
        }
    }
}
