//! Process-wide channel connection-state registry.
//!
//! Each running channel account owns a [`StatusHandle`] keyed by
//! `(channel, account)`. The gateway task supervisor wraps the channel's run
//! loop in [`track`] (marks `running` on start, `disconnected` / `error` on
//! exit), and channels with a real link-level signal (feishu WebSocket, wechat
//! long-poll) refine it to `connecting` / `connected` / `error` from inside
//! their loops. `/api/v1/status` reads [`snapshot`].
//!
//! Every handle carries a generation number: a hot-reload replacement mints a
//! newer generation, so late writes from the torn-down predecessor (its drop
//! guard firing after the replacement started) are ignored.

use std::{
    collections::BTreeMap,
    sync::{
        LazyLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use serde::Serialize;

/// Link state of one channel account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelState {
    /// Establishing (or re-establishing) the upstream connection.
    Connecting,
    /// Upstream connection confirmed live.
    Connected,
    /// Run loop alive, but the channel does not report link-level state.
    Running,
    /// Run loop stopped (shutdown, cancel, or clean exit).
    Disconnected,
    /// Run loop exited with an error, or repeated connection failures.
    Error,
}

impl ChannelState {
    /// Wire name (`"connected"`, `"error"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Running => "running",
            Self::Disconnected => "disconnected",
            Self::Error => "error",
        }
    }
}

/// One registry row, as exposed by [`snapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct ChannelStatus {
    /// Channel type (`"feishu"`, `"telegram"`, ...).
    pub channel: String,
    /// Account name within the channel (`"default"` for single-account).
    pub account: String,
    pub state: ChannelState,
    /// Unix milliseconds of the last state change.
    pub since_ms: i64,
    /// Most recent error message, kept across later state changes.
    pub last_error: Option<String>,
}

struct Entry {
    generation: u64,
    state: ChannelState,
    since_ms: i64,
    last_error: Option<String>,
}

static REGISTRY: LazyLock<RwLock<BTreeMap<(String, String), Entry>>> =
    LazyLock::new(|| RwLock::new(BTreeMap::new()));
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Max stored error length (bytes); longer messages are truncated CJK-safely.
const MAX_ERROR_BYTES: usize = 500;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Writer for one `(channel, account)` registry row.
#[derive(Debug, Clone)]
pub struct StatusHandle {
    channel: String,
    account: String,
    generation: u64,
}

impl StatusHandle {
    /// Register a new generation for `(channel, account)` in `connecting`
    /// state, superseding any previous handle for the same key.
    pub fn new(channel: &str, account: &str) -> Self {
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let handle = Self {
            channel: channel.to_owned(),
            account: if account.is_empty() {
                "default".to_owned()
            } else {
                account.to_owned()
            },
            generation,
        };
        match REGISTRY.write() {
            Ok(mut map) => {
                let key = (handle.channel.clone(), handle.account.clone());
                let last_error = map.get(&key).and_then(|e| e.last_error.clone());
                map.insert(
                    key,
                    Entry {
                        generation,
                        state: ChannelState::Connecting,
                        since_ms: now_ms(),
                        last_error,
                    },
                );
            }
            Err(_) => tracing::warn!("channel status registry lock poisoned; status not recorded"),
        }
        handle
    }

    fn update(&self, f: impl FnOnce(&mut Entry)) {
        match REGISTRY.write() {
            Ok(mut map) => {
                if let Some(entry) = map.get_mut(&(self.channel.clone(), self.account.clone()))
                    && entry.generation == self.generation
                {
                    f(entry);
                }
            }
            Err(_) => tracing::warn!("channel status registry lock poisoned; status not recorded"),
        }
    }

    /// Move to `state`. `since` only advances when the state actually changes.
    pub fn set(&self, state: ChannelState) {
        self.update(|e| {
            if e.state != state {
                e.state = state;
                e.since_ms = now_ms();
            }
        });
    }

    /// Move to `state` and record `err` as the latest error.
    pub fn set_with_error(&self, state: ChannelState, err: &str) {
        let msg = rsclaw_util::truncate_str(err, MAX_ERROR_BYTES).to_owned();
        self.update(|e| {
            if e.state != state {
                e.state = state;
                e.since_ms = now_ms();
            }
            e.last_error = Some(msg);
        });
    }

    /// Record `err` as the latest error without changing the state (for
    /// routine, self-healing failures such as a single long-poll timeout).
    pub fn note_error(&self, err: &str) {
        let msg = rsclaw_util::truncate_str(err, MAX_ERROR_BYTES).to_owned();
        self.update(|e| e.last_error = Some(msg));
    }
}

/// Marks the row `disconnected` if the tracked run future is dropped before
/// completing (shutdown / cancel arm of the supervisor's `select!`).
struct StopGuard {
    handle: StatusHandle,
    armed: bool,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if self.armed {
            self.handle.set(ChannelState::Disconnected);
        }
    }
}

/// Run a channel's main loop while recording its lifecycle: `running` on
/// start (channels with real signals refine it), `disconnected` on clean exit
/// or drop, `error` (with message) when the loop returns an error.
pub async fn track<F>(handle: StatusHandle, fut: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    handle.set(ChannelState::Running);
    let mut guard = StopGuard {
        handle: handle.clone(),
        armed: true,
    };
    let res = fut.await;
    guard.armed = false;
    match &res {
        Ok(()) => handle.set(ChannelState::Disconnected),
        Err(e) => handle.set_with_error(ChannelState::Error, &format!("{e:#}")),
    }
    res
}

/// All registry rows, ordered by `(channel, account)`.
pub fn snapshot() -> Vec<ChannelStatus> {
    match REGISTRY.read() {
        Ok(map) => map
            .iter()
            .map(|((channel, account), e)| ChannelStatus {
                channel: channel.clone(),
                account: account.clone(),
                state: e.state,
                since_ms: e.since_ms,
                last_error: e.last_error.clone(),
            })
            .collect(),
        Err(_) => {
            tracing::warn!("channel status registry lock poisoned; reporting no channels");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(channel: &str, account: &str) -> Option<ChannelStatus> {
        snapshot()
            .into_iter()
            .find(|s| s.channel == channel && s.account == account)
    }

    #[tokio::test]
    async fn track_records_lifecycle_and_errors() {
        let h = StatusHandle::new("status_test_ch", "a1");
        assert_eq!(row("status_test_ch", "a1").unwrap().state, ChannelState::Connecting);
        let res = track(h.clone(), async {
            h.set(ChannelState::Connected);
            assert_eq!(row("status_test_ch", "a1").unwrap().state, ChannelState::Connected);
            Err(anyhow::anyhow!("boom"))
        })
        .await;
        assert!(res.is_err());
        let r = row("status_test_ch", "a1").unwrap();
        assert_eq!(r.state, ChannelState::Error);
        assert_eq!(r.last_error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn stale_generation_writes_are_ignored() {
        let old = StatusHandle::new("status_test_gen", "default");
        let new = StatusHandle::new("status_test_gen", "");
        new.set(ChannelState::Connected);
        // The predecessor's late teardown must not clobber the replacement.
        old.set(ChannelState::Disconnected);
        assert_eq!(
            row("status_test_gen", "default").unwrap().state,
            ChannelState::Connected
        );
    }

    #[tokio::test]
    async fn dropped_run_future_marks_disconnected() {
        let h = StatusHandle::new("status_test_drop", "default");
        tokio::select! {
            _ = track(h.clone(), std::future::pending::<Result<()>>()) => {}
            () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
        }
        assert_eq!(
            row("status_test_drop", "default").unwrap().state,
            ChannelState::Disconnected
        );
    }
}
