//! Cron job scheduler — runs periodic agent tasks (AGENTS.md §16).
//!
//! Uses a self-implemented timer loop (tokio::time::sleep) instead of
//! tokio-cron-scheduler, for reliable cross-platform behavior.
//!
//! Schedule format: standard 5-field cron "min hr dom mon dow".
//! Timezone: stored in schedule but currently executes in UTC.
//!
//! Each job run uses an isolated session (`cron:<jobId>:<run timestamp>`)
//! unless the job explicitly configures a persistent session key. Concurrent
//! runs are capped by `max_concurrent_runs`.
//!
//! The DATA / PERSISTENCE / PURE-COMPUTE layer lives in the lower
//! `rsclaw-cron` crate; this module re-exports those items (so existing
//! `crate::cron::X` paths keep resolving) and hosts the runtime
//! orchestrator [`CronRunner`], which is wired to `agent`, `gateway`,
//! `ws`, and channels and therefore cannot move down.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use rsclaw_agent::{AgentMessage, AgentRegistry};
use rsclaw_channel::{ChannelManager, OutboundMessage};
use rsclaw_config::schema::{CronConfig, CronDelivery};
// Re-export the DATA / PERSISTENCE / PURE-COMPUTE layer so existing
// `crate::cron::X` callers across agent / cmd / server / gateway keep
// resolving unchanged.
pub use rsclaw_cron::{
    CRON_FILE_LOCK, CronIter, CronJob, CronJobState, CronPayload, CronSchedule, CronScheduleTagged,
    CronStore, RunLogEntry, build_run_log_entry, compute_next_run_from_expr,
    cron_jobs_config_equal, cron_store, current_timestamp_ms, error_backoff_ms,
    export_cron_jobs_to_file, extract_saved_files_content, init_cron_store, install_reload_sender,
    load_cron_jobs, load_cron_jobs_from_file, reconcile_file_to_redb_on_boot,
    resolve_cron_store_path, save_cron_jobs, trigger_reload, validate_cron_expr,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{Semaphore, broadcast},
    time::sleep,
};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Constants (runner-only — pure-compute backoff table lives in rsclaw-cron)
// ---------------------------------------------------------------------------

/// Maximum time between timer ticks (ms). Prevents schedule drift.
const MAX_TIMER_DELAY_MS: u64 = 60_000;

/// Minimum gap between re-triggering the same job (ms). Prevents spin-loops.
const MIN_REFIRE_GAP_MS: u64 = 2_000;

/// Max consecutive errors before a job is silently skipped (won't block
/// scheduler).
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// After this many ms without completing, a running job is considered stale.
const STUCK_RUN_MS: u64 = 2 * 60 * 60 * 1000; // 2 hours

/// Sentinel error message produced when a running job is cancelled because
/// reload detected the job was deleted, disabled, or its config changed.
/// Used to distinguish reload-driven cancellation from actual failures so
/// `consecutive_errors` is not bumped and the new job version starts clean.
const CANCEL_BY_RELOAD: &str = "cron: cancelled by reload";

// ---------------------------------------------------------------------------
// CronRunner
// ---------------------------------------------------------------------------

pub struct CronRunner {
    jobs: Vec<CronJob>,
    agents: Arc<AgentRegistry>,
    /// Optional direct WASM plugin access for deterministic cron preflights.
    wasm_plugins: Option<Arc<Vec<rsclaw_plugin::WasmPlugin>>>,
    /// Agent IDs whose cron turns run without a timeout (daemon loops).
    daemon_agent_ids: Vec<String>,
    channels: Arc<ChannelManager>,
    run_log_dir: PathBuf,
    store_path: PathBuf,
    semaphore: Arc<Semaphore>,
    default_delivery: Option<CronDelivery>,
    reload_tx: broadcast::Sender<()>,
    ws_conns: Arc<crate::ws::ConnRegistry>,
    /// Optional graceful-shutdown coordinator. When draining, the scheduler
    /// loop exits at the next iteration without firing further jobs. Tests
    /// that don't care about graceful shutdown can pass `None`.
    shutdown: Option<crate::gateway::ShutdownCoordinator>,
    /// If true, the cron.json5 file failed to parse. Skip ALL saves to
    /// avoid wiping user's config. The runner will still operate with
    /// whatever jobs it could parse, but won't overwrite the file.
    parse_failed: bool,
}

impl CronRunner {
    /// Construct a new cron runner without a shutdown coordinator. Suitable
    /// for tests that don't exercise graceful shutdown.
    pub fn new(
        config: &CronConfig,
        jobs: Vec<CronJob>,
        agents: Arc<AgentRegistry>,
        channels: Arc<ChannelManager>,
        data_dir: PathBuf,
        reload_tx: broadcast::Sender<()>,
        ws_conns: Arc<crate::ws::ConnRegistry>,
    ) -> Self {
        Self::new_with_shutdown(
            config, jobs, false, agents, channels, data_dir, reload_tx, ws_conns, None,
        )
    }

    /// Construct a new cron runner with an explicit shutdown coordinator.
    /// The runtime uses this constructor; tests typically use [`new`].
    ///
    /// # Arguments
    /// * `parse_failed` - If true, skip ALL saves (including after job
    ///   execution). Set to true when cron.json5 failed to parse, to avoid
    ///   wiping user's config.
    pub fn new_with_shutdown(
        config: &CronConfig,
        jobs: Vec<CronJob>,
        parse_failed: bool,
        agents: Arc<AgentRegistry>,
        channels: Arc<ChannelManager>,
        data_dir: PathBuf,
        reload_tx: broadcast::Sender<()>,
        ws_conns: Arc<crate::ws::ConnRegistry>,
        shutdown: Option<crate::gateway::ShutdownCoordinator>,
    ) -> Self {
        let run_log_dir = data_dir.join("cron");
        // Use the canonical cron.json5 path — the same file the UI, CLI,
        // and tool_cron read/write. Previously this was a separate
        // `cron_store.json` under data_dir/, so save_store() updates
        // (including one-shot job removal) never landed in the file
        // anyone else looked at — the next reload would resurrect the
        // already-fired one-shot job.
        let store_path = resolve_cron_store_path();
        if let Err(e) = std::fs::create_dir_all(&run_log_dir) {
            tracing::warn!("failed to create cron run log dir: {e}");
        }
        Self {
            jobs,
            agents,
            wasm_plugins: None,
            channels,
            run_log_dir,
            store_path,
            semaphore: Arc::new(Semaphore::new(4)),
            default_delivery: config.default_delivery.clone(),
            reload_tx,
            ws_conns,
            shutdown,
            parse_failed,
            daemon_agent_ids: Vec::new(),
        }
    }

    /// Agent IDs that run as long-lived daemon loops — their cron-triggered
    /// turns are NOT subject to the per-job timeout (they loop forever by
    /// design; see `agents.defaults.daemon_agent_ids`).
    #[must_use]
    pub fn with_daemon_agent_ids(mut self, ids: Vec<String>) -> Self {
        self.daemon_agent_ids = ids;
        self
    }

    /// Enable deterministic WASM preflights for jobs that opt in through
    /// `wakeMode`.
    #[must_use]
    pub fn with_wasm_plugins(mut self, plugins: Arc<Vec<rsclaw_plugin::WasmPlugin>>) -> Self {
        self.wasm_plugins = Some(plugins);
        self
    }

    pub fn jobs(&self) -> &[CronJob] {
        &self.jobs
    }

    /// Check if file parsing failed - callers should avoid saving to disk
    pub fn parse_failed(&self) -> bool {
        self.parse_failed
    }

    /// Save jobs after a cron task completion.
    ///
    /// **F2 semantics (redb-authoritative)**: each job is patched
    /// individually in redb. From memory we ONLY write the `state`
    /// sub-object (run statistics) plus a forced disable when memory
    /// says `enabled=false` (covers auto-disable after
    /// MAX_CONSECUTIVE_ERRORS and one-shot completion). User-config
    /// fields (`enabled=true`, `schedule`, `payload`, `message`,
    /// `delivery`, etc.) come from the redb-stored value and are
    /// NEVER overwritten with the in-memory copy — that's what
    /// produced the "I disabled it but cron keeps firing" race.
    ///
    /// After updating redb, the file `cron.json5` is re-exported as a
    /// best-effort human-readable copy.
    pub(crate) async fn save_store(&self, jobs: &[CronJob]) -> Result<()> {
        if self.parse_failed {
            return Ok(());
        }

        // Fast path: redb available (production).
        if let Some(store) = cron_store() {
            for mem_job in jobs {
                let merged = match store.cron_get(&mem_job.id) {
                    Ok(Some(json)) => match serde_json::from_str::<CronJob>(&json) {
                        Ok(mut redb_job) => {
                            // state: memory wins (it's the authoritative
                            // run-statistics source).
                            redb_job.state = mem_job.state.clone();
                            // enabled: memory can disable, never re-enable.
                            if !mem_job.enabled {
                                redb_job.enabled = false;
                            }
                            // If the user edited the schedule in cron.json5,
                            // adopt it and force a next-run recompute — otherwise
                            // the stale `next_run_at_ms` carried in state keeps
                            // firing on the OLD time (or never, if it's past).
                            let sched_changed = serde_json::to_string(&redb_job.schedule).ok()
                                != serde_json::to_string(&mem_job.schedule).ok();
                            if sched_changed {
                                redb_job.schedule = mem_job.schedule.clone();
                                if let Some(st) = redb_job.state.as_mut() {
                                    st.next_run_at_ms = None;
                                }
                            }
                            redb_job
                        }
                        Err(e) => {
                            warn!(err = %e, job_id = %mem_job.id, "cron: redb job parse failed; using memory version");
                            mem_job.clone()
                        }
                    },
                    Ok(None) => {
                        // First-time write for this job (e.g. cron.add
                        // path before the next reload).
                        mem_job.clone()
                    }
                    Err(e) => {
                        warn!(err = %e, job_id = %mem_job.id, "cron: redb cron_get failed; skipping");
                        continue;
                    }
                };
                let json = match serde_json::to_string(&merged) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(err = %e, job_id = %mem_job.id, "cron: serialize failed");
                        continue;
                    }
                };
                if let Err(e) = store.cron_put(&merged.id, &json) {
                    warn!(err = %e, job_id = %merged.id, "cron: redb cron_put failed");
                }
            }

            // Best-effort export: read the canonical job set back from
            // redb (so we capture any user-config preservation done
            // above) and write `cron.json5` for human readability.
            if let Ok(entries) = store.cron_list() {
                let exported: Vec<CronJob> = entries
                    .into_iter()
                    .filter_map(|(_, j)| serde_json::from_str(&j).ok())
                    .collect();
                tokio::task::spawn_blocking(move || {
                    export_cron_jobs_to_file(&exported);
                });
            }
            return Ok(());
        }

        // Legacy fallback: file-only (tests, standalone tools).
        let store_data = CronStore {
            version: 1,
            jobs: jobs.to_vec(),
        };
        let json = serde_json::to_string_pretty(&store_data)?;
        let tmp = format!("{}.tmp", self.store_path.display());
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &self.store_path).await?;
        Ok(())
    }

    /// Start all enabled cron jobs and block until shutdown is signalled.
    ///
    /// When constructed with a `ShutdownCoordinator`, waits on
    /// `shutdown.notified()` so the gateway-wide drain (HTTP /shutdown,
    /// SIGINT, SIGTERM) coordinates a single graceful exit. Without one,
    /// falls back to listening for Ctrl-C directly (test/standalone use).
    pub async fn run(&self) -> Result<()> {
        info!("cron scheduler starting");

        let mut jobs = self.jobs.clone();
        let now_ms = current_timestamp_ms();

        // Initialize state for each job
        for job in &mut jobs {
            if job.state.is_none() {
                job.state = Some(CronJobState {
                    consecutive_errors: 0,
                    ..Default::default()
                });
            }

            let state = job.state.as_mut().unwrap();

            // Clear stale running marker
            if let Some(running_at) = state.running_at_ms {
                if now_ms - running_at > STUCK_RUN_MS {
                    warn!(job_id = %job.id, "cron: clearing stale running marker");
                    state.running_at_ms = None;
                }
            }

            // Compute next_run_at_ms if not set OR if the stored value is in the past
            // (may have been computed with the old buggy algorithm that ignored timezone)
            if state.next_run_at_ms.is_none() || state.next_run_at_ms.is_some_and(|t| t <= now_ms) {
                let old_ts = state.next_run_at_ms;
                state.next_run_at_ms = job.schedule.compute_next_run(now_ms);
                info!(job_id = %job.id, old = ?old_ts, new = ?state.next_run_at_ms, "cron: recomputed next_run_at_ms");
            }
        }

        // Sweep zombie one-shot jobs left disabled by previous runs that
        // crashed or used the pre-fix `cron_store.json` save path. These
        // would otherwise sit in cron.json5 forever, since the in-loop
        // retain only fires after a try_recv result event.
        let zombies_before = jobs.len();
        jobs.retain(|j| !(j.schedule.is_once() && !j.enabled));
        if jobs.len() < zombies_before {
            info!(
                removed = zombies_before - jobs.len(),
                "cron: cleaned up zombie one-shot jobs at startup"
            );
        }

        // Persist initial state (skip if file failed to parse - don't wipe user's
        // config)
        if !self.parse_failed {
            if let Err(e) = self.save_store(&jobs).await {
                warn!(err = %e, "cron: failed to save initial store");
            }
        } else {
            warn!(
                "cron: parse failed - all saves disabled until cron.json5 syntax errors are fixed"
            );
        }

        let enabled_count = jobs.iter().filter(|j| j.enabled).count();
        info!(
            total = jobs.len(),
            enabled = enabled_count,
            next_wake = jobs
                .iter()
                .filter_map(|j| j.state.as_ref().and_then(|s| s.next_run_at_ms))
                .min()
                .unwrap_or(0),
            "cron scheduler started"
        );

        // Main timer loop
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let semaphore = Arc::clone(&self.semaphore);
        let reload_rx = self.reload_tx.subscribe();

        let runner = self.clone();
        let timer_handle = tokio::spawn(async move {
            runner
                .timer_loop(jobs, running_clone, semaphore, reload_rx)
                .await;
        });

        // Wait for shutdown: prefer the shared coordinator (gateway-wide
        // drain) so SIGINT/SIGTERM/HTTP shutdown all funnel through one
        // path. Fall back to a direct Ctrl-C listener for tests that
        // don't pass a coordinator.
        if let Some(sd) = self.shutdown.clone() {
            sd.notified().await;
        } else {
            tokio::signal::ctrl_c().await?;
        }
        info!("cron scheduler shutting down");
        running.store(false, std::sync::atomic::Ordering::SeqCst);

        // Wake the timer by dropping the permit briefly
        sleep(Duration::from_millis(100)).await;

        timer_handle.await.ok();
        info!("cron scheduler stopped");
        Ok(())
    }

    async fn timer_loop(
        &self,
        mut jobs: Vec<CronJob>,
        running: Arc<std::sync::atomic::AtomicBool>,
        semaphore: Arc<Semaphore>,
        mut reload_rx: broadcast::Receiver<()>,
    ) {
        // Channel for collecting job results asynchronously.
        let (result_tx, mut result_rx) =
            tokio::sync::mpsc::channel::<(String, bool, u64, u64, Option<String>)>(64);

        // Cancel flags for running jobs — set to true to signal abort on deletion.
        let mut cancel_flags: HashMap<String, Arc<std::sync::atomic::AtomicBool>> = HashMap::new();

        // Clear orphaned running_at_ms states from previous app run.
        // When the app restarts, any jobs that were running at shutdown will have
        // running_at_ms set but no actual spawned task, causing them to be stuck.
        let orphan_count = jobs
            .iter_mut()
            .filter(|j| j.state.as_ref().and_then(|s| s.running_at_ms).is_some())
            .count();
        if orphan_count > 0 {
            warn!(
                count = orphan_count,
                "cron: clearing orphaned running_at_ms states from previous run"
            );
            for job in jobs.iter_mut() {
                if let Some(state) = job.state.as_mut() {
                    if state.running_at_ms.is_some() {
                        info!(job_id = %job.id, "cron: clearing orphaned running_at_ms");
                        state.running_at_ms = None;
                        // Recompute next_run_at_ms if needed
                        if state.next_run_at_ms.is_none()
                            || state
                                .next_run_at_ms
                                .map(|t| t <= current_timestamp_ms())
                                .unwrap_or(true)
                        {
                            state.next_run_at_ms =
                                job.schedule.compute_next_run(current_timestamp_ms());
                        }
                    }
                }
            }
            // Save the cleaned state
            if let Err(e) = self.save_store(&jobs).await {
                warn!(err = %e, "cron: failed to save store after clearing orphaned states");
            }
        }

        loop {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            if let Some(s) = &self.shutdown {
                if s.is_draining() {
                    info!("cron scheduler: drain signaled, stopping job dispatch");
                    break;
                }
            }

            let now_ms = current_timestamp_ms();

            // Find next wake time among enabled jobs
            let next_wake_job = jobs
                .iter()
                .filter(|j| j.enabled)
                .filter_map(|j| {
                    j.state
                        .as_ref()
                        .and_then(|s| s.next_run_at_ms)
                        .map(|t| (t, &j.id, &j.name))
                })
                .min_by_key(|(t, _, _)| *t);

            let next_wake = next_wake_job.map(|(t, _, _)| t);

            // Auto-remove expired once jobs (past due by > 5 minutes).
            // This prevents stale once jobs from spamming "next_wake in the past" warnings.
            let expired_threshold_ms = 5 * 60 * 1000;
            let before_len = jobs.len();
            jobs.retain(|j| {
                if !j.schedule.is_once() || !j.enabled { return true; }
                if let Some(state) = &j.state {
                    if let Some(next_at) = state.next_run_at_ms {
                        if now_ms > next_at + expired_threshold_ms {
                            info!(job_id = %j.id, name = ?j.name, "cron: removing expired once job (past due by {}s)", (now_ms - next_at) / 1000);
                            return false;
                        }
                    }
                }
                true
            });
            if jobs.len() < before_len {
                if let Err(e) = self.save_store(&jobs).await {
                    warn!(err = %e, "cron: failed to persist after expired job cleanup");
                }
            }

            debug!(
                next_wake = next_wake.unwrap_or(0),
                now_ms, "cron: timer tick"
            );

            let delay_ms = match next_wake {
                Some(next_wake) => {
                    let delay = next_wake.saturating_sub(now_ms);
                    if delay == 0 {
                        MIN_REFIRE_GAP_MS
                    } else {
                        delay.min(MAX_TIMER_DELAY_MS)
                    }
                }
                None => {
                    // No jobs — wait max interval and re-check
                    debug!("cron: no jobs scheduled, waiting {}ms", MAX_TIMER_DELAY_MS);
                    MAX_TIMER_DELAY_MS
                }
            };

            // Use tokio::select! to wait for either timer or reload signal
            let mut reload_triggered = tokio::select! {
                _ = sleep(Duration::from_millis(delay_ms)) => {
                    false
                }
                result = reload_rx.recv() => {
                    match result {
                        Ok(()) => true,
                        Err(broadcast::error::RecvError::Closed) => {
                            // Channel closed, exit loop
                            return;
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Lagged, but still reload
                            true
                        }
                    }
                }
            };

            // Close the select! wake-up race: if the sleep branch won the
            // race, a reload signal that arrived just before could be sitting
            // in the broadcast buffer.  Drain it now so we apply the new
            // file before computing due jobs — otherwise a freshly-deleted
            // cron could fire one extra time on this iteration.
            loop {
                match reload_rx.try_recv() {
                    Ok(()) => reload_triggered = true,
                    Err(broadcast::error::TryRecvError::Lagged(_)) => reload_triggered = true,
                    Err(_) => break,
                }
            }

            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            if reload_triggered {
                // Reload jobs from file
                let old_count = jobs.len();
                let (new_jobs, parse_ok) = load_cron_jobs();

                if !parse_ok {
                    // File has syntax errors - don't replace jobs, don't save
                    warn!(
                        old_count,
                        "cron: reload skipped - cron.json5 has syntax errors, fix before modifying"
                    );
                    continue;
                }

                let file_count = new_jobs.len();

                // Debug: check if disabled job is in new_jobs
                let disabled_in_file: Vec<_> = new_jobs
                    .iter()
                    .filter(|j| !j.enabled)
                    .map(|j| (&j.id, j.enabled))
                    .collect();
                info!(old_count, new_count = new_jobs.len(), file_count, disabled=?disabled_in_file, "cron: reload triggered, reloading from file");

                let (merged_jobs, modified_ids) = self.merge_jobs(&jobs, new_jobs, now_ms);
                jobs = merged_jobs;

                // Debug: check enabled state after merge
                let disabled_after_merge: Vec<_> = jobs
                    .iter()
                    .filter(|j| !j.enabled)
                    .map(|j| (&j.id, j.enabled))
                    .collect();
                info!(after_merge_count = jobs.len(), disabled=?disabled_after_merge, modified=?modified_ids, "cron: merge complete");

                // Cancel running tasks that were removed, disabled, OR whose
                // user-facing config changed.  The "modified" case matters
                // because a user editing a long-running job (e.g. switching a
                // 5-minute schedule to 30 minutes) expects the old in-flight
                // run on the OLD config to stop — otherwise it keeps using the
                // old message/payload/cadence side-by-side with the new one.
                let active_unchanged: HashSet<&str> = jobs
                    .iter()
                    .filter(|j| j.enabled && !modified_ids.contains(&j.id))
                    .map(|j| j.id.as_str())
                    .collect();
                let to_cancel: Vec<String> = cancel_flags
                    .keys()
                    .filter(|id| !active_unchanged.contains(id.as_str()))
                    .cloned()
                    .collect();
                for id in &to_cancel {
                    if let Some(flag) = cancel_flags.remove(id) {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        let reason = if modified_ids.contains(id) {
                            "modified"
                        } else {
                            "deleted/disabled"
                        };
                        info!(job_id = id, reason, "cron: cancelled running job");
                    }
                }

                if let Err(e) = self.save_store(&jobs).await {
                    warn!(err = %e, "cron: failed to save store after reload");
                }
                info!(
                    old_count,
                    new_count = jobs.len(),
                    file_count,
                    "cron jobs reloaded"
                );
                continue;
            }

            // Collect any completed job results FIRST, before checking due.
            // This is critical: if we skip try_recv when due.is_empty(), runningAtMs
            // will never be cleared, causing the job to be stuck forever.
            let mut collected_count = 0;
            while let Ok((job_id, success, duration_ms, started_at, error_msg)) =
                result_rx.try_recv()
            {
                collected_count += 1;
                info!(job_id = %job_id, success, duration_ms, "cron: collected job result via try_recv");
                cancel_flags.remove(&job_id);
                if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
                    if let Some(state) = job.state.as_mut() {
                        state.running_at_ms = None;
                        state.last_run_at_ms = Some(current_timestamp_ms());
                        state.last_duration_ms = Some(duration_ms);

                        let completion_time = started_at + duration_ms;

                        if success {
                            state.consecutive_errors = 0;
                            state.last_run_status = Some("ok".to_string());
                            state.last_status = Some("ok".to_string());
                            state.last_error = None;

                            // One-shot: disable after successful execution (will be removed below)
                            if job.schedule.is_once() {
                                info!(job_id = %job.id, "cron: one-shot job completed, marking for removal");
                                state.next_run_at_ms = None;
                                job.enabled = false;
                            } else {
                                // Compute next run normally
                                state.next_run_at_ms =
                                    job.schedule.compute_next_run(completion_time);
                            }
                            info!(job_id = %job.id, next_run_at_ms = state.next_run_at_ms, "cron: updated next_run_at_ms after success");
                        } else if error_msg.as_deref() == Some(CANCEL_BY_RELOAD) {
                            // Cancellation triggered by reload (delete / disable /
                            // config edit).  Treat as benign: don't bump
                            // consecutive_errors, don't apply backoff, don't
                            // auto-disable.  Leave next_run_at_ms alone — for a
                            // schedule edit, merge_jobs has already recomputed it
                            // for the new cadence; for a non-schedule edit the
                            // existing cadence still applies.
                            state.last_run_status = Some("cancelled".to_string());
                            state.last_status = Some("cancelled".to_string());
                            state.last_error = error_msg;
                            info!(
                                job_id = %job.id,
                                next_run_at_ms = state.next_run_at_ms,
                                "cron: run cancelled by reload (config changed / disabled / deleted)"
                            );
                        } else {
                            state.consecutive_errors += 1;
                            state.last_run_status = Some("error".to_string());
                            state.last_status = Some("error".to_string());
                            state.last_error = error_msg;

                            if matches!(
                                job.wake_mode.as_deref(),
                                Some("wechat-ios-monitor" | "wechat-android-monitor")
                            ) {
                                // Device availability is transient. Keep the minute-level
                                // sales monitor alive so it reconnects promptly and still
                                // reaches its ten-minute friend-request sweep after recovery.
                                state.next_run_at_ms =
                                    job.schedule.compute_next_run(completion_time);
                                info!(
                                    job_id = %job.id,
                                    consecutive_errors = state.consecutive_errors,
                                    next_run_at_ms = state.next_run_at_ms,
                                    "cron: monitor error; keeping scheduled cadence"
                                );
                            } else {
                                // Apply exponential backoff for errored jobs
                                let backoff = error_backoff_ms(state.consecutive_errors);
                                let backoff_next = completion_time + backoff;
                                let normal_next = job.schedule.compute_next_run(completion_time);
                                // Use whichever is later: the natural next run or the backoff delay
                                state.next_run_at_ms = Some(
                                    normal_next
                                        .map(|n| n.max(backoff_next))
                                        .unwrap_or(backoff_next),
                                );

                                info!(
                                    job_id = %job.id,
                                    consecutive_errors = state.consecutive_errors,
                                    backoff_ms = backoff,
                                    next_run_at_ms = state.next_run_at_ms,
                                    "cron: applying error backoff"
                                );

                                // Auto-disable after max consecutive errors
                                if state.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                    warn!(
                                        job_id = %job.id,
                                        consecutive_errors = state.consecutive_errors,
                                        "cron: disabling job after repeated failures"
                                    );
                                    job.enabled = false;
                                }
                            }
                        }
                    }
                }
            }

            // Persist updated state if any results were collected
            if collected_count > 0 {
                if let Err(e) = self.save_store(&jobs).await {
                    warn!(err = %e, "cron: failed to save store after collecting results");
                }
            }

            let due: Vec<_> = jobs
                .iter_mut()
                .filter(|j| {
                    j.enabled
                        && j.state
                            .as_ref()
                            .and_then(|s| s.next_run_at_ms)
                            .map(|t| t <= now_ms)
                            .unwrap_or(false)
                        && j.state.as_ref().and_then(|s| s.running_at_ms).is_none()
                })
                .map(|j| j.id.clone())
                .collect();

            // Debug: log enabled state of all jobs that are due but shouldn't fire
            if !due.is_empty() {
                let disabled_due: Vec<_> = jobs
                    .iter()
                    .filter(|j| {
                        !j.enabled
                            && j.state
                                .as_ref()
                                .and_then(|s| s.next_run_at_ms)
                                .map(|t| t <= now_ms)
                                .unwrap_or(false)
                    })
                    .map(|j| j.id.clone())
                    .collect();
                if !disabled_due.is_empty() {
                    warn!(job_ids = ?disabled_due, "cron: these jobs are due but disabled!");
                }
            }

            if due.is_empty() {
                continue;
            }

            info!(count = due.len(), "cron: {} jobs due", due.len());

            // Execute due jobs concurrently — spawn and continue immediately.
            // Results are collected via a channel, not by join_all.
            for job_id in due {
                let permit = semaphore.clone().acquire_owned().await.ok();
                if permit.is_none() {
                    // Max concurrency reached — remaining jobs will fire next tick.
                    break;
                }
                // Re-check drain after the await — `acquire_owned` can park
                // arbitrarily long on a saturated semaphore, and a restart can
                // arrive while parked. Without this re-check, a job slot
                // claimed during drain would spawn after `is_draining()`
                // returned true on the previous iteration, hiding from the
                // 60s drain window.
                if let Some(s) = &self.shutdown {
                    if s.is_draining() {
                        info!(
                            "cron scheduler: drain signaled during permit await, dropping job {}",
                            job_id
                        );
                        drop(permit);
                        break;
                    }
                }

                // Mark as running, render iter substitutions, and clone the job
                // for dispatch — all under a tight mutable borrow of `jobs`.
                let started_at = current_timestamp_ms();
                let (rendered_text, mut job) = {
                    let Some(job_ref) = jobs.iter_mut().find(|j| j.id == job_id) else {
                        continue;
                    };
                    if let Some(state) = job_ref.state.as_mut() {
                        state.running_at_ms = Some(started_at);
                        // Don't compute next_run_at_ms here; compute it AFTER
                        // the job finishes
                        // using the completion time, so interval-based jobs
                        // don't fire early
                    }
                    let rendered = if job_ref.iter.is_some() {
                        let r = job_ref.render_message();
                        if job_ref.advance_iter().is_none() {
                            // Reachable only when iter exists but items is
                            // empty — render_message produces the raw text
                            // unchanged, so the dispatch still does
                            // something useful, but we should warn so the
                            // operator can fix the job config.
                            tracing::warn!(
                                job_id = %job_ref.id,
                                "cron: iter set but items list is empty; cursor not advanced"
                            );
                        }
                        Some(r)
                    } else {
                        None
                    };
                    (rendered, job_ref.clone())
                };

                // Iter cycling: persist the advanced cursor BEFORE dispatch so a
                // crash/restart can never replay the same item.
                //
                // Trade-off: a reload-driven cancel that fires AFTER this point
                // (CANCEL_BY_RELOAD) leaves the cursor advanced even though the
                // current item never actually delivered. The next fire picks
                // up at the following item instead of retrying the cancelled
                // one. We accept this — "every fire moves the cursor" is
                // simpler to reason about than a rewind, matches the user
                // expectation that an iter job runs forward through the list,
                // and the alternative (rewinding under reload while keeping
                // the advance under a real crash) requires distinguishing
                // cancellation causes after the fact.
                if let Some(text) = rendered_text {
                    if let Err(e) = self.save_store(&jobs).await {
                        warn!(error = %e, job_id, "cron: failed to persist iter cursor; the next run may repeat the same item");
                    }
                    job.bake_message(text);
                }

                let permit = permit.expect("permit checked above");
                let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                cancel_flags.insert(job.id.clone(), Arc::clone(&cancelled));
                let job_id_for_log = job.id.clone(); // Clone BEFORE async move
                let agents = Arc::clone(&self.agents);
                let wasm_plugins = self.wasm_plugins.clone();
                let daemon_agent_ids = self.daemon_agent_ids.clone();
                let channels = Arc::clone(&self.channels);
                let run_log_dir = self.run_log_dir.clone();
                let default_delivery = self.default_delivery.clone();
                let ws_conns = Arc::clone(&self.ws_conns);
                // Track this job in the gateway's inflight count so a graceful
                // restart waits for it (until drain timeout) before exiting.
                let inflight_guard = self.shutdown.as_ref().map(|s| s.begin_work());

                let handle = tokio::spawn(async move {
                    let _inflight_guard = inflight_guard;
                    let start_time = current_timestamp_ms();
                    let job_started_at = started_at;
                    let prev_consecutive_errors = job
                        .state
                        .as_ref()
                        .map(|s| s.consecutive_errors)
                        .unwrap_or(0);
                    info!(job_id = %job.id, "cron job triggered");

                    let preflight_result = if let Some(plugin_name) =
                        wechat_monitor_plugin(job.wake_mode.as_deref())
                    {
                        match run_wechat_monitor_preflight(
                            &job,
                            wasm_plugins.as_deref(),
                            plugin_name,
                        )
                        .await
                        {
                            Ok(tick) => match tick_has_work(&tick) {
                                Ok(false) => Some(Ok("monitor tick: no changes".to_string())),
                                Ok(true) => {
                                    job.bake_message(format!(
                                        "{}\n\n确定性 monitor_tick 结果（已完成 UI 锁保护）；只处理此结果中的待办，不要再次调用 monitor_tick：\n{}",
                                        job.effective_message(),
                                        tick
                                    ));
                                    None
                                }
                                Err(error) => Some(Err(error)),
                            },
                            Err(error) => Some(Err(error)),
                        }
                    } else {
                        None
                    };
                    let monitor_agent_turn = preflight_result.is_none()
                        && wechat_monitor_plugin(job.wake_mode.as_deref()).is_some();

                    // systemEvent: deliver payload text directly — no agent call needed.
                    // execCommand: execute the command directly, bypassing agent and session
                    // history.
                    let result: Result<String> = if let Some(result) = preflight_result {
                        result
                    } else if job.payload.as_ref().and_then(|p| match p {
                        CronPayload::Structured { kind, .. } => kind.as_deref(),
                        _ => None,
                    }) == Some("systemEvent")
                    {
                        Ok(job.effective_message().to_owned())
                    } else if job.payload.as_ref().and_then(|p| match p {
                        CronPayload::Structured { kind, .. } => kind.as_deref(),
                        _ => None,
                    }) == Some("execCommand")
                    {
                        // Execute command directly, bypassing agent to avoid session history
                        // pollution
                        run_exec_command(
                            job.effective_message(),
                            job.payload.as_ref().and_then(|p| match p {
                                CronPayload::Structured {
                                    timeout_seconds, ..
                                } => *timeout_seconds,
                                _ => None,
                            }),
                            job.payload.as_ref().map(|p| p.summarize()).unwrap_or(false),
                            &job,
                            &agents,
                        )
                        .await
                    } else {
                        // Run with cancellation check — polls cancel flag every second.
                        tokio::select! {
                            r = run_cron_job(&job, &agents, &daemon_agent_ids) => r,
                            _ = async {
                                loop {
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                    if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                                        info!(job_id = %job.id, "cron job cancelled");
                                        break;
                                    }
                                }
                            } => {
                                Err(anyhow::anyhow!(CANCEL_BY_RELOAD))
                            }
                        }
                    };
                    if monitor_agent_turn {
                        if let Some(plugin_name) = wechat_monitor_plugin(job.wake_mode.as_deref()) {
                            if let Err(error) = release_wechat_monitor_agent_lock(
                                wasm_plugins.as_deref(),
                                plugin_name,
                            )
                            .await
                            {
                                warn!(
                                    job_id = %job.id,
                                    error = %error,
                                    "cron: monitor agent lock cleanup failed"
                                );
                            }
                        }
                    }
                    let duration_ms = current_timestamp_ms() - start_time;
                    drop(permit);

                    // Build delivery message with execution summary.  None means
                    // skip delivery entirely (used for reload-driven
                    // cancellation — that's a control-plane event, not
                    // something the user wants pushed to their channel).
                    let delivery_text: Option<String> = match &result {
                        Ok(output) if !output.trim().is_empty() => {
                            // Agent returned output, use it directly
                            Some(output.clone())
                        }
                        Ok(_) => {
                            // Success but no output - send summary
                            let job_name = job.name.as_deref().unwrap_or(&job.id);
                            let seconds = (duration_ms / 1000).to_string();
                            Some(rsclaw_i18n::t_fmt(
                                "cron_run_success",
                                rsclaw_i18n::default_lang(),
                                &[("name", job_name), ("seconds", &seconds)],
                            ))
                        }
                        Err(e) if e.to_string() == CANCEL_BY_RELOAD => {
                            // Reload cancelled this run.  Skip delivery so the
                            // user isn't spammed when they edit a job.
                            None
                        }
                        Err(e) => {
                            // Error - send error notification with consecutive failure count and
                            // backoff
                            let job_name = job.name.as_deref().unwrap_or(&job.id);
                            let consecutive = prev_consecutive_errors + 1;
                            let backoff = error_backoff_ms(consecutive);
                            let will_disable = consecutive >= MAX_CONSECUTIVE_ERRORS;

                            let backoff_text = if backoff < 60_000 {
                                format!("{}秒", backoff / 1000)
                            } else if backoff < 3_600_000 {
                                format!("{}分钟", backoff / 60_000)
                            } else {
                                format!("{}小时", backoff / 3_600_000)
                            };

                            let consecutive_str = consecutive.to_string();
                            let error_str = e.to_string();
                            Some(if will_disable {
                                rsclaw_i18n::t_fmt(
                                    "cron_run_failed_disabled",
                                    rsclaw_i18n::default_lang(),
                                    &[
                                        ("name", job_name),
                                        ("consecutive", &consecutive_str),
                                        ("error", &error_str),
                                    ],
                                )
                            } else {
                                rsclaw_i18n::t_fmt(
                                    "cron_run_failed_retry",
                                    rsclaw_i18n::default_lang(),
                                    &[
                                        ("name", job_name),
                                        ("consecutive", &consecutive_str),
                                        ("backoff", &backoff_text),
                                        ("error", &error_str),
                                    ],
                                )
                            })
                        }
                    };

                    // Delivery path: send_delivery → DesktopChannel (for desktop
                    // deliveries) broadcasts via ws_conns, so we don't need a
                    // separate direct broadcast here (would double-deliver).
                    let _ = &ws_conns; // kept in scope for future direct use

                    // Spawn delivery as a detached task so it doesn't block.
                    // The result is logged but we don't wait for it.
                    if let Some(delivery_text) = delivery_text {
                        let delivery_channels = Arc::clone(&channels);
                        let delivery_agents = Arc::clone(&agents);
                        let delivery_job = job.clone();
                        let delivery_default = default_delivery.clone();
                        tokio::spawn(async move {
                            if let Err(e) = send_delivery(
                                &delivery_channels,
                                &delivery_agents,
                                &delivery_job,
                                &delivery_default,
                                &delivery_text,
                            )
                            .await
                            {
                                warn!(job_id = %delivery_job.id, %e, "delivery failed");
                            }
                        });
                    }

                    let entry = build_run_log_entry(
                        &job,
                        result.is_ok(),
                        result.as_ref().err().map(|e| anyhow::anyhow!("{e}")),
                    );
                    if let Err(e) = write_run_log(&run_log_dir, &job.id, entry).await {
                        tracing::warn!(job_id = %job.id, "failed to write cron run log: {e}");
                    }

                    let error_msg = result.as_ref().err().map(|e| e.to_string());
                    (
                        job.id,
                        result.is_ok(),
                        duration_ms,
                        job_started_at,
                        error_msg,
                    )
                });

                // Send result back via channel for async collection.
                let result_tx = result_tx.clone();
                tokio::spawn(async move {
                    let result = handle.await;
                    match result {
                        Ok(r) => {
                            tracing::info!(job_id = %job_id_for_log, success = r.1, duration_ms = r.2, "cron: result sender got result, sending to channel");
                            if let Err(e) = result_tx.send(r).await {
                                tracing::warn!(job_id = %job_id_for_log, "cron: failed to send result to channel: {}", e);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(job_id = %job_id_for_log, "cron: handle.await failed (spawn error): {}", e);
                        }
                    }
                });
            }

            // Remove completed one-shot jobs (already disabled in try_recv handler above)
            let before = jobs.len();
            jobs.retain(|j| !(j.schedule.is_once() && !j.enabled));
            if jobs.len() < before {
                info!(
                    removed = before - jobs.len(),
                    "cron: cleaned up completed one-shot jobs"
                );
                if let Err(e) = self.save_store(&jobs).await {
                    warn!(err = %e, "cron: failed to save store after removing one-shot jobs");
                }
            }
        }
    }

    /// Manually trigger a job by ID (bypasses schedule).
    pub async fn trigger(&self, job_id: &str) -> Result<()> {
        let job = self
            .jobs
            .iter()
            .find(|j| j.id == job_id)
            .with_context(|| format!("cron job not found: {job_id}"))?;

        info!(job_id = %job.id, "manually triggering cron job");
        let _permit = self.semaphore.acquire().await?;
        // Track in the gateway inflight count so a graceful restart waits for
        // a manual /api/v1/cron/run invocation to finish (until drain timeout)
        // before re-execing.
        let _inflight_guard = self.shutdown.as_ref().map(|s| s.begin_work());
        let prev_consecutive_errors = job
            .state
            .as_ref()
            .map(|s| s.consecutive_errors)
            .unwrap_or(0);
        // systemEvent: deliver payload text directly — no agent call needed.
        // execCommand: execute the command directly, bypassing agent and session
        // history.
        let result: Result<String> = if job.payload.as_ref().and_then(|p| match p {
            CronPayload::Structured { kind, .. } => kind.as_deref(),
            _ => None,
        }) == Some("systemEvent")
        {
            Ok(job.effective_message().to_owned())
        } else if job.payload.as_ref().and_then(|p| match p {
            CronPayload::Structured { kind, .. } => kind.as_deref(),
            _ => None,
        }) == Some("execCommand")
        {
            run_exec_command(
                job.effective_message(),
                job.payload.as_ref().and_then(|p| match p {
                    CronPayload::Structured {
                        timeout_seconds, ..
                    } => *timeout_seconds,
                    _ => None,
                }),
                job.payload.as_ref().map(|p| p.summarize()).unwrap_or(false),
                job,
                &self.agents,
            )
            .await
        } else {
            run_cron_job(job, &self.agents, &self.daemon_agent_ids).await
        };
        let success = result.is_ok();

        // Build delivery message with execution summary
        let delivery_text = match &result {
            Ok(output) if !output.trim().is_empty() => output.clone(),
            Ok(_) => {
                let job_name = job.name.as_deref().unwrap_or(&job.id);
                rsclaw_i18n::t_fmt(
                    "cron_run_success_no_duration",
                    rsclaw_i18n::default_lang(),
                    &[("name", job_name)],
                )
            }
            Err(e) => {
                let job_name = job.name.as_deref().unwrap_or(&job.id);
                let consecutive = prev_consecutive_errors + 1;
                // Manual trigger: show error but don't mention auto-disable
                // (manual triggers don't count toward auto-disable threshold)
                let consecutive_str = consecutive.to_string();
                let error_str = e.to_string();
                rsclaw_i18n::t_fmt(
                    "cron_run_failed_manual",
                    rsclaw_i18n::default_lang(),
                    &[
                        ("name", job_name),
                        ("consecutive", &consecutive_str),
                        ("error", &error_str),
                    ],
                )
            }
        };

        // Delivery goes through send_delivery → DesktopChannel (which broadcasts
        // via ws_conns). A separate direct broadcast here would double-deliver.
        if let Err(e) = send_delivery(
            &self.channels,
            &self.agents,
            job,
            &self.default_delivery,
            &delivery_text,
        )
        .await
        {
            warn!(job_id = %job.id, %e, "delivery failed");
        }

        let log_err = if success {
            None
        } else {
            result.as_ref().err().map(|e| anyhow::anyhow!("{e:#}"))
        };
        let entry = build_run_log_entry(job, success, log_err);
        write_run_log(&self.run_log_dir, &job.id, entry).await?;
        result.map(|_| ())
    }

    /// Merge old jobs (with their state) with new jobs from file.
    /// Preserves running state and error counts for existing jobs.
    /// Jobs in old_jobs but NOT in new_jobs are dropped (deleted from file).
    /// Takes `now_ms` from the caller (timer_loop) to avoid redundant calls.
    ///
    /// When a job's schedule changes (e.g. user edits `*/1 * * * *` to
    /// `*/30 * * * *`), the cached `next_run_at_ms` was computed against the
    /// OLD cadence and would still fire under that old rhythm one more time
    /// before the new schedule kicks in.  Detect a schedule change here and
    /// force-recompute `next_run_at_ms` so the new cadence takes effect at the
    /// next reload tick.
    ///
    /// Also returns a set of ids whose user-facing config (any field other
    /// than runtime state and audit timestamps) changed since the previous
    /// load.  Caller uses this to cancel any in-flight execution of the OLD
    /// version so the new config takes effect cleanly — without this, a long
    /// 5-minute job whose schedule was just edited to 30 minutes would keep
    /// running on the old cadence side-by-side with the new one.
    fn merge_jobs(
        &self,
        old_jobs: &[CronJob],
        new_jobs: Vec<CronJob>,
        now_ms: u64,
    ) -> (Vec<CronJob>, HashSet<String>) {
        let mut result = Vec::with_capacity(new_jobs.len());
        let mut modified: HashSet<String> = HashSet::new();

        for mut new_job in new_jobs {
            let mut schedule_changed = false;
            // Try to find existing job by ID and preserve its state
            if let Some(old_job) = old_jobs.iter().find(|j| j.id == new_job.id) {
                // Detect schedule edit before we overwrite state.  Compare via
                // serde_json::Value so we don't have to derive PartialEq on the
                // CronSchedule enum (which would force PartialEq on every
                // variant payload).
                schedule_changed = serde_json::to_value(&old_job.schedule).ok()
                    != serde_json::to_value(&new_job.schedule).ok();
                // Detect any user-facing config change (broader than schedule
                // alone — covers message/payload/delivery/sessionTarget/etc.).
                if !cron_jobs_config_equal(old_job, &new_job) {
                    modified.insert(new_job.id.clone());
                }
                // Preserve state from old job
                new_job.state = old_job.state.clone();
            } else {
                // New job - initialize state
                if new_job.state.is_none() {
                    new_job.state = Some(CronJobState {
                        consecutive_errors: 0,
                        ..Default::default()
                    });
                }
            }

            // Ensure next_run_at_ms is set; recompute when the schedule changed
            // so an edit cancels the old cadence rather than firing one more
            // time on the OLD schedule.
            if let Some(ref mut state) = new_job.state {
                if schedule_changed {
                    let next = new_job.schedule.compute_next_run(now_ms);
                    debug!(
                        job_id = %new_job.id,
                        old_next = ?state.next_run_at_ms,
                        new_next = ?next,
                        "cron: schedule changed, recomputing next_run_at_ms"
                    );
                    state.next_run_at_ms = next;
                } else if state.next_run_at_ms.is_none() {
                    state.next_run_at_ms = new_job.schedule.compute_next_run(now_ms);
                }
            }

            result.push(new_job);
        }

        (result, modified)
    }
}

impl Clone for CronRunner {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            agents: Arc::clone(&self.agents),
            wasm_plugins: self.wasm_plugins.clone(),
            daemon_agent_ids: self.daemon_agent_ids.clone(),
            channels: Arc::clone(&self.channels),
            run_log_dir: self.run_log_dir.clone(),
            store_path: self.store_path.clone(),
            semaphore: Arc::clone(&self.semaphore),
            default_delivery: self.default_delivery.clone(),
            reload_tx: self.reload_tx.clone(),
            ws_conns: Arc::clone(&self.ws_conns),
            shutdown: self.shutdown.clone(),
            parse_failed: self.parse_failed,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (runner-only — touch rsclaw_agent / crate::gateway)
// ---------------------------------------------------------------------------

fn wechat_monitor_plugin(wake_mode: Option<&str>) -> Option<&'static str> {
    match wake_mode {
        Some("wechat-ios-monitor") => Some("wechat-ios"),
        Some("wechat-android-monitor") => Some("wechat-android"),
        _ => None,
    }
}

async fn run_wechat_monitor_preflight(
    job: &CronJob,
    plugins: Option<&Vec<rsclaw_plugin::WasmPlugin>>,
    plugin_name: &str,
) -> Result<String> {
    let plugins = plugins.context("wechat monitor preflight has no WASM plugin registry")?;
    let plugin = plugins
        .iter()
        .find(|plugin| plugin.name == plugin_name)
        .with_context(|| format!("{plugin_name} monitor preflight plugin is not loaded"))?;
    let holder = format!("cron-preflight:{}", job.id);
    // Android's preflight performs a bounded 35-second screenshot pass. A
    // short lease lets the next minute recover promptly after cancellation;
    // iOS retains its longer lease for WDA's slower friend-request path.
    let ttl_secs = if plugin_name == "wechat-android" {
        90
    } else {
        330
    };
    let lock = plugin
        .call_tool(
            "acquire_ui_lock",
            serde_json::json!({ "holder": holder, "ttlSecs": ttl_secs }),
        )
        .await?;
    let acquired = lock
        .get("acquired")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !acquired {
        return Ok(serde_json::json!({ "skipped": "ui_lock_busy" }).to_string());
    }

    // A tunnel can accept TCP and then stop forwarding WDA bytes forever.
    // Bound the whole component invocation as a second line of defence; the
    // plugin-level request timeout cannot protect cron if the transport stalls
    // below reqwest's cancellation point.
    let timeout_secs = if plugin_name == "wechat-android" {
        55
    } else {
        35
    };
    let tick = tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        plugin.call_tool("monitor_tick", serde_json::json!({})),
    )
    .await
    .map_err(|_| anyhow!("{plugin_name} monitor_tick timed out after {timeout_secs}s"))
    .and_then(|result| result);
    match tokio::time::timeout(
        Duration::from_secs(10),
        plugin.call_tool("release_ui_lock", serde_json::json!({ "holder": holder })),
    )
    .await
    {
        Ok(Err(error)) => {
            warn!(job_id = %job.id, error = %error, "cron monitor preflight failed to release UI lock");
        }
        Err(_) => {
            warn!(job_id = %job.id, "cron monitor preflight release_ui_lock timed out");
        }
        Ok(Ok(_)) => {}
    }
    let tick = tick?;
    Ok(match tick {
        serde_json::Value::String(text) => text,
        value => value.to_string(),
    })
}

/// Release the monitor agent's own UI lease after its cron turn completes.
///
/// The preflight uses a distinct holder and releases it before the agent turn.
/// Models are instructed to release the subsequent `cron` lease, but cleanup
/// here keeps a missed tool call from suppressing the next one-minute sweep.
async fn release_wechat_monitor_agent_lock(
    plugins: Option<&Vec<rsclaw_plugin::WasmPlugin>>,
    plugin_name: &str,
) -> Result<()> {
    let plugins = plugins.context("wechat monitor lock cleanup has no WASM plugin registry")?;
    let plugin = plugins
        .iter()
        .find(|plugin| plugin.name == plugin_name)
        .with_context(|| format!("{plugin_name} monitor lock cleanup plugin is not loaded"))?;
    tokio::time::timeout(
        Duration::from_secs(10),
        plugin.call_tool("release_ui_lock", serde_json::json!({ "holder": "cron" })),
    )
    .await
    .map_err(|_| anyhow!("{plugin_name} monitor agent lock cleanup timed out"))??;
    Ok(())
}

fn tick_has_work(tick: &str) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(tick)
        .with_context(|| format!("invalid monitor_tick result: {tick}"))?;
    if value.get("skipped").is_some() {
        return Ok(false);
    }
    let active = value
        .get("activeChat")
        .and_then(|active| active.get("needsReply"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let changed = value
        .get("changedChats")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|chats| !chats.is_empty());
    let contact_badge = value
        .get("contactBadge")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let prepared_followup = value
        .get("preparedFollowup")
        .is_some_and(|followup| !followup.is_null());
    Ok(active || changed || contact_badge || prepared_followup)
}

#[cfg(test)]
mod monitor_preflight_tests {
    use super::tick_has_work;

    #[test]
    fn due_followup_wakes_monitor_agent() {
        let tick = r#"{"activeChat":{"needsReply":false},"changedChats":[],"contactBadge":false,"preparedFollowup":{"ticket":"followup-1","name":"客户"}}"#;
        assert!(tick_has_work(tick).expect("valid monitor result"));
    }

    #[test]
    fn empty_monitor_result_skips_agent() {
        let tick = r#"{"activeChat":{"needsReply":false},"changedChats":[],"contactBadge":false,"preparedFollowup":null}"#;
        assert!(!tick_has_work(tick).expect("valid monitor result"));
    }
}

async fn run_cron_job(
    job: &CronJob,
    agents: &AgentRegistry,
    daemon_agent_ids: &[String],
) -> Result<String> {
    let session_key = job
        .session_key
        .clone()
        .unwrap_or_else(|| format!("cron:{}:{}", job.id, chrono::Utc::now().timestamp_millis()));

    let handle = agents
        .get(&job.agent_id)
        .with_context(|| format!("agent not found: {}", job.agent_id))?;

    // Allow configurable timeout via payload.timeout_seconds, default 300s.
    // Daemon agents (long-lived monitor loops) get NO timeout — they loop
    // forever by design, so a cron-triggered (re)launch must not be killed at
    // 300s. Treated the same as `timeout_seconds: 0` below.
    let timeout_secs = if daemon_agent_ids.iter().any(|id| id == &job.agent_id) {
        0
    } else {
        job.payload
            .as_ref()
            .and_then(|p| match p {
                CronPayload::Structured {
                    timeout_seconds, ..
                } => *timeout_seconds,
                CronPayload::Text(_) => None,
            })
            .unwrap_or(300)
    };

    // Register abort flag for this session before dispatching
    let abort_flag = {
        let mut flags = handle
            .abort_flags
            .write()
            .expect("abort_flags lock poisoned");
        flags
            .entry(session_key.clone())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone()
    };

    // Slash-command short-circuit: if the cron-fired text starts with `/`
    // and is handled by fast preparse (e.g. /status, /loop, /cron list),
    // run it through the same path a user would hit when typing it in the
    // originating channel. Falls through to the agent inbox when preparse
    // returns None — anything not slash, or slash commands that need the
    // full LLM (e.g. /help text rendering at agent level), still reach
    // the agent loop unchanged.
    let job_text = job.effective_message();
    if job_text.starts_with('/') {
        let (preparse_channel, preparse_peer) = match job.delivery.as_ref() {
            Some(d) => (
                d.channel.as_deref().unwrap_or(""),
                d.to.as_ref().and_then(|t| t.head()).unwrap_or(""),
            ),
            None => ("", ""),
        };
        if let Some(reply) = crate::gateway::preparse::try_preparse_locally(
            job_text,
            handle.as_ref(),
            preparse_channel,
            preparse_peer,
            crate::gateway::preparse::PreparseOrigin::Cron,
        )
        .await
        {
            // Clear the abort flag (we never dispatched to the agent).
            abort_flag.store(false, std::sync::atomic::Ordering::SeqCst);
            // Empty reply text = preparse handled silently (e.g. /watch dedup-hit
            // triggered by /loop replay). Skip delivery — don't send blank chat
            // messages or fall through to the agent.
            if reply.text.is_empty() && reply.images.is_empty() {
                info!(job_id = %job.id, "cron job handled silently by preparse");
                return Ok(String::new());
            }
            info!(
                job_id = %job.id,
                len = reply.text.len(),
                "cron job handled by preparse short-circuit"
            );
            return Ok(reply.text);
        }
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text: job_text.to_owned(),
        channel: "cron".to_string(),
        peer_id: format!("cron:{}", job.id),
        chat_id: String::new(),
        reply_tx,
        task_id: None,
        context_id: None,
        event_tx: None,
        cancel_token: None,
        input_request_tx: None,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
        account: None,
    };

    handle.tx.send(msg).await.context("agent inbox closed")?;

    // `timeout_seconds: 0` means NO timeout — for long-lived daemon/monitor
    // turns that loop forever by design (paired with `agents.defaults.
    // daemon_agent_ids`). Otherwise enforce the (default 300s) timeout.
    let reply = if timeout_secs == 0 {
        reply_rx.await.context("agent dropped reply channel")?
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx)
            .await
            .map_err(|_| {
                // Timeout fired: abort the agent execution and capture status for error
                // reporting.
                abort_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                warn!(job_id = %job.id, session = %session_key, "cron: timeout fired, aborting agent");

                let agent_status = handle
                    .live_status
                    .try_read()
                    .map(|s| {
                        let task = if s.current_task.is_empty() {
                            "none".to_string()
                        } else {
                            s.current_task.chars().take(100).collect::<String>()
                        };
                        let tools = if s.tool_history.is_empty() {
                            "none".to_string()
                        } else {
                            s.tool_history.join(", ")
                        };
                        format!(
                            " (state: {}, task: \"{}\", tools called: [{}])",
                            s.state, task, tools
                        )
                    })
                    .unwrap_or_default();
                anyhow!("cron job timed out after {}s{}", timeout_secs, agent_status)
            })?
            .context("agent dropped reply channel")?
    };

    // Clear abort flag after successful completion
    abort_flag.store(false, std::sync::atomic::Ordering::SeqCst);

    if reply.is_empty {
        debug!(job_id = %job.id, "cron job returned no output");
        Ok(String::new())
    } else {
        // Check for exec tool failure in the reply.
        // The agent formats exec results as: "[stderr] ... [exit code: X]"
        // or returns JSON like: {"exit_code": 1, "stderr": "..."}
        let text = reply.text.clone();

        // Check formatted string pattern: [exit code: X] where X != 0
        if let Some(exit_match) = text.lines().rev().find(|line| line.contains("[exit code:")) {
            if let Some(code_str) = exit_match.split(':').nth(1) {
                if let Ok(code) = code_str.trim().replace(']', "").parse::<i64>() {
                    if code != 0 {
                        let error_detail = text
                            .lines()
                            .filter(|l| !l.contains("[exit code:"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let error_msg = if error_detail.is_empty() {
                            "command failed with no output".to_string()
                        } else {
                            error_detail
                        };
                        info!(job_id = %job.id, exit_code = code, "cron job exec failed");
                        return Err(anyhow!("command exit_code={}, error: {}", code, error_msg));
                    }
                }
            }
        }

        // Also check JSON format (fallback): {"exit_code": 1, ...}
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(exit_code) = json.get("exit_code").and_then(|v| v.as_i64()) {
                if exit_code != 0 {
                    let stderr = json.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
                    let stdout = json.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
                    let error_detail = if !stderr.is_empty() {
                        stderr
                    } else if !stdout.is_empty() {
                        stdout
                    } else {
                        "command failed with no output"
                    };
                    info!(job_id = %job.id, exit_code, "cron job exec failed");
                    return Err(anyhow!(
                        "command exit_code={}, error: {}",
                        exit_code,
                        error_detail
                    ));
                }
            }
        }

        info!(job_id = %job.id, len = reply.text.len(), "cron job completed");
        Ok(reply.text)
    }
}

/// Sentinel `delivery.to` value: fan out to every channel the agent is bound
/// to (`agents.<id>.channels`), sending to each channel's most-recent
/// conversation peer.
const TO_AGENT_CHANNELS: &str = "agent.channels";
/// Max concurrent deliveries per batch when fanning out to many recipients.
const DELIVERY_BATCH: usize = 10;

/// Resolve the peer of the agent's most-recent conversation, optionally
/// restricted to one channel and one account. Considers both the agent's own
/// DM/group sessions and a2a-delegated sessions (`…:a2a:<id>`, where another
/// agent handed work to this one — the peer is the human who started that
/// upstream conversation). Session-key shapes (see gateway/session.rs):
///   `agent:<id>:<channel>:direct|group:<peer>`            (per-channel-peer)
///   `agent:<id>:<channel>:<account>:direct|group:<peer>`  (per-account-…)
/// Returns `(channel, account_from_key, peer_or_group_id, is_group)`.
fn resolve_recent_session_target(
    agent_id: &str,
    channel: Option<&str>,
    account: Option<&str>,
) -> Option<(String, Option<String>, String, bool)> {
    let store = cron_store()?;
    let keys = store.list_sessions().ok()?;
    let own_prefix = format!("agent:{agent_id}:");
    let a2a_suffix = format!(":a2a:{agent_id}");
    let mut best: Option<(i64, String)> = None;
    for k in &keys {
        if !(k.starts_with(&own_prefix) || k.contains(&a2a_suffix)) {
            continue;
        }
        let toks: Vec<&str> = k.split(':').collect();
        let Some(marker) = toks.iter().position(|t| *t == "direct" || *t == "group") else {
            continue;
        };
        // channel is the 3rd segment; account (per-account scope) sits between
        // channel and the direct/group marker, i.e. present iff marker == 4.
        if let Some(want) = channel {
            if toks.get(2) != Some(&want) {
                continue;
            }
        }
        let key_account = if marker == 4 {
            toks.get(3).copied()
        } else {
            None
        };
        if let (Some(want), Some(have)) = (account, key_account) {
            if want != have {
                continue;
            }
        }
        let la = store
            .get_session_meta(k)
            .ok()
            .flatten()
            .map(|m| m.last_active)
            .unwrap_or(0);
        if best.as_ref().map_or(true, |(b, _)| la > *b) {
            best = Some((la, k.clone()));
        }
    }
    let (_, key) = best?;
    let toks: Vec<&str> = key.split(':').collect();
    let marker = toks.iter().position(|t| *t == "direct" || *t == "group")?;
    let ch = toks.get(2)?.to_string();
    let key_account = if marker == 4 {
        toks.get(3).map(|s| s.to_string())
    } else {
        None
    };
    let is_group = toks[marker] == "group";
    let peer = toks.get(marker + 1)?.to_string();
    Some((ch, key_account, peer, is_group))
}

/// Heuristic: does this target id look like a group/chat rather than a user?
fn target_is_group(id: &str) -> bool {
    id.starts_with("oc_") || id.ends_with("@chatroom")
}

/// Resolve a cron job's delivery config into concrete
/// `(channel, account, target, is_group)` recipients. Shared by the scheduled
/// path (`send_delivery`) and the manual-trigger path (`cron_trigger`) so both
/// honor `to` lists, the `agent.channels` sentinel, and the no-`to` "reply to
/// the agent's most-recent conversation" fallback identically. The account
/// (which bot/app sends) keeps each channel-specific open_id valid.
pub(crate) fn resolve_delivery_targets(
    agents: &AgentRegistry,
    agent_id: &str,
    delivery: &CronDelivery,
) -> Vec<(String, Option<String>, String, bool)> {
    let default_account = delivery.account_id.clone();
    let to_list: Vec<String> = delivery
        .to
        .as_ref()
        .map(|t| t.to_chain())
        .unwrap_or_default();
    let mut targets: Vec<(String, Option<String>, String, bool)> = Vec::new();

    if to_list.iter().any(|s| s == TO_AGENT_CHANNELS) {
        // Fan out to every channel:account the agent is bound to. Each binding
        // entry is `<channel>` or `<channel>:<account>` (see route_account).
        let bound: Vec<String> = agents
            .get(agent_id)
            .ok()
            .and_then(|h| h.config.channels.clone())
            .filter(|v| !v.is_empty())
            .or_else(|| delivery.channel.clone().map(|c| vec![c]))
            .unwrap_or_default();
        for entry in &bound {
            let (ch, acct) = match entry.split_once(':') {
                Some((c, a)) => (c, Some(a)),
                None => (entry.as_str(), None),
            };
            match resolve_recent_session_target(agent_id, Some(ch), acct) {
                Some((rch, key_acct, peer, is_group)) => {
                    let send_acct = acct
                        .map(|s| s.to_string())
                        .or(key_acct)
                        .or_else(|| default_account.clone());
                    targets.push((rch, send_acct, peer, is_group));
                }
                None => {
                    warn!(agent = %agent_id, binding = %entry, "cron: agent.channels — no recent conversation for binding, skipping")
                }
            }
        }
        // Explicit ids listed alongside the sentinel still go through.
        if let Some(ch) = &delivery.channel {
            for to in to_list.iter().filter(|s| *s != TO_AGENT_CHANNELS) {
                targets.push((
                    ch.clone(),
                    default_account.clone(),
                    to.clone(),
                    target_is_group(to),
                ));
            }
        }
    } else if !to_list.is_empty() {
        // Explicit recipient(s) on the configured channel + account.
        let Some(ch) = delivery.channel.clone() else {
            warn!(agent = %agent_id, "cron: delivery channel not specified");
            return targets;
        };
        for to in to_list {
            let is_group = target_is_group(&to);
            targets.push((ch.clone(), default_account.clone(), to, is_group));
        }
    } else {
        // No `to`: reply to the agent's most-recent conversation peer
        // (including a2a-delegated conversations), via that session's account.
        match resolve_recent_session_target(agent_id, None, None) {
            Some((rch, key_acct, peer, is_group)) => {
                let send_acct = key_acct.or_else(|| default_account.clone());
                targets.push((rch, send_acct, peer, is_group));
            }
            None => {
                info!(agent = %agent_id, "cron: no `to` set and no recent conversation found; discarding result")
            }
        }
    }
    targets
}

async fn send_delivery(
    channels: &ChannelManager,
    agents: &AgentRegistry,
    job: &CronJob,
    default_delivery: &Option<CronDelivery>,
    output_text: &str,
) -> Result<()> {
    let delivery = match job.delivery.as_ref().or(default_delivery.as_ref()) {
        Some(d) => d,
        None => {
            info!(job_id = %job.id, name = ?job.name, "cron: no delivery configured, result discarded. Set delivery on the job or configure default_delivery in cron config.");
            return Ok(());
        }
    };

    let mode = delivery.mode.as_deref().unwrap_or("none");
    if mode == "none" {
        debug!(job_id = %job.id, "cron: delivery mode is 'none', skipping");
        return Ok(());
    }

    let text = output_text.trim();
    if text.is_empty() && default_delivery.is_none() && job.delivery.is_none() {
        debug!(job_id = %job.id, "cron: output text is empty and no delivery configured");
        return Ok(());
    }

    let thread = delivery.thread_id.clone();
    let best_effort = delivery.best_effort.unwrap_or(false);

    // Resolve recipients (channel, account, target, is_group). Shared with the
    // manual-trigger path so both honor `to` lists, the `agent.channels`
    // sentinel, and the no-`to` "reply to recent conversation" fallback.
    let targets = resolve_delivery_targets(agents, &job.agent_id, delivery);
    if targets.is_empty() {
        return Ok(());
    }

    info!(job_id = %job.id, recipients = targets.len(), text_len = text.len(), "cron: sending delivery");

    // Send in concurrent batches of DELIVERY_BATCH.
    for chunk in targets.chunks(DELIVERY_BATCH) {
        let futs = chunk.iter().map(|(channel_name, account, to, is_group)| {
            // Back-compat: legacy jobs carry channel="ws"; the desktop
            // broadcaster is registered under "desktop".
            let resolved_channel = if channel_name == "ws" {
                "desktop".to_string()
            } else {
                channel_name.clone()
            };
            let ch = channels.get(&resolved_channel);
            let msg = OutboundMessage {
                target_id: to.clone(),
                is_group: *is_group,
                text: text.to_owned(),
                reply_to: thread.clone(),
                images: vec![],
                files: vec![],
                channel: Some(resolved_channel.clone()),
                account: account.clone(),
            };
            let job_id = job.id.clone();
            let to_log = to.clone();
            async move {
                match ch {
                    Some(c) => match c.send(msg).await {
                        Ok(()) => {
                            info!(job_id = %job_id, channel = %resolved_channel, to = %to_log, "cron delivery sent successfully");
                            Ok(())
                        }
                        Err(e) => Err(e),
                    },
                    None => {
                        warn!(job_id = %job_id, channel = %resolved_channel, "cron: channel not found in ChannelManager");
                        Ok(())
                    }
                }
            }
        });
        let results = futures::future::join_all(futs).await;
        for r in results {
            if let Err(e) = r {
                if best_effort {
                    warn!(job_id = %job.id, error = %e, "cron delivery failed (best_effort)");
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Execute a command directly without agent, returning real output.
/// Used for execCommand payload type to bypass session history pollution.
/// Uses background execution pattern to avoid blocking the spawned task.
/// If summarize=true, sends output to agent for summarization.
async fn run_exec_command(
    command: &str,
    timeout_secs: Option<u64>,
    summarize: bool,
    job: &CronJob,
    agents: &AgentRegistry,
) -> Result<String> {
    let exec_timeout = Duration::from_secs(timeout_secs.unwrap_or(120));
    let task_id = format!("cron:{}:{}", job.id, chrono::Utc::now().timestamp_millis());

    // Determine shell based on platform
    let (shell, shell_args) = if cfg!(target_os = "windows") {
        ("powershell", vec!["-NoProfile", "-Command"])
    } else {
        ("sh", vec!["-c"])
    };

    // Build command
    let mut cmd = tokio::process::Command::new(shell);
    cmd.args(&shell_args)
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    // Use oneshot channel to receive result from background task
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();

    tracing::info!(task_id = %task_id, command = %command, "cron exec: spawning background task");

    let tid = task_id.clone();
    let cmd_timeout = exec_timeout;
    tokio::spawn(async move {
        let started_at = std::time::Instant::now();
        let result = tokio::time::timeout(cmd_timeout, cmd.output()).await;

        let (exit_code, stdout, stderr) = match result {
            Ok(Ok(output)) => {
                let exit_code = output.status.code();
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (exit_code, stdout, stderr)
            }
            Ok(Err(e)) => {
                tracing::error!(task_id = %tid, "cron exec background spawn failed: {}", e);
                (None, String::new(), format!("spawn error: {}", e))
            }
            Err(_) => {
                tracing::warn!(task_id = %tid, timeout_secs = cmd_timeout.as_secs(), "cron exec background timed out");
                (
                    None,
                    String::new(),
                    format!("timed out after {} seconds", cmd_timeout.as_secs()),
                )
            }
        };

        let completed_at = std::time::Instant::now();
        tracing::info!(
            task_id = %tid,
            exit_code = ?exit_code,
            stdout_len = stdout.len(),
            stderr_len = stderr.len(),
            elapsed_ms = (completed_at - started_at).as_millis(),
            "cron exec background completed"
        );

        // Send result back via oneshot channel
        let _ = result_tx.send((exit_code, stdout, stderr));
    });

    // Wait for background task result (non-blocking for spawned task, but waits
    // here)
    let (exit_code, stdout, stderr) = result_rx
        .await
        .map_err(|_| anyhow!("background exec channel closed"))?;

    let exit_code = exit_code.unwrap_or(-1);

    if exit_code != 0 {
        // Return error with details
        let error_msg = if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            "command failed with no output".to_string()
        };
        return Err(anyhow!(
            "command exit_code={}, error: {}",
            exit_code,
            error_msg
        ));
    }

    // Get raw output
    let raw_output = if !stdout.is_empty() {
        stdout
    } else if !stderr.is_empty() {
        stderr
    } else {
        "command succeeded with no output".to_string()
    };

    // Detect saved file paths in output and read their content.
    // Common patterns: "report saved: xxx", "saved to: xxx", "file saved: xxx"
    // (and the Chinese equivalents — see extract_saved_files_content).
    let saved_files_content = extract_saved_files_content(&raw_output);
    let full_output = if saved_files_content.is_empty() {
        raw_output.clone()
    } else {
        format!(
            "{}\n\n---\n\n[FULL CONTENT OF SAVED REPORT FILES]\n{}\n\n[NOTE] The above is the full report the script saved. Base your summary on this content; don't omit key information.",
            raw_output, saved_files_content
        )
    };

    // If summarize=true, send output to agent for summarization
    if summarize {
        // Try to use a dedicated summarizer agent first to avoid queue conflicts
        // with the main agent. Falls back to job.agent_id if not available.
        let summarize_agent_id = if agents.get("_summarizer").is_ok() {
            "_summarizer"
        } else {
            &job.agent_id
        };

        let session_key = job
            .session_key
            .clone()
            .unwrap_or_else(|| format!("cron:{}", job.id));

        let handle = agents
            .get(summarize_agent_id)
            .with_context(|| format!("agent not found: {}", summarize_agent_id))?;

        // Create summarize prompt with real output. Strict anti-fabrication
        // rules so LLM only summarizes what's actually in raw_output (and
        // any saved report file pulled in below by the include-saved-file
        // logic — that lands in `full_output`).
        let summarize_prompt = format!(
            "[CRON TASK EXECUTION RESULT — NO FABRICATION]\n\
            Below is the real output of a script execution.\n\
            \n\
            [HARD RULES — MUST FOLLOW]\n\
            1. You may ONLY summarize information that is already in the output below; do not add anything not present.\n\
            2. If the output contains a \"FULL CONTENT OF SAVED REPORT FILES\" section, base your summary on that full content and do not omit key information.\n\
            3. If the output has no concrete data (e.g. stock counts, prices), do not invent numbers.\n\
            4. If the output is empty or only contains errors, honestly report \"script execution failed\" or \"no output\".\n\
            5. Do not claim actions like \"done\", \"found\", \"executed\" — you only summarize, you did not execute anything.\n\
            6. Return the summary text directly; do not return HEARTBEAT_OK.\n\
            \n\
            [OUTPUT]\n\
            ```\n{}\n\
            ```\n\
            \n\
            Summarize strictly per the rules above. Violating any rule counts as deception.",
            full_output
        );

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = AgentMessage {
            // `summarize:` prefix is detected by the agent runtime and disables
            // ALL tools for the turn — forces the LLM to return a text summary
            // instead of calling memory.put / write_file / etc. Without this,
            // the agent treats summarize requests as normal turns and often
            // chooses tool calls over plain text.
            session_key: format!("summarize:{}", session_key),
            text: summarize_prompt,
            channel: "cron".to_string(),
            peer_id: format!("cron:{}", job.id),
            chat_id: String::new(),
            reply_tx,
            task_id: None,
            context_id: None,
            event_tx: None,
            cancel_token: None,
            input_request_tx: None,
            extra_tools: vec![], // No tools - only summarize
            images: vec![],
            files: vec![],
            account: None,
        };

        handle.tx.send(msg).await.context("agent inbox closed")?;

        // Wait for summary with timeout. 300s gives the (possibly busy)
        // agent room to process other tasks first; cron jobs tend to be
        // batch-style so a longer wait is acceptable. Note that the new
        // _summarizer agent path above is the real fix for queue
        // contention — the timeout is just a safety net.
        let summary_timeout = Duration::from_secs(300);
        match tokio::time::timeout(summary_timeout, reply_rx).await {
            Ok(Ok(reply)) => {
                if reply.is_empty {
                    // Agent returned nothing, use raw output
                    Ok(raw_output)
                } else {
                    Ok(reply.text)
                }
            }
            Ok(Err(_)) => {
                // Agent dropped reply channel, fallback to raw output
                tracing::warn!(job_id = %job.id, "summarize: agent dropped reply, using raw output");
                Ok(raw_output)
            }
            Err(_) => {
                // Timeout - agent is busy, fallback to raw output
                tracing::warn!(job_id = %job.id, timeout_secs = summary_timeout.as_secs(), "summarize: timed out, using raw output");
                Ok(raw_output)
            }
        }
    } else {
        // summarize=false: return full report if file was saved
        if saved_files_content.is_empty() {
            Ok(raw_output)
        } else {
            Ok(saved_files_content)
        }
    }
}

async fn write_run_log(log_dir: &std::path::Path, job_id: &str, entry: RunLogEntry) -> Result<()> {
    let path = log_dir.join(format!("{job_id}.jsonl"));
    let line = serde_json::to_string(&entry)? + "\n";
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(line.as_bytes()).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Legacy file reader (used by gateway startup for initial job loading)
// ---------------------------------------------------------------------------

pub async fn read_jobs_from_file(cron_dir: PathBuf) -> Result<Vec<CronJob>> {
    let jobs_path = cron_dir.join("jobs.json");
    let data = tokio::fs::read_to_string(&jobs_path)
        .await
        .unwrap_or_else(|_| "[]".to_owned());

    let wrapper: serde_json::Value =
        serde_json::from_str(&data).unwrap_or_else(|_| serde_json::Value::Array(vec![]));

    let jobs_array = if let Some(arr) = wrapper.get("jobs").and_then(|v| v.as_array()) {
        arr.clone()
    } else if wrapper.is_array() {
        wrapper.as_array().cloned().unwrap_or_default()
    } else {
        vec![]
    };

    let mut jobs: Vec<CronJob> = jobs_array
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();

    jobs.sort_by_key(|j| j.created_at_ms.unwrap_or(0));

    Ok(jobs)
}
