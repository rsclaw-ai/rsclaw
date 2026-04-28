//! Cron job scheduler — runs periodic agent tasks (AGENTS.md §16).
//!
//! Uses a self-implemented timer loop (tokio::time::sleep) instead of
//! tokio-cron-scheduler, for reliable cross-platform behavior.
//!
//! Schedule format: standard 5-field cron "min hr dom mon dow".
//! Timezone: stored in schedule but currently executes in UTC.
//!
//! Each job runs in an isolated session (`cron:<jobId>`) or a persistent
//! session (`session:<key>`). Concurrent runs are capped by
//! `max_concurrent_runs`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Semaphore};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tracing::{debug, info, trace, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{ChannelManager, OutboundMessage},
    config::schema::{CronConfig, CronDelivery, CronJobConfig},
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum time between timer ticks (ms). Prevents schedule drift.
const MAX_TIMER_DELAY_MS: u64 = 60_000;

/// Minimum gap between re-triggering the same job (ms). Prevents spin-loops.
const MIN_REFIRE_GAP_MS: u64 = 2_000;

/// Max consecutive errors before a job is silently skipped (won't block scheduler).
const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// After this many ms without completing, a running job is considered stale.
const STUCK_RUN_MS: u64 = 2 * 60 * 60 * 1000; // 2 hours

/// Exponential backoff delays (ms) indexed by consecutive error count.
/// After the last entry the delay stays constant.
const ERROR_BACKOFF_MS: [u64; 5] = [
    30_000,    // 1st error  →  30 seconds
    60_000,    // 2nd error  →  1 minute
    300_000,   // 3rd error  →  5 minutes
    900_000,   // 4th error  →  15 minutes
    3_600_000, // 5th+ error →  60 minutes
];

/// Sentinel error message produced when a running job is cancelled because
/// reload detected the job was deleted, disabled, or its config changed.
/// Used to distinguish reload-driven cancellation from actual failures so
/// `consecutive_errors` is not bumped and the new job version starts clean.
const CANCEL_BY_RELOAD: &str = "cron: cancelled by reload";

/// Get backoff delay for consecutive error count.
fn error_backoff_ms(consecutive_errors: u32) -> u64 {
    let idx = (consecutive_errors.saturating_sub(1) as usize).min(ERROR_BACKOFF_MS.len() - 1);
    ERROR_BACKOFF_MS[idx]
}

// ---------------------------------------------------------------------------
// CronJob — serialisable description of a single scheduled task
// ---------------------------------------------------------------------------

/// Schedule descriptor — supports both rsclaw flat format and OpenClaw nested format.
///
/// Uses `#[serde(untagged)]` at the top level to distinguish a plain string (Flat)
/// from an object. Object variants use `#[serde(tag = "kind")]` (internally tagged)
/// so that `{"kind": "once", "atMs": ...}` is not accidentally matched by `Every`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronSchedule {
    /// Flat string: "*/30 9-11 * * 1-5" (rsclaw native).
    Flat(String),
    /// Object-based schedule with a "kind" discriminator.
    Tagged(CronScheduleTagged),
}

/// Internally tagged schedule object. Discriminated by the `kind` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum CronScheduleTagged {
    /// Cron expression: { kind: "cron", expr: "...", tz: "Asia/Shanghai" } (OpenClaw compat).
    #[serde(rename = "cron")]
    Nested {
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    /// Interval-based schedule: { kind: "every", everyMs: 259200000, anchorMs: ... } (OpenClaw compat).
    #[serde(rename = "every")]
    Every {
        #[serde(default, alias = "everyMs")]
        every_ms: Option<u64>,
        #[serde(default, alias = "anchorMs")]
        anchor_ms: Option<u64>,
    },
    /// One-shot schedule: fires once then auto-removes.
    /// { kind: "once", atMs: 1713600000000 } — absolute timestamp
    /// { kind: "once", delayMs: 1200000 }   — relative delay from creation
    #[serde(rename = "once")]
    Once {
        #[serde(default, alias = "atMs")]
        at_ms: Option<u64>,
        #[serde(default, alias = "delayMs")]
        delay_ms: Option<u64>,
    },
}

impl CronSchedule {
    pub fn expr(&self) -> &str {
        match self {
            CronSchedule::Flat(s) => s,
            CronSchedule::Tagged(CronScheduleTagged::Nested { expr, .. }) => expr,
            CronSchedule::Tagged(CronScheduleTagged::Every { .. }) => "every",
            CronSchedule::Tagged(CronScheduleTagged::Once { .. }) => "once",
        }
    }

    pub fn tz(&self) -> Option<&str> {
        match self {
            CronSchedule::Flat(_) => None,
            CronSchedule::Tagged(CronScheduleTagged::Nested { tz, .. }) => tz.as_deref(),
            CronSchedule::Tagged(CronScheduleTagged::Every { .. }) => None,
            CronSchedule::Tagged(CronScheduleTagged::Once { .. }) => None,
        }
    }

    /// Whether this is a one-shot schedule (auto-remove after execution).
    pub fn is_once(&self) -> bool {
        matches!(self, CronSchedule::Tagged(CronScheduleTagged::Once { .. }))
    }

    /// Compute the next run timestamp (ms) from the given `from_ms`.
    /// For cron schedules: searches forward up to 1 year.
    /// For interval schedules (every): uses anchor + n*everyMs.
    pub fn compute_next_run(&self, from_ms: u64) -> Option<u64> {
        match self {
            CronSchedule::Flat(expr) => {
                crate::cron::compute_next_run_from_expr(expr, from_ms, None)
            }
            CronSchedule::Tagged(CronScheduleTagged::Nested { expr, tz, .. }) => {
                crate::cron::compute_next_run_from_expr(expr, from_ms, tz.as_deref())
            }
            CronSchedule::Tagged(CronScheduleTagged::Every { every_ms, anchor_ms }) => {
                let every_ms = every_ms.unwrap_or(0);
                if every_ms == 0 {
                    return None;
                }
                let anchor = anchor_ms.unwrap_or(from_ms);
                // Find smallest n where anchor + n * every_ms > from_ms
                if anchor > from_ms {
                    Some(anchor)
                } else {
                    let elapsed = from_ms - anchor;
                    let n = (elapsed / every_ms) + 1;
                    Some(anchor + n * every_ms)
                }
            }
            CronSchedule::Tagged(CronScheduleTagged::Once { at_ms, delay_ms }) => {
                // Absolute timestamp takes priority over delay.
                if let Some(at) = at_ms {
                    if *at > from_ms { Some(*at) } else { None }
                } else if let Some(delay) = delay_ms {
                    // delay_ms is relative to creation, but compute_next_run
                    // is always called with current time.  The actual fire time
                    // is set in tool_cron when creating the job (createdAtMs + delayMs),
                    // stored as at_ms.  If we reach here, treat from_ms + delay as fallback.
                    let target = from_ms + delay;
                    if target > from_ms { Some(target) } else { None }
                } else {
                    None
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronPayload {
    /// Plain text message.
    Text(String),
    /// Structured payload (OpenClaw compat): { kind: "agentTurn", message: "...", timeoutSeconds: 1800 }
    Structured {
        #[serde(default, alias = "kind")]
        kind: Option<String>,
        /// Message text - serializes as "message" for openclaw compat, accepts "text" too
        #[serde(alias = "text", rename = "message", default)]
        text: Option<String>,
        #[serde(default, alias = "timeoutSeconds")]
        timeout_seconds: Option<u64>,
        /// For execCommand: if true, send output to agent for summarization.
        #[serde(default)]
        summarize: Option<bool>,
    },
}

impl CronPayload {
    pub fn text(&self) -> &str {
        match self {
            CronPayload::Text(s) => s,
            CronPayload::Structured { text, .. } => text.as_deref().unwrap_or(""),
        }
    }

    pub fn summarize(&self) -> bool {
        match self {
            CronPayload::Text(_) => false,
            CronPayload::Structured { summarize, .. } => summarize.unwrap_or(false),
        }
    }
}

/// Persistent run state (OpenClaw compat).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_status: Option<String>,
    #[serde(default)]
    pub consecutive_errors: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub enabled: bool,
    pub schedule: CronSchedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CronJobState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_ms: Option<u64>,
}

impl CronJob {
    pub fn effective_message(&self) -> &str {
        if let Some(ref payload) = self.payload {
            return payload.text();
        }
        self.message.as_deref().unwrap_or("")
    }

    pub fn cron_expr(&self) -> &str {
        self.schedule.expr()
    }

    pub fn timezone(&self) -> Option<&str> {
        self.schedule.tz()
    }
}

impl From<&CronJobConfig> for CronJob {
    fn from(cfg: &CronJobConfig) -> Self {
        let session_key = cfg.session.as_ref().and_then(|v| {
            if let serde_json::Value::String(s) = v {
                Some(s.clone())
            } else {
                None
            }
        });
        let schedule = if let Some(ref tz) = cfg.tz {
            CronSchedule::Tagged(CronScheduleTagged::Nested {
                expr: cfg.schedule.clone(),
                tz: Some(tz.clone()),
            })
        } else {
            CronSchedule::Flat(cfg.schedule.clone())
        };
        Self {
            id: cfg.id.clone(),
            name: cfg.name.clone(),
            agent_id: cfg.agent_id.clone().unwrap_or_else(|| "default".to_string()),
            session_key,
            enabled: cfg.enabled.unwrap_or(true),
            schedule,
            payload: None,
            message: Some(cfg.message.clone()),
            delivery: cfg.delivery.clone(),
            session_target: None,
            wake_mode: None,
            state: None,
            created_at_ms: None,
            updated_at_ms: None,
        }
    }
}

// ---------------------------------------------------------------------------
// CronStore — persisted state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CronStore {
    version: u32,
    jobs: Vec<CronJob>,
}

impl Default for CronStore {
    fn default() -> Self {
        Self { version: 1, jobs: Vec::new() }
    }
}

// ---------------------------------------------------------------------------
// RunLogEntry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogEntry {
    pub id: String,
    pub job_id: String,
    pub started_at: chrono::DateTime<Utc>,
    pub finished_at: Option<chrono::DateTime<Utc>>,
    pub success: bool,
    pub reply_preview: Option<String>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// CronRunner
// ---------------------------------------------------------------------------

pub struct CronRunner {
    jobs: Vec<CronJob>,
    agents: Arc<AgentRegistry>,
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
            config, jobs, agents, channels, data_dir, reload_tx, ws_conns, None,
        )
    }

    /// Construct a new cron runner with an explicit shutdown coordinator.
    /// The runtime uses this constructor; tests typically use [`new`].
    pub fn new_with_shutdown(
        config: &CronConfig,
        jobs: Vec<CronJob>,
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
            channels,
            run_log_dir,
            store_path,
            semaphore: Arc::new(Semaphore::new(4)),
            default_delivery: config.default_delivery.clone(),
            reload_tx,
            ws_conns,
            shutdown,
        }
    }

    pub fn jobs(&self) -> &[CronJob] {
        &self.jobs
    }

    /// Start all enabled cron jobs and block until Ctrl-C.
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

        // Persist initial state
        if let Err(e) = self.save_store(&jobs).await {
            warn!(err = %e, "cron: failed to save initial store");
        }

        let enabled_count = jobs.iter().filter(|j| j.enabled).count();
        info!(
            total = jobs.len(),
            enabled = enabled_count,
            next_wake = jobs.iter().filter_map(|j| j.state.as_ref().and_then(|s| s.next_run_at_ms)).min().unwrap_or(0),
            "cron scheduler started"
        );

        // Main timer loop
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let running_clone = Arc::clone(&running);
        let semaphore = Arc::clone(&self.semaphore);
        let reload_rx = self.reload_tx.subscribe();

        let runner = self.clone();
        let timer_handle = tokio::spawn(async move {
            runner.timer_loop(jobs, running_clone, semaphore, reload_rx).await;
        });

        tokio::signal::ctrl_c().await?;
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
        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<(String, bool, u64, u64, Option<String>)>(64);

        // Cancel flags for running jobs — set to true to signal abort on deletion.
        let mut cancel_flags: HashMap<String, Arc<std::sync::atomic::AtomicBool>> = HashMap::new();

        // Clear orphaned running_at_ms states from previous app run.
        // When the app restarts, any jobs that were running at shutdown will have
        // running_at_ms set but no actual spawned task, causing them to be stuck.
        let orphan_count = jobs.iter_mut()
            .filter(|j| j.state.as_ref().and_then(|s| s.running_at_ms).is_some())
            .count();
        if orphan_count > 0 {
            warn!(count = orphan_count, "cron: clearing orphaned running_at_ms states from previous run");
            for job in jobs.iter_mut() {
                if let Some(state) = job.state.as_mut() {
                    if state.running_at_ms.is_some() {
                        info!(job_id = %job.id, "cron: clearing orphaned running_at_ms");
                        state.running_at_ms = None;
                        // Recompute next_run_at_ms if needed
                        if state.next_run_at_ms.is_none() || state.next_run_at_ms.map(|t| t <= current_timestamp_ms()).unwrap_or(true) {
                            state.next_run_at_ms = job.schedule.compute_next_run(current_timestamp_ms());
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
                    j.state.as_ref().and_then(|s| s.next_run_at_ms).map(|t| (t, &j.id, &j.name))
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

            debug!(next_wake = next_wake.unwrap_or(0), now_ms, "cron: timer tick");

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
            let reload_triggered = tokio::select! {
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

            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            if reload_triggered {
                // Reload jobs from file
                let old_count = jobs.len();
                let new_jobs = crate::cron::load_cron_jobs();
                let file_count = new_jobs.len();

                // Debug: check if disabled job is in new_jobs
                let disabled_in_file: Vec<_> = new_jobs.iter()
                    .filter(|j| !j.enabled)
                    .map(|j| (&j.id, j.enabled))
                    .collect();
                info!(old_count, new_count = new_jobs.len(), file_count, disabled=?disabled_in_file, "cron: reload triggered, reloading from file");

                let (merged_jobs, modified_ids) = self.merge_jobs(&jobs, new_jobs, now_ms);
                jobs = merged_jobs;

                // Debug: check enabled state after merge
                let disabled_after_merge: Vec<_> = jobs.iter()
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
                let active_unchanged: HashSet<&str> = jobs.iter()
                    .filter(|j| j.enabled && !modified_ids.contains(&j.id))
                    .map(|j| j.id.as_str())
                    .collect();
                let to_cancel: Vec<String> = cancel_flags.keys()
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
                info!(old_count, new_count = jobs.len(), file_count, "cron jobs reloaded");
                continue;
            }

            // Collect any completed job results FIRST, before checking due.
            // This is critical: if we skip try_recv when due.is_empty(), runningAtMs
            // will never be cleared, causing the job to be stuck forever.
            let mut collected_count = 0;
            while let Ok((job_id, success, duration_ms, started_at, error_msg)) = result_rx.try_recv() {
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
                            state.next_run_at_ms = job.schedule.compute_next_run(completion_time);
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

                            // Apply exponential backoff for errored jobs
                            let backoff = error_backoff_ms(state.consecutive_errors);
                            let backoff_next = completion_time + backoff;
                            let normal_next = job.schedule.compute_next_run(completion_time);
                            // Use whichever is later: the natural next run or the backoff delay
                            state.next_run_at_ms = Some(normal_next.map(|n| n.max(backoff_next)).unwrap_or(backoff_next));

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
                        && j.state
                            .as_ref()
                            .and_then(|s| s.running_at_ms)
                            .is_none()
                })
                .map(|j| j.id.clone())
                .collect();

            // Debug: log enabled state of all jobs that are due but shouldn't fire
            if !due.is_empty() {
                let disabled_due: Vec<_> = jobs.iter()
                    .filter(|j| !j.enabled && j.state.as_ref().and_then(|s| s.next_run_at_ms).map(|t| t <= now_ms).unwrap_or(false))
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
                        info!("cron scheduler: drain signaled during permit await, dropping job {}", job_id);
                        drop(permit);
                        break;
                    }
                }

                let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) else {
                    continue;
                };

                // Mark as running
                let started_at = current_timestamp_ms();
                if let Some(state) = job.state.as_mut() {
                    state.running_at_ms = Some(started_at);
                    // Don't compute next_run_at_ms here; compute it AFTER the job finishes
                    // using the completion time, so interval-based jobs don't fire early
                }

                let permit = permit.expect("permit checked above");
                let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                cancel_flags.insert(job.id.clone(), Arc::clone(&cancelled));
                let job_id_for_log = job.id.clone();  // Clone BEFORE async move
                let job = job.clone();
                let agents = Arc::clone(&self.agents);
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
                    let prev_consecutive_errors = job.state.as_ref().map(|s| s.consecutive_errors).unwrap_or(0);
                    info!(job_id = %job.id, "cron job triggered");

                    // systemEvent: deliver payload text directly — no agent call needed.
                    // execCommand: execute the command directly, bypassing agent and session history.
                    let result: Result<String> = if job.payload.as_ref().and_then(|p| match p {
                        CronPayload::Structured { kind, .. } => kind.as_deref(),
                        _ => None,
                    }) == Some("systemEvent") {
                        Ok(job.effective_message().to_owned())
                    } else if job.payload.as_ref().and_then(|p| match p {
                        CronPayload::Structured { kind, .. } => kind.as_deref(),
                        _ => None,
                    }) == Some("execCommand") {
                        // Execute command directly, bypassing agent to avoid session history pollution
                        run_exec_command(
                            job.effective_message(),
                            job.payload.as_ref().and_then(|p| match p {
                                CronPayload::Structured { timeout_seconds, .. } => *timeout_seconds,
                                _ => None,
                            }),
                            job.payload.as_ref().map(|p| p.summarize()).unwrap_or(false),
                            &job,
                            &agents,
                        ).await
                    } else {
                        // Run with cancellation check — polls cancel flag every second.
                        tokio::select! {
                            r = run_cron_job(&job, &agents) => r,
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
                            Some(crate::i18n::t_fmt(
                                "cron_run_success",
                                crate::i18n::default_lang(),
                                &[("name", job_name), ("seconds", &seconds)],
                            ))
                        }
                        Err(e) if e.to_string() == CANCEL_BY_RELOAD => {
                            // Reload cancelled this run.  Skip delivery so the
                            // user isn't spammed when they edit a job.
                            None
                        }
                        Err(e) => {
                            // Error - send error notification with consecutive failure count and backoff
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
                                crate::i18n::t_fmt(
                                    "cron_run_failed_disabled",
                                    crate::i18n::default_lang(),
                                    &[
                                        ("name", job_name),
                                        ("consecutive", &consecutive_str),
                                        ("error", &error_str),
                                    ],
                                )
                            } else {
                                crate::i18n::t_fmt(
                                    "cron_run_failed_retry",
                                    crate::i18n::default_lang(),
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
                        let delivery_job = job.clone();
                        let delivery_default = default_delivery.clone();
                        tokio::spawn(async move {
                            if let Err(e) = send_delivery(
                                &delivery_channels,
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
                    (job.id, result.is_ok(), duration_ms, job_started_at, error_msg)
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
                info!(removed = before - jobs.len(), "cron: cleaned up completed one-shot jobs");
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
        let prev_consecutive_errors = job.state.as_ref().map(|s| s.consecutive_errors).unwrap_or(0);
        // systemEvent: deliver payload text directly — no agent call needed.
        // execCommand: execute the command directly, bypassing agent and session history.
        let result: Result<String> = if job.payload.as_ref().and_then(|p| match p {
            CronPayload::Structured { kind, .. } => kind.as_deref(),
            _ => None,
        }) == Some("systemEvent") {
            Ok(job.effective_message().to_owned())
        } else if job.payload.as_ref().and_then(|p| match p {
            CronPayload::Structured { kind, .. } => kind.as_deref(),
            _ => None,
        }) == Some("execCommand") {
            run_exec_command(
                job.effective_message(),
                job.payload.as_ref().and_then(|p| match p {
                    CronPayload::Structured { timeout_seconds, .. } => *timeout_seconds,
                    _ => None,
                }),
                job.payload.as_ref().map(|p| p.summarize()).unwrap_or(false),
                job,
                &self.agents,
            ).await
        } else {
            run_cron_job(job, &self.agents).await
        };
        let success = result.is_ok();

        // Build delivery message with execution summary
        let delivery_text = match &result {
            Ok(output) if !output.trim().is_empty() => output.clone(),
            Ok(_) => {
                let job_name = job.name.as_deref().unwrap_or(&job.id);
                crate::i18n::t_fmt(
                    "cron_run_success_no_duration",
                    crate::i18n::default_lang(),
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
                crate::i18n::t_fmt(
                    "cron_run_failed_manual",
                    crate::i18n::default_lang(),
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
        if let Err(e) =
            send_delivery(&self.channels, job, &self.default_delivery, &delivery_text).await
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

    async fn save_store(&self, jobs: &[CronJob]) -> Result<()> {
        let store = CronStore {
            version: 1,
            jobs: jobs.to_vec(),
        };
        let json = serde_json::to_string_pretty(&store)?;
        let tmp = format!("{}.tmp", self.store_path.display());
        tokio::fs::write(&tmp, &json).await?;
        tokio::fs::rename(&tmp, &self.store_path).await?;
        Ok(())
    }
}

impl Clone for CronRunner {
    fn clone(&self) -> Self {
        Self {
            jobs: self.jobs.clone(),
            agents: Arc::clone(&self.agents),
            channels: Arc::clone(&self.channels),
            run_log_dir: self.run_log_dir.clone(),
            store_path: self.store_path.clone(),
            semaphore: Arc::clone(&self.semaphore),
            default_delivery: self.default_delivery.clone(),
            reload_tx: self.reload_tx.clone(),
            ws_conns: Arc::clone(&self.ws_conns),
            shutdown: self.shutdown.clone(),
        }
    }
}

/// True when two CronJobs have identical user-facing configuration.
/// Compared via serde_json::Value so we don't have to derive PartialEq across
/// every nested type.  Strips fields that should NOT count as a meaningful
/// change:
///   - `state`: runtime-only execution state.
///   - `createdAtMs` / `updatedAtMs`: audit timestamps that don't affect
///     execution semantics (and `updatedAtMs` flips on every save).
fn cron_jobs_config_equal(a: &CronJob, b: &CronJob) -> bool {
    let mut a_v = match serde_json::to_value(a) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut b_v = match serde_json::to_value(b) {
        Ok(v) => v,
        Err(_) => return false,
    };
    for v in [&mut a_v, &mut b_v] {
        if let Some(obj) = v.as_object_mut() {
            obj.remove("state");
            obj.remove("createdAtMs");
            obj.remove("updatedAtMs");
        }
    }
    a_v == b_v
}

// ---------------------------------------------------------------------------
// Cron expression parsing — next-run computation
// ---------------------------------------------------------------------------

/// Parse a cron field value (min/hr/dom/mon/dow) and check if a value matches.
/// Supports: * (any), */n (every n), n (specific), n,m (list).
/// Does NOT support: n-m (range), n/m (step with start).
fn field_matches(field: &str, value: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        if let Ok(n) = step.parse::<u32>() {
            return n > 0 && value % n == 0;
        }
    }
    // Handle comma-separated lists (each part may be a value, range, or step)
    if field.contains(',') {
        return field.split(',').any(|part| field_matches(part.trim(), value));
    }
    // Handle range: "9-17" means 9 through 17 inclusive (standard cron semantics)
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return value >= start && value <= end;
            }
        }
    }
    field.parse::<u32>().map(|v| v == value).unwrap_or(false)
}

/// Check if a dow value matches a dow field.
/// Dow ranges use INCLUSIVE end (e.g., "1-5" = 1,2,3,4,5, where 1=Sunday).
/// Same inclusive-end semantics as field_matches.
fn dow_matches(field: &str, dow: u32) -> bool {
    if field == "*" {
        return true;
    }
    if let Some(step) = field.strip_prefix("*/") {
        if let Ok(n) = step.parse::<u32>() {
            return n > 0 && dow % n == 0;
        }
    }
    // Handle comma-separated lists
    if field.contains(',') {
        return field.split(',').any(|part| dow_matches(part.trim(), dow));
    }
    // Dow ranges: inclusive on end (e.g., "1-5" means 1 through 5 inclusive)
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return dow >= start && dow <= end;
            }
        }
    }
    field.parse::<u32>().map(|v| v == dow).unwrap_or(false)
}

/// Compute the next UTC timestamp (ms) when a cron expression should fire,
/// starting from `from_ms`. Returns None if parsing fails.
/// If `tz` is Some, the cron expression is evaluated in that timezone.
/// Otherwise, UTC is used.
fn compute_next_run_from_expr(cron_expr: &str, from_ms: u64, tz: Option<&str>) -> Option<u64> {
    let fields: Vec<&str> = cron_expr.split_whitespace().collect();
    if fields.len() != 5 {
        warn!(expr = %cron_expr, "cron: expression must have exactly 5 fields");
        return None;
    }
    let [min_f, hr_f, dom_f, mon_f, dow_f] = fields[..] else {
        return None;
    };

    // Parse from_ms as UTC DateTime
    let utc_dt = match chrono::DateTime::from_timestamp_millis(from_ms as i64) {
        Some(dt) => dt,
        None => return None,
    };

    // Determine timezone
    let tz_opt: Option<chrono_tz::Tz> = tz.and_then(|tz_str| tz_str.parse().ok());

    // Search in local time, always using a timezone-aware DateTime.
    // When no timezone is specified, use the system's local timezone (not UTC).
    let tz_for_search: chrono_tz::Tz = tz_opt.unwrap_or_else(crate::config::system_tz);

    // Current minute in the target timezone
    let local_now = utc_dt.with_timezone(&tz_for_search);
    let mut cand = local_now
        .with_second(0).expect("second 0 always valid")
        .with_nanosecond(0).expect("nanosecond 0 always valid");
    cand += chrono::Duration::minutes(1);

    // Search up to 1 year ahead (in local time).
    let max_cand = cand + chrono::Duration::days(366);

    while cand < max_cand {
        // Use naive date's weekday to get the weekday in local time (not UTC)
        // chrono weekday IS compatible with openclaw dow (both use Sunday=0/1 as the anchor)
        let dow = cand.date_naive().weekday().num_days_from_sunday();
        let m = field_matches(mon_f, cand.month());
        let d = field_matches(dom_f, cand.day());
        let w = dow_matches(dow_f, dow);
        // Optimization: if the date fields don't match, skip to next day midnight
        // instead of scanning minute-by-minute.  Reduces worst case from ~525K to ~1460
        // iterations per year.
        if !(m && d && w) {
            // Advance to 00:00 of the next day.
            cand = (cand.date_naive() + chrono::Days::new(1))
                .and_hms_opt(0, 0, 0)
                .and_then(|naive| cand.timezone().from_local_datetime(&naive).single())
                .unwrap_or_else(|| cand + chrono::Duration::days(1));
            continue;
        }
        let h = field_matches(hr_f, cand.hour());
        let mi = field_matches(min_f, cand.minute());
        trace!(expr=%cron_expr, dow, "searching: {} m={} d={} w={} h={} mi={}", cand.date_naive(), m, d, w, h, mi);
        if h && mi {
            // Convert the matched local time to UTC
            let utc_cand = cand.with_timezone(&chrono::Utc);
            debug!(expr=%cron_expr, "MATCH: {} (UTC: {})", cand, utc_cand);
            return Some(utc_cand.timestamp_millis() as u64);
        }
        cand += chrono::Duration::minutes(1);
    }

    warn!(expr = %cron_expr, "cron: no next run found within 1 year");
    None
}

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

async fn run_cron_job(job: &CronJob, agents: &AgentRegistry) -> Result<String> {
    let session_key = job
        .session_key
        .clone()
        .unwrap_or_else(|| format!("cron:{}", job.id));

    let handle = agents
        .get(&job.agent_id)
        .with_context(|| format!("agent not found: {}", job.agent_id))?;

    // Allow configurable timeout via payload.timeout_seconds, default 300s
    let timeout_secs = job
        .payload
        .as_ref()
        .and_then(|p| match p {
CronPayload::Structured { timeout_seconds, .. } => *timeout_seconds,
            CronPayload::Text(_) => None,
        })
        .unwrap_or(300);

    // Register abort flag for this session before dispatching
    let abort_flag = {
        let mut flags = handle.abort_flags.write()
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
                d.to.as_deref().unwrap_or(""),
            ),
            None => ("", ""),
        };
        if let Some(reply) = crate::gateway::preparse::try_preparse_locally(
            job_text,
            handle.as_ref(),
            preparse_channel,
            preparse_peer,
        )
        .await
        {
            // Clear the abort flag (we never dispatched to the agent).
            abort_flag.store(false, std::sync::atomic::Ordering::SeqCst);
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
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };

    handle.tx.send(msg).await.context("agent inbox closed")?;

    let reply = tokio::time::timeout(Duration::from_secs(timeout_secs), reply_rx)
        .await
        .map_err(|_| {
            // Timeout fired: abort the agent execution and capture status for error reporting.
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
        .context("agent dropped reply channel")?;

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
                        let error_detail = text.lines()
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
                    return Err(anyhow!("command exit_code={}, error: {}", exit_code, error_detail));
                }
            }
        }

        info!(job_id = %job.id, len = reply.text.len(), "cron job completed");
        Ok(reply.text)
    }
}

async fn send_delivery(
    channels: &ChannelManager,
    job: &CronJob,
    default_delivery: &Option<CronDelivery>,
    output_text: &str,
) -> Result<()> {
    let delivery = match &job.delivery {
        Some(d) if d.channel.is_some() && d.to.is_some() => {
            debug!(job_id = %job.id, "cron: using job-level delivery");
            d
        }
        Some(_) | None => match default_delivery {
            Some(d) => {
                debug!(job_id = %job.id, mode = ?d.mode, channel = ?d.channel, to = ?d.to, "cron: using default_delivery");
                d
            }
            None => {
                info!(job_id = %job.id, name = ?job.name, "cron: no delivery configured, result discarded. Set delivery on the job or configure default_delivery in cron config.");
                return Ok(());
            }
        },
    };

    let mode = delivery.mode.as_deref().unwrap_or("none");
    if mode == "none" {
        debug!(job_id = %job.id, "cron: delivery mode is 'none', skipping");
        return Ok(());
    }

    let channel_name = match &delivery.channel {
        Some(c) => c,
        None => {
            warn!(job_id = %job.id, "cron: delivery channel not specified");
            return Ok(());
        }
    };

    let to = match &delivery.to {
        Some(t) => t,
        None => {
            warn!(job_id = %job.id, "cron: delivery target 'to' not specified");
            return Ok(());
        }
    };

    let text = output_text.trim();
    // Note: empty text is now handled by the caller which generates a summary
    // We only skip if both text is empty AND this is the original behavior
    // (no default_delivery configured)
    if text.is_empty() && default_delivery.is_none() && job.delivery.is_none() {
        debug!(job_id = %job.id, "cron: output text is empty and no delivery configured");
        return Ok(());
    }

    // Back-compat: historical cron.json5 entries created from the WS chat
    // transport carry channel="ws" (copied from ctx.channel). That is not a
    // registered ChannelManager entry — the desktop broadcaster is registered
    // under "desktop". Remap here so pre-existing jobs still deliver.
    let resolved_channel: &str = if channel_name == "ws" {
        "desktop"
    } else {
        channel_name.as_str()
    };
    let channel = match channels.get(resolved_channel) {
        Some(ch) => ch,
        None => {
            warn!(job_id = %job.id, channel = %channel_name, resolved = %resolved_channel, "cron: channel not found in ChannelManager");
            return Ok(());
        }
    };

    info!(job_id = %job.id, channel = %channel_name, to = %to, text_len = text.len(), "cron: sending delivery");

    let msg = OutboundMessage {
        target_id: to.clone(),
        is_group: false,
        text: text.to_owned(),
        reply_to: delivery.thread_id.clone(),
        images: vec![],
        files: vec![],
        channel: Some(resolved_channel.to_owned()),
    };

    match channel.send(msg).await {
        Ok(()) => {
            info!(job_id = %job.id, channel = %channel_name, to = %to, "cron delivery sent successfully");
            Ok(())
        }
        Err(e) => {
            if delivery.best_effort.unwrap_or(false) {
                warn!(job_id = %job.id, error = %e, "cron delivery failed (best_effort)");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn build_run_log_entry(
    job: &CronJob,
    success: bool,
    error: Option<anyhow::Error>,
) -> RunLogEntry {
    RunLogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        job_id: job.id.clone(),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        success,
        reply_preview: None,
        error: error.map(|e| e.to_string()),
    }
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
                (None, String::new(), format!("timed out after {} seconds", cmd_timeout.as_secs()))
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

    // Wait for background task result (non-blocking for spawned task, but waits here)
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
        return Err(anyhow!("command exit_code={}, error: {}", exit_code, error_msg));
    }

    // Get raw output
    let raw_output = if !stdout.is_empty() {
        stdout
    } else if !stderr.is_empty() {
        stderr
    } else {
        "command succeeded with no output".to_string()
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

        // Create summarize prompt with real output
        // CRITICAL: Tell the LLM that the output MUST be summarized and returned.
        // The summary will be sent to the user. Do NOT just call memory tool.
        // Use "summarize:" prefix to disable all tools (internal channels have memory tool).
        let summarize_prompt = format!(
            "【定时任务执行结果】\n\
            以下是一个脚本执行的真实输出，脚本返回了内容。\n\
            你必须：\n\
            1. 用简洁的语言总结关键信息（不要编造数据，只总结已有内容）\n\
            2. 直接返回摘要文本给用户\n\
            3. 不要返回 HEARTBEAT_OK\n\n\
            输出内容：\n```\n{}\n```",
            raw_output
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
            extra_tools: vec![], // No tools - only summarize
            images: vec![],
            files: vec![],
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
        Ok(raw_output)
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

// ---------------------------------------------------------------------------
// Cron store file helpers (used by gateway API)
// ---------------------------------------------------------------------------

/// Returns the cron store file path.
/// Respects RSCLAW_BASE_DIR env var (same as other rsclaw data).
pub fn resolve_cron_store_path() -> PathBuf {
    let base = crate::config::loader::base_dir();
    base.join("cron.json5")
}

/// Load cron jobs from the cron store file.
/// Auto-migrates legacy `cron/jobs.json` to `cron.json5` if needed.
/// Returns an empty list if no file exists.
pub fn load_cron_jobs() -> Vec<CronJob> {
    let source = resolve_cron_store_path();

    // Auto-migrate legacy cron/jobs.json -> cron.json5
    if !source.exists() {
        let base = crate::config::loader::base_dir();
        let legacy = base.join("cron").join("jobs.json");
        if legacy.exists() {
            info!(from = %legacy.display(), to = %source.display(), "migrating legacy cron/jobs.json to cron.json5");
            if let Err(e) = std::fs::copy(&legacy, &source) {
                warn!(err = %e, "failed to migrate legacy cron/jobs.json");
            } else {
                // Remove legacy file and empty directory
                if let Err(e) = std::fs::remove_file(&legacy) {
                    tracing::debug!("failed to remove legacy cron file: {e}");
                }
                if let Err(e) = std::fs::remove_dir(base.join("cron")) {
                    tracing::debug!("failed to remove legacy cron dir: {e}");
                }
            }
        }
    }

    if !source.exists() {
        return Vec::new();
    }

    let raw = match std::fs::read_to_string(&source) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };

    let parsed: serde_json::Value = json5::from_str(&raw).or_else(|_| serde_json::from_str(&raw)).unwrap_or_default();

    let jobs_array = if let Some(arr) = parsed.get("jobs").and_then(|v| v.as_array()) {
        arr.clone()
    } else if parsed.is_array() {
        parsed.as_array().cloned().unwrap_or_default()
    } else {
        Vec::new()
    };

    let total = jobs_array.len();
    let jobs: Vec<CronJob> = jobs_array
        .iter()
        .filter_map(|v| match serde_json::from_value::<CronJob>(v.clone()) {
            Ok(job) => Some(job),
            Err(e) => {
                warn!(err = %e, job_json = %serde_json::to_string_pretty(&v).unwrap_or_default(), "failed to parse cron job");
                None
            }
        })
        .collect();
    let loaded = jobs.len();
    if loaded < total {
        warn!(file = %source.display(), total, loaded, "some cron jobs failed to parse");
    }

    jobs
}

/// Save cron jobs to the cron store file (openclaw-compatible path).
pub fn save_cron_jobs(jobs: &[CronJob]) -> anyhow::Result<()> {
    let cron_file = resolve_cron_store_path();
    debug!(path = %cron_file.display(), "cron: saving jobs to file");

    let store = serde_json::json!({
        "version": 1,
        "jobs": jobs,
    });

    let json = serde_json::to_string_pretty(&store)
        .context("failed to serialize cron jobs to JSON")?;

    // Ensure directory exists
    if let Some(parent) = cron_file.parent() {
        std::fs::create_dir_all(parent).context("failed to create cron directory")?;
    }

    std::fs::write(&cron_file, json).context("failed to write cron jobs file")?;
    Ok(())
}

/// Global mutex that serializes read-modify-write on the cron store file.
/// Without this, concurrent `cron.add` calls (common when an LLM dispatches
/// multiple tool calls in one turn) race and silently lose writes.
pub static CRON_FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------------------
// Cross-module reload signal
// ---------------------------------------------------------------------------
//
// Lets non-server code paths (e.g. fast preparse `/loop`) ask the cron runner
// to reload `cron.json5` after appending a new job. Populated once at gateway
// startup with the same broadcast sender wired into AppState.

static CRON_RELOAD_TX: OnceLock<broadcast::Sender<()>> = OnceLock::new();

/// Install the cron reload broadcast sender. Called once at gateway startup.
/// Subsequent installs are silently ignored (idempotent).
pub fn install_reload_sender(tx: broadcast::Sender<()>) {
    if CRON_RELOAD_TX.set(tx).is_err() {
        warn!("cron: reload sender already installed, ignoring duplicate install");
    }
}

/// Trigger a cron reload from anywhere in the crate. Returns `true` if the
/// signal was sent, `false` if no sender is installed yet (during early
/// startup) or if every receiver has been dropped.
pub fn trigger_reload() -> bool {
    match CRON_RELOAD_TX.get() {
        Some(tx) => tx.send(()).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod cron_config_equal_tests {
    use super::*;

    fn job(id: &str, expr: &str, msg: &str) -> CronJob {
        CronJob {
            id: id.to_string(),
            name: Some(id.to_string()),
            agent_id: "default".to_string(),
            session_key: None,
            enabled: true,
            schedule: CronSchedule::Flat(expr.to_string()),
            payload: None,
            message: Some(msg.to_string()),
            delivery: None,
            session_target: None,
            wake_mode: None,
            state: None,
            created_at_ms: Some(1_000),
            updated_at_ms: Some(1_000),
        }
    }

    #[test]
    fn identical_jobs_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/5 * * * *", "ping");
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn different_message_not_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/5 * * * *", "pong");
        assert!(!cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn different_schedule_not_equal() {
        let a = job("j1", "*/5 * * * *", "ping");
        let b = job("j1", "*/30 * * * *", "ping");
        assert!(!cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn state_diff_still_equal() {
        // State is runtime-only; two configs that differ only in state must
        // be treated as equal so a state update doesn't trip cancellation.
        let mut a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        a.state = Some(CronJobState {
            consecutive_errors: 0,
            ..Default::default()
        });
        b.state = Some(CronJobState {
            consecutive_errors: 7,
            last_error: Some("boom".to_string()),
            next_run_at_ms: Some(99_999),
            ..Default::default()
        });
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn updated_at_diff_still_equal() {
        // updated_at_ms flips on every save; treating it as a config change
        // would cause spurious cancellations.
        let mut a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        a.updated_at_ms = Some(1_000);
        b.updated_at_ms = Some(2_000);
        assert!(cron_jobs_config_equal(&a, &b));
    }

    #[test]
    fn enabled_diff_not_equal() {
        // Toggling enabled IS a meaningful change — but the cancellation
        // path for disabled jobs goes through the active_unchanged filter
        // (a disabled job is not in active_unchanged), so it'd be cancelled
        // either way.  This just documents that enabled is part of config.
        let a = job("j1", "*/5 * * * *", "ping");
        let mut b = job("j1", "*/5 * * * *", "ping");
        b.enabled = false;
        assert!(!cron_jobs_config_equal(&a, &b));
    }
}

#[cfg(test)]
mod cron_validate_tests {
    use super::validate_cron_expr;

    #[test]
    fn accepts_common_patterns() {
        for ok in ["*/5 * * * *", "0 17 * * *", "30 8 * * 1-5", "0 9 1 * *"] {
            assert!(validate_cron_expr(ok).is_ok(), "should accept '{}'", ok);
        }
    }

    #[test]
    fn rejects_empty() {
        assert!(validate_cron_expr("").is_err());
        assert!(validate_cron_expr("   ").is_err());
    }

    #[test]
    fn rejects_four_fields_with_hint() {
        let err = validate_cron_expr("017 * * *").unwrap_err();
        assert!(err.contains("5 fields"), "err = {err}");
        assert!(err.contains("0 17"), "should hint at '0 17': {err}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(validate_cron_expr("not a cron").is_err());
    }
}

/// Validate a cron expression at save time. Returns a friendly error string
/// the LLM can act on, instead of silently accepting broken expressions and
/// failing later at scheduling time.
pub fn validate_cron_expr(expr: &str) -> Result<(), String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return Err("cron expression is empty".to_owned());
    }
    let fields: Vec<&str> = trimmed.split_whitespace().collect();
    if fields.len() != 5 {
        // Build a hint that catches the common "forgot a space" mistake.
        // E.g. "017 * * *" → hint that "017" might be "0 17" (4 fields → 5).
        let hint = if fields.len() == 4 && fields[0].len() >= 2 && fields[0].chars().all(|c| c.is_ascii_digit()) {
            let n = fields[0];
            format!(
                " — looks like a missing space: '{}' could be '{} {}' which makes 5 fields (e.g. '0 17 * * *' for 5pm daily)",
                n,
                &n[..1],
                &n[1..]
            )
        } else {
            String::new()
        };
        return Err(format!(
            "cron expression must have exactly 5 fields separated by spaces \
             (minute hour day month weekday), got {} field(s): '{}'{}",
            fields.len(),
            trimmed,
            hint
        ));
    }
    // Delegate range parsing to the existing scheduler. If it can compute a
    // next run, the expression is valid; otherwise reject.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if compute_next_run_from_expr(trimmed, now, None).is_none() {
        return Err(format!(
            "cron expression '{}' could not be parsed. Valid examples: \
             '*/5 * * * *' (every 5 min), '0 17 * * *' (5pm daily), \
             '0 9 * * 1' (9am Mondays)",
            trimmed
        ));
    }
    Ok(())
}
