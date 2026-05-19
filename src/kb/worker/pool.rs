//! Worker pool: a single tokio task that claims + runs jobs, with
//! `reclaim_stale` interleaved every `reclaim_interval`. Single-task
//! design keeps shutdown observation trivial — the loop checks an
//! `AtomicBool` at the top of each iteration and on every wake from
//! the idle sleep.

use crate::kb::store::{jobs, KbStore};
use crate::kb::worker::handlers::{DefaultDispatcher, HandlerCtx, JobHandler};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub worker_id: String,
    pub claim_ttl_ms: i64,
    pub poll_idle: Duration,
    pub reclaim_interval: Duration,
    pub max_attempts: u32,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            worker_id: format!("worker-{}", ulid::Ulid::new()),
            claim_ttl_ms: 60_000,
            poll_idle: Duration::from_millis(100),
            reclaim_interval: Duration::from_secs(30),
            max_attempts: 5,
        }
    }
}

pub struct WorkerPool {
    main: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

impl WorkerPool {
    /// Start with `DefaultDispatcher`.
    ///
    /// **Runtime requirement:** must be called from a multi-threaded
    /// tokio runtime. `run_main` wraps the synchronous handler call
    /// with `tokio::task::block_in_place`, which panics on a
    /// single-threaded runtime.
    pub fn start(ctx: HandlerCtx, cfg: WorkerConfig) -> Self {
        Self::start_with_handler(ctx, cfg, Arc::new(DefaultDispatcher))
    }

    pub fn start_with_handler(
        ctx: HandlerCtx,
        cfg: WorkerConfig,
        handler: Arc<dyn JobHandler>,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let main = tokio::spawn(run_main(ctx, cfg, handler, shutdown.clone()));
        Self { main, shutdown }
    }

    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::Release);
        if let Err(e) = self.main.await {
            tracing::error!("kb worker pool exited with error: {e:#}");
        }
    }

    /// Test helper: run exactly one job synchronously.
    pub fn run_one_blocking(
        ctx: &HandlerCtx,
        cfg: &WorkerConfig,
        handler: &dyn JobHandler,
    ) -> Result<bool> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let claimed = {
            let wtx = ctx.store.begin_write()?;
            let claim = jobs::claim_next(&wtx, &cfg.worker_id, now_ms, cfg.claim_ttl_ms)?;
            wtx.commit()?;
            claim
        };
        let Some((job, token)) = claimed else {
            return Ok(false);
        };
        match handler.handle(ctx, &job.kind) {
            Ok(()) => {
                let wtx = ctx.store.begin_write()?;
                jobs::mark_done(&wtx, &job.id, &token.token)?;
                wtx.commit()?;
                Ok(true)
            }
            Err(e) => {
                let wtx = ctx.store.begin_write()?;
                if job.attempts + 1 >= cfg.max_attempts {
                    jobs::mark_failed(&wtx, &job.id, &token.token, &format!("{e:#}"))?;
                } else {
                    jobs::requeue(&wtx, &job.id)?;
                }
                wtx.commit()?;
                Ok(true)
            }
        }
    }
}

async fn run_main(
    ctx: HandlerCtx,
    cfg: WorkerConfig,
    handler: Arc<dyn JobHandler>,
    shutdown: Arc<AtomicBool>,
) {
    let mut next_reclaim = Instant::now() + cfg.reclaim_interval;
    while !shutdown.load(Ordering::Acquire) {
        if Instant::now() >= next_reclaim {
            run_reclaim_once(&ctx.store);
            next_reclaim = Instant::now() + cfg.reclaim_interval;
        }

        let did_work = match tokio::task::block_in_place(|| {
            WorkerPool::run_one_blocking(&ctx, &cfg, handler.as_ref())
        }) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("kb worker main loop error: {e:#}");
                false
            }
        };
        if !did_work {
            let deadline = Instant::now() + cfg.poll_idle;
            while Instant::now() < deadline {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

fn run_reclaim_once(store: &KbStore) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let res = (|| -> Result<usize> {
        let wtx = store.begin_write()?;
        let n = jobs::reclaim_stale(&wtx, now_ms)?.len();
        wtx.commit()?;
        Ok(n)
    })();
    match res {
        Ok(n) if n > 0 => tracing::info!("kb worker: reclaimed {n} stale jobs"),
        Ok(_) => {}
        Err(e) => tracing::error!("kb worker reclaim error: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::{canonicalize_by_mime, CanonicalizeInput};
    use crate::kb::embedder::{KbEmbedder, StubEmbedder};
    use crate::kb::jobs::JobStatus;
    use crate::kb::paths::KbPaths;
    use crate::kb::pipeline::{ingest_canonicalized, IngestInput};
    use crate::kb::store::{chunks as chunks_store, jobs as jobs_store};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, HandlerCtx, WorkerConfig, String, String) {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(KbStore::open(&tmp.path().join("kb.redb")).unwrap());
        let paths = Arc::new(KbPaths::new(tmp.path().join("kb")));
        paths.ensure_layout().unwrap();
        let embedder: Arc<dyn KbEmbedder> = Arc::new(StubEmbedder::default());

        let bytes = b"# T\n\nbody.";
        let canon = canonicalize_by_mime(CanonicalizeInput {
            bytes,
            mime: "text/markdown",
            hint_title: Some("t"),
            logical_source_id_seed: None,
        })
        .unwrap()
        .unwrap();
        let lsid = canon.metadata.logical_source_id.0.clone();
        let out = ingest_canonicalized(
            &store,
            IngestInput {
                canon: &canon,
                raw_bytes: bytes,
                raw_ext: "md",
                visibility: None,
                owner_user_id: None,
                seen_key: None,
                source: None,
                paths: &paths,
            },
        )
        .unwrap();

        let index = Arc::new(crate::kb::index::KbIndex::open(&paths).unwrap());
        let ctx = HandlerCtx { store, paths, embedder, index };
        let cfg = WorkerConfig { worker_id: "w-test".into(), ..WorkerConfig::default() };
        (tmp, ctx, cfg, out.doc_id, lsid)
    }

    #[test]
    fn run_one_processes_ready_job() {
        let (_tmp, ctx, cfg, _doc_id, lsid) = fixture();
        let handler = DefaultDispatcher;
        assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
        let rtx = ctx.store.begin_read().unwrap();
        assert!(!chunks_store::chunks_for_logical(&rtx, &lsid).unwrap().is_empty());
        assert!(jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap().is_empty());
        let done = jobs_store::list_by_status(&rtx, JobStatus::Done).unwrap();
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn run_one_returns_false_when_idle() {
        let (_tmp, ctx, cfg, _doc_id, _lsid) = fixture();
        let handler = DefaultDispatcher;
        assert!(WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
        assert!(!WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap());
    }

    #[test]
    fn handler_error_requeues_until_max_attempts() {
        let (_tmp, ctx, mut cfg, _doc_id, _lsid) = fixture();
        cfg.max_attempts = 2;
        struct AlwaysFails;
        impl JobHandler for AlwaysFails {
            fn handle(&self, _: &HandlerCtx, _: &crate::kb::jobs::JobKind) -> Result<()> {
                Err(anyhow::anyhow!("nope"))
            }
        }
        let h = AlwaysFails;
        WorkerPool::run_one_blocking(&ctx, &cfg, &h).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let ready = jobs_store::list_by_status(&rtx, JobStatus::Ready).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].attempts, 1);
        drop(rtx);
        WorkerPool::run_one_blocking(&ctx, &cfg, &h).unwrap();
        let rtx = ctx.store.begin_read().unwrap();
        let failed = jobs_store::list_by_status(&rtx, JobStatus::Failed).unwrap();
        assert_eq!(failed.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_exits_within_poll_idle_plus_margin() {
        let (_tmp, ctx, mut cfg, _doc_id, _lsid) = fixture();
        cfg.poll_idle = Duration::from_millis(50);
        {
            let handler = DefaultDispatcher;
            WorkerPool::run_one_blocking(&ctx, &cfg, &handler).unwrap();
        }
        let pool = WorkerPool::start(ctx, cfg);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let start = std::time::Instant::now();
        pool.shutdown().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "shutdown took {elapsed:?}, expected < 200ms"
        );
    }
}
