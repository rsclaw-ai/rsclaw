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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Semaphore};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tracing::{debug, info, warn};

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

/// Get backoff delay for consecutive error count.
fn error_backoff_ms(consecutive_errors: u32) -> u64 {
    let idx = (consecutive_errors.saturating_sub(1) as usize).min(ERROR_BACKOFF_MS.len() - 1);
    ERROR_BACKOFF_MS[idx]
}

// ---------------------------------------------------------------------------
// CronJob — serialisable description of a single scheduled task
// ---------------------------------------------------------------------------

/// Schedule descriptor — supports both rsclaw flat format and OpenClaw nested format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronSchedule {
    /// Flat string: "*/30 9-11 * * 1-5" (rsclaw native).
    Flat(String),
    /// Nested object: { kind: "cron", expr: "...", tz: "Asia/Shanghai" } (OpenClaw compat).
    Nested {
        #[serde(default)]
        kind: Option<String>,
        expr: String,
        #[serde(default)]
        tz: Option<String>,
    },
    /// Interval-based schedule: { kind: "every", everyMs: 259200000, anchorMs: ... } (OpenClaw compat).
    Every {
        #[serde(default, alias = "everyMs")]
        every_ms: Option<u64>,
        #[serde(default, alias = "anchorMs")]
        anchor_ms: Option<u64>,
    },
}

impl CronSchedule {
    pub fn expr(&self) -> &str {
        match self {
            CronSchedule::Flat(s) => s,
            CronSchedule::Nested { expr, .. } => expr,
            CronSchedule::Every { .. } => "every",
        }
    }

    pub fn tz(&self) -> Option<&str> {
        match self {
            CronSchedule::Flat(_) => None,
            CronSchedule::Nested { tz, .. } => tz.as_deref(),
            CronSchedule::Every { .. } => None,
        }
    }

    /// Compute the next run timestamp (ms) from the given `from_ms`.
    /// For cron schedules: searches forward up to 1 year.
    /// For interval schedules (every): uses anchor + n*everyMs.
    pub fn compute_next_run(&self, from_ms: u64) -> Option<u64> {
        match self {
            CronSchedule::Flat(expr) => {
                crate::cron::compute_next_run_from_expr(expr, from_ms, None)
            }
            CronSchedule::Nested { expr, tz, .. } => {
                crate::cron::compute_next_run_from_expr(expr, from_ms, tz.as_deref())
            }
            CronSchedule::Every { every_ms, anchor_ms } => {
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
    },
}

impl CronPayload {
    pub fn text(&self) -> &str {
        match self {
            CronPayload::Text(s) => s,
            CronPayload::Structured { text, .. } => text.as_deref().unwrap_or(""),
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
            CronSchedule::Nested {
                kind: Some("cron".to_string()),
                expr: cfg.schedule.clone(),
                tz: Some(tz.clone()),
            }
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
}

impl CronRunner {
    pub fn new(
        config: &CronConfig,
        jobs: Vec<CronJob>,
        agents: Arc<AgentRegistry>,
        channels: Arc<ChannelManager>,
        data_dir: PathBuf,
        reload_tx: broadcast::Sender<()>,
    ) -> Self {
        let run_log_dir = data_dir.join("cron");
        let store_path = data_dir.join("cron_store.json");
        let _ = std::fs::create_dir_all(&run_log_dir);
        Self {
            jobs,
            agents,
            channels,
            run_log_dir,
            store_path,
            semaphore: Arc::new(Semaphore::new(4)),
            default_delivery: config.default_delivery.clone(),
            reload_tx,
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
        let mut reload_rx = self.reload_tx.subscribe();

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
        loop {
            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            let now_ms = current_timestamp_ms();

            // Find next wake time among enabled jobs
            let next_wake = jobs
                .iter()
                .filter(|j| j.enabled)
                .filter_map(|j| j.state.as_ref().and_then(|s| s.next_run_at_ms))
                .min();

            // DEBUG: log next_wake to diagnose early firing
            debug!(next_wake = next_wake.unwrap_or(0), now_ms, "cron: timer tick");
            if next_wake.map(|t| t <= now_ms).unwrap_or(false) {
                warn!(next_wake = next_wake.unwrap_or(0), now_ms, "cron: next_wake is in the past!");
            }

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

                jobs = self.merge_jobs(&jobs, new_jobs, now_ms);

                // Debug: check enabled state after merge
                let disabled_after_merge: Vec<_> = jobs.iter()
                    .filter(|j| !j.enabled)
                    .map(|j| (&j.id, j.enabled))
                    .collect();
                info!(after_merge_count = jobs.len(), disabled=?disabled_after_merge, "cron: merge complete");

                if let Err(e) = self.save_store(&jobs).await {
                    warn!(err = %e, "cron: failed to save store after reload");
                }
                info!(old_count, new_count = jobs.len(), file_count, "cron jobs reloaded");
                continue;
            }

            // Collect jobs that are due and not already running
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

            // Execute due jobs with concurrency limit
            let mut handles = Vec::new();

            for job_id in due {
                let permit = semaphore.clone().acquire_owned().await.ok();
                if permit.is_none() {
                    // Max concurrency reached
                    break;
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

                let permit = permit.unwrap();
                let job = job.clone();
                let agents = Arc::clone(&self.agents);
                let channels = Arc::clone(&self.channels);
                let run_log_dir = self.run_log_dir.clone();
                let default_delivery = self.default_delivery.clone();

                let handle = tokio::spawn(async move {
                    let start_time = current_timestamp_ms();
                    let job_started_at = started_at;
                    let prev_consecutive_errors = job.state.as_ref().map(|s| s.consecutive_errors).unwrap_or(0);
                    info!(job_id = %job.id, "cron job triggered");

                    let result = run_cron_job(&job, &agents).await;
                    let duration_ms = current_timestamp_ms() - start_time;
                    drop(permit);

                    // Build delivery message with execution summary
                    let delivery_text = match &result {
                        Ok(output) if !output.trim().is_empty() => {
                            // Agent returned output, use it directly
                            output.clone()
                        }
                        Ok(_) => {
                            // Success but no output - send summary
                            let job_name = job.name.as_deref().unwrap_or(&job.id);
                            format!(
                                "✅ 定时任务执行完成\n\n**任务**: {}\n**耗时**: {}秒",
                                job_name,
                                duration_ms / 1000
                            )
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

                            if will_disable {
                                format!(
                                    "❌ 定时任务执行失败\n\n**任务**: {}\n**连续失败**: {} 次\n**错误**: {}\n\n⚠️ 任务已被自动禁用，请检查配置后手动启用。",
                                    job_name,
                                    consecutive,
                                    e
                                )
                            } else {
                                format!(
                                    "❌ 定时任务执行失败\n\n**任务**: {}\n**连续失败**: {} 次\n**下次重试**: {}后\n**错误**: {}",
                                    job_name,
                                    consecutive,
                                    backoff_text,
                                    e
                                )
                            }
                        }
                    };

                    if let Err(e) =
                        send_delivery(&channels, &job, &default_delivery, &delivery_text).await
                    {
                        warn!(job_id = %job.id, %e, "delivery failed");
                    }

                    let entry = build_run_log_entry(
                        &job,
                        result.is_ok(),
                        result.as_ref().err().map(|e| anyhow::anyhow!("{e}")),
                    );
                    let _ = write_run_log(&run_log_dir, &job.id, entry).await;

                    let error_msg = result.as_ref().err().map(|e| e.to_string());
                    (job.id, result.is_ok(), duration_ms, job_started_at, error_msg)
                });

                handles.push(handle);
            }

            // Wait for all jobs to complete and update state
            for handle in handles {
                if let Ok((job_id, success, duration_ms, started_at, error_msg)) = handle.await {
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
                                // Compute next run normally
                                state.next_run_at_ms = job.schedule.compute_next_run(completion_time);
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
            }

            // Persist updated state
            if let Err(e) = self.save_store(&jobs).await {
                warn!(err = %e, "cron: failed to persist state");
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
        let prev_consecutive_errors = job.state.as_ref().map(|s| s.consecutive_errors).unwrap_or(0);
        let result = run_cron_job(job, &self.agents).await;
        let success = result.is_ok();

        // Build delivery message with execution summary
        let delivery_text = match &result {
            Ok(output) if !output.trim().is_empty() => output.clone(),
            Ok(_) => {
                let job_name = job.name.as_deref().unwrap_or(&job.id);
                format!("✅ 定时任务执行完成\n\n**任务**: {}", job_name)
            }
            Err(e) => {
                let job_name = job.name.as_deref().unwrap_or(&job.id);
                let consecutive = prev_consecutive_errors + 1;
                // Manual trigger: show error but don't mention auto-disable
                // (manual triggers don't count toward auto-disable threshold)
                format!(
                    "❌ 定时任务执行失败\n\n**任务**: {}\n**连续失败**: {} 次\n**错误**: {}",
                    job_name, consecutive, e
                )
            }
        };

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
    fn merge_jobs(&self, old_jobs: &[CronJob], new_jobs: Vec<CronJob>, now_ms: u64) -> Vec<CronJob> {
        let mut result = Vec::with_capacity(new_jobs.len());

        for mut new_job in new_jobs {
            // Try to find existing job by ID and preserve its state
            if let Some(old_job) = old_jobs.iter().find(|j| j.id == new_job.id) {
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

            // Ensure next_run_at_ms is set
            if let Some(ref mut state) = new_job.state {
                if state.next_run_at_ms.is_none() {
                    state.next_run_at_ms = new_job.schedule.compute_next_run(now_ms);
                }
            }

            result.push(new_job);
        }

        result
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
        }
    }
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
    // Handle range: "9-15" means 9 through 15 exclusive (start <= value < end)
    // This matches openclaw's interpretation where "9-11" means 9,10 only (not 11:59)
    // and "13-15" means 13,14 only (not 15:59)
    if field.contains('-') {
        let parts: Vec<&str> = field.split('-').collect();
        if parts.len() == 2 {
            if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                return value >= start && value < end;
            }
        }
    }
    field.parse::<u32>().map(|v| v == value).unwrap_or(false)
}

/// Check if a dow value matches a dow field.
/// Dow ranges use INCLUSIVE end (e.g., "1-5" = 1,2,3,4,5, where 1=Sunday).
/// This differs from hour/dom ranges which use exclusive end.
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

    // Search in local time, always using a timezone-aware DateTime
    // (UTC if no timezone specified, so we still use DateTime<chrono_tz::Tz> with UTC)
    let tz_for_search: chrono_tz::Tz = match tz_opt {
        Some(tz) => tz,
        None => chrono_tz::UTC,
    };

    // Current minute in the target timezone
    let local_now = utc_dt.with_timezone(&tz_for_search);
    let mut cand = local_now.with_second(0).unwrap().with_nanosecond(0).unwrap();
    cand += chrono::Duration::minutes(1);

    // Search up to 1 year ahead (in local time)
    let max_cand = cand + chrono::Duration::days(366);

    while cand < max_cand {
        // Use naive date's weekday to get the weekday in local time (not UTC)
        // chrono: Monday=0, Tuesday=1, ..., Sunday=6
        // openclaw: Monday=1, Tuesday=2, ..., Sunday=7
        // chrono: Sunday=0, Monday=1, ..., Saturday=6
        // openclaw dow: Sunday=1, Monday=2, ..., Saturday=7
        // chrono weekday IS compatible with openclaw dow (both use Sunday=0/1 as the anchor)
        let dow = cand.date_naive().weekday().num_days_from_sunday();
        let m = field_matches(mon_f, cand.month());
        let d = field_matches(dom_f, cand.day());
        let w = dow_matches(dow_f, dow);
        let h = field_matches(hr_f, cand.hour());
        let mi = field_matches(min_f, cand.minute());
        debug!(expr=%cron_expr, dow, "searching: {} m={} d={} w={} h={} mi={}", cand.date_naive(), m, d, w, h, mi);
        if m && d && w && h && mi {
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
        .unwrap()
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

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key,
        text: job.effective_message().to_owned(),
        channel: "cron".to_string(),
        peer_id: format!("cron:{}", job.id),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };

    handle.tx.send(msg).await.context("agent inbox closed")?;

    let reply = tokio::time::timeout(Duration::from_secs(300), reply_rx)
        .await
        .context("cron job timed out after 300s")?
        .context("agent dropped reply channel")?;

    if reply.is_empty {
        debug!(job_id = %job.id, "cron job returned no output");
        Ok(String::new())
    } else {
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
                debug!(job_id = %job.id, "cron: no delivery configured, skipping notification");
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

    let channel = match channels.get(channel_name) {
        Some(ch) => ch,
        None => {
            warn!(job_id = %job.id, channel = %channel_name, "cron: channel not found in ChannelManager");
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
        channel: Some(channel_name.to_owned()),
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
    let cron_dir = if let Some(state_dir) = std::env::var_os("OPENCLAW_STATE_DIR") {
        PathBuf::from(state_dir)
    } else {
        dirs_next::home_dir().unwrap_or_default().join(".openclaw")
    };
    cron_dir.join("cron").join("jobs.json")
}

/// Load cron jobs from the cron store file (openclaw-compatible path).
/// Returns an empty list if no file exists.
pub fn load_cron_jobs() -> Vec<CronJob> {
    let source = resolve_cron_store_path();

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
