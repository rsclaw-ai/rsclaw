//! Gateway-wide graceful shutdown coordinator.
//!
//! On graceful restart, multiple async tasks need to:
//!   1. Stop accepting new work (set `draining = true`).
//!   2. Wait for currently-running work to complete (`inflight == 0`).
//!   3. Exit cleanly so the parent process can spawn the replacement.
//!
//! Subscribers:
//!   - `axum::serve(...).with_graceful_shutdown(coord.notified())` — drains HTTP.
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

use tokio::sync::Notify;

/// Coordinates graceful shutdown across the HTTP server, task queue worker,
/// and channel handlers. Cheap to clone (single `Arc`).
#[derive(Clone, Default)]
pub struct ShutdownCoordinator {
    inner: Arc<ShutdownInner>,
}

#[derive(Default)]
struct ShutdownInner {
    /// Set to true when graceful shutdown begins. Workers check this before
    /// pulling new work; HTTP server stops accepting new connections.
    draining: AtomicBool,
    /// Wakes up `axum::serve(...).with_graceful_shutdown(future)` and any
    /// other awaiter that wants to be notified the moment drain begins.
    notify: Notify,
    /// Number of in-flight units of work (HTTP requests, agent turns,
    /// task queue entries) currently being processed. Restart waits for
    /// this to drop to zero (with a timeout) before terminating the process.
    inflight: AtomicUsize,
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
        self.inner.notify.notify_waiters();
    }

    /// Wait for `begin_drain` to be called. If drain has already begun, this
    /// awaits the next call (so subscribers that arrive late should check
    /// `is_draining` first if they need a one-shot guarantee).
    ///
    /// Intended for `axum::serve(...).with_graceful_shutdown(future)`.
    pub async fn notified(&self) {
        // Fast path — already draining.
        if self.is_draining() {
            return;
        }
        // Slow path — wait for begin_drain.
        let waiter = self.inner.notify.notified();
        // Re-check after subscribing to close the race where drain happens
        // between our first check and `notified.await`.
        if self.is_draining() {
            return;
        }
        waiter.await;
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
