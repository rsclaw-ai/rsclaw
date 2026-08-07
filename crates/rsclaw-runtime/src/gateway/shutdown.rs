//! Gateway-wide graceful shutdown coordinator.
//!
//! On graceful restart, multiple async tasks need to:
//!   1. Stop accepting new work (set `draining = true`).
//!   2. Wait for currently-running work to complete (`inflight == 0`).
//!   3. Exit cleanly so the parent process can spawn the replacement.
//!
//! Subscribers:
//!   - `axum::serve(...).with_graceful_shutdown(coord.notified())` — drains
//!     HTTP.
//!   - `TaskQueueWorker::run()` — checks `draining` at top of loop.
//!   - Channel handlers — same pattern, when refactored to honor it.
//!
//! Publishers:
//!   - `POST /api/v1/restart` handler triggers `begin_drain()`.
//!   - SIGTERM / Ctrl+C handlers can do the same.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use tokio::sync::watch;

/// Coordinates graceful shutdown across the HTTP server, task queue worker,
/// and channel handlers. Cheap to clone (single `Arc`).
#[derive(Clone)]
pub struct ShutdownCoordinator {
    inner: Arc<ShutdownInner>,
}

impl Default for ShutdownCoordinator {
    fn default() -> Self {
        let (tx, _) = watch::channel(false);
        Self {
            inner: Arc::new(ShutdownInner {
                draining: AtomicBool::new(false),
                notify_tx: tx,
                inflight: AtomicUsize::new(0),
                restart_requested: AtomicBool::new(false),
            }),
        }
    }
}

struct ShutdownInner {
    /// Set to true when graceful shutdown begins. Workers check this before
    /// pulling new work; HTTP server stops accepting new connections.
    draining: AtomicBool,
    /// Wakes up `axum::serve(...).with_graceful_shutdown(future)` and any
    /// other awaiter that wants to be notified the moment drain begins.
    /// Uses a watch channel instead of Notify so every subscriber (current
    /// and future) sees the drain signal — a watch send is durable.
    notify_tx: watch::Sender<bool>,
    /// Number of in-flight units of work (HTTP requests, agent turns,
    /// task queue entries) currently being processed. Restart waits for
    /// this to drop to zero (with a timeout) before terminating the process.
    inflight: AtomicUsize,
    /// Set by `request_restart()`. After `axum::serve()` returns (i.e., the
    /// listener has been released), `start_gateway` reads this flag to decide
    /// whether to spawn a replacement gateway process. Decoupling the spawn
    /// from the restart handler avoids the race where the child tries to
    /// `bind()` while the parent's listener is still held by axum.
    restart_requested: AtomicBool,
}

impl ShutdownCoordinator {
    /// Construct a new coordinator. The fresh state is `draining = false`,
    /// `inflight = 0`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `begin_drain` has been called.
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }

    /// Mark the gateway as draining and wake every subscriber to `notified`.
    /// Idempotent — calling twice is safe.
    pub fn begin_drain(&self) {
        self.inner.draining.store(true, Ordering::Release);
        // watch: send false→true; durable, so every subscriber (past + future)
        // sees it immediately. Ignore error: only happens when all receivers
        // are dropped, which means nobody is waiting — fine.
        let _ = self.inner.notify_tx.send(true);
    }

    /// Wait for `begin_drain` to be called. If drain has already begun,
    /// returns immediately. Unlike the old Notify-based implementation, a
    /// watch channel is durable — late subscribers see the current value
    /// instantly, so there is no multi-waiter race.
    ///
    /// Intended for `axum::serve(...).with_graceful_shutdown(future)`.
    pub async fn notified(&self) {
        if self.is_draining() {
            return;
        }
        let mut rx = self.inner.notify_tx.subscribe();
        if self.is_draining() {
            return;
        }
        // Wait for the value to become true. Changed() returns Ok when tx
        // sends, Err when tx is dropped. Either way: if drain has begun,
        // *rx.borrow() is true and we return.
        let _ = rx.wait_for(|v| *v).await;
    }

    /// Increment the in-flight counter. Pair with `complete()` in a guard.
    pub fn begin_work(&self) -> InflightGuard {
        self.inner.inflight.fetch_add(1, Ordering::AcqRel);
        InflightGuard {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Current number of in-flight units of work.
    pub fn inflight(&self) -> usize {
        self.inner.inflight.load(Ordering::Acquire)
    }

    /// Mark this drain as a restart, then begin draining. After
    /// `axum::serve()` returns, `start_gateway` will spawn a replacement
    /// gateway process instead of exiting cleanly.
    ///
    /// Idempotent — safe to call concurrently with `begin_drain` or itself.
    pub fn request_restart(&self) {
        self.inner.restart_requested.store(true, Ordering::Release);
        self.begin_drain();
    }

    /// Whether `request_restart` has been called this session.
    pub fn is_restart_requested(&self) -> bool {
        self.inner.restart_requested.load(Ordering::Acquire)
    }
}

/// RAII guard returned from `begin_work`. Decrements the in-flight counter on
/// drop, even if the work future is cancelled or panics.
pub struct InflightGuard {
    inner: Arc<ShutdownInner>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.inner.inflight.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn begin_drain_wakes_notified() {
        let coord = ShutdownCoordinator::new();
        let coord_clone = coord.clone();
        let waiter = tokio::spawn(async move { coord_clone.notified().await });

        // Yield so the spawned task starts awaiting.
        tokio::task::yield_now().await;
        assert!(!coord.is_draining());

        coord.begin_drain();
        waiter.await.expect("waiter ok");
        assert!(coord.is_draining());
    }

    #[tokio::test]
    async fn notified_returns_immediately_if_already_draining() {
        let coord = ShutdownCoordinator::new();
        coord.begin_drain();
        // Should not hang.
        tokio::time::timeout(std::time::Duration::from_millis(100), coord.notified())
            .await
            .expect("notified returned");
    }

    #[tokio::test]
    async fn request_restart_sets_flag_and_begins_drain() {
        let coord = ShutdownCoordinator::new();
        assert!(!coord.is_draining());
        assert!(!coord.is_restart_requested());

        coord.request_restart();

        assert!(coord.is_draining(), "request_restart should also drain");
        assert!(coord.is_restart_requested());

        // Idempotent.
        coord.request_restart();
        assert!(coord.is_restart_requested());

        // `notified` returns immediately (drain already in progress).
        tokio::time::timeout(std::time::Duration::from_millis(100), coord.notified())
            .await
            .expect("notified after request_restart");
    }

    #[test]
    fn begin_drain_alone_does_not_set_restart_flag() {
        let coord = ShutdownCoordinator::new();
        coord.begin_drain();
        assert!(coord.is_draining());
        assert!(
            !coord.is_restart_requested(),
            "drain without restart must not set the restart flag"
        );
    }

    #[test]
    fn inflight_guard_decrements_on_drop() {
        let coord = ShutdownCoordinator::new();
        assert_eq!(coord.inflight(), 0);
        let g1 = coord.begin_work();
        let g2 = coord.begin_work();
        assert_eq!(coord.inflight(), 2);
        drop(g1);
        assert_eq!(coord.inflight(), 1);
        drop(g2);
        assert_eq!(coord.inflight(), 0);
    }
}
