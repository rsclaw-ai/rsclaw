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
use tokio::sync::Semaphore;
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

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
}

impl CronSchedule {
    pub fn expr(&self) -> &str {
        match self {
            CronSchedule::Flat(s) => s,
            CronSchedule::Nested { expr, .. } => expr,
        }
    }

    pub fn tz(&self) -> Option<&str> {
        match self {
            CronSchedule::Flat(_) => None,
            CronSchedule::Nested { tz, .. } => tz.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronPayload {
    Text(String),
    Structured {
        #[serde(default)]
        kind: Option<String>,
        text: String,
    },
}

impl CronPayload {
    pub fn text(&self) -> &str {
        match self {
            CronPayload::Text(s) => s,
            CronPayload::Structured { text, .. } => text,
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
}

impl CronRunner {
    pub fn new(
        config: &CronConfig,
        jobs: Vec<CronJob>,
        agents: Arc<AgentRegistry>,
        channels: Arc<ChannelManager>,
        data_dir: PathBuf,
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

            let cron_expr = job.cron_expr().to_string();
            let state = job.state.as_mut().unwrap();

            // Clear stale running marker
            if let Some(running_at) = state.running_at_ms {
                if now_ms - running_at > STUCK_RUN_MS {
                    warn!(job_id = %job.id, "cron: clearing stale running marker");
                    state.running_at_ms = None;
                }
            }

            // Compute next_run_at_ms if not set
            if state.next_run_at_ms.is_none() {
                state.next_run_at_ms = compute_next_run(&cron_expr, now_ms);
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

        let runner = self.clone();
        let timer_handle = tokio::spawn(async move {
            runner.timer_loop(jobs, running_clone, semaphore).await;
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

            let Some(next_wake) = next_wake else {
                // No jobs — wait max interval and re-check
                debug!("cron: no jobs scheduled, waiting {}ms", MAX_TIMER_DELAY_MS);
                sleep(Duration::from_millis(MAX_TIMER_DELAY_MS)).await;
                continue;
            };

            let delay = next_wake.saturating_sub(now_ms);
            let delay_ms = if delay == 0 {
                // Due now — prevent tight loop with MIN_REFIRE_GAP
                MIN_REFIRE_GAP_MS
            } else {
                delay.min(MAX_TIMER_DELAY_MS)
            };

            debug!(next_wake, delay_ms, "cron: sleeping until next job");
            sleep(Duration::from_millis(delay_ms)).await;

            if !running.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            let now_ms = current_timestamp_ms();

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

            if due.is_empty() {
                continue;
            }

            debug!(count = due.len(), "cron: {} jobs due", due.len());

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

                // Mark as running and compute next run
                let started_at = current_timestamp_ms();
                let cron_expr = job.cron_expr().to_string();
                if let Some(state) = job.state.as_mut() {
                    state.running_at_ms = Some(started_at);
                    state.next_run_at_ms = compute_next_run(&cron_expr, started_at);
                }

                let permit = permit.unwrap();
                let job = job.clone();
                let agents = Arc::clone(&self.agents);
                let channels = Arc::clone(&self.channels);
                let run_log_dir = self.run_log_dir.clone();
                let default_delivery = self.default_delivery.clone();

                let handle = tokio::spawn(async move {
                    let start_time = current_timestamp_ms();
                    info!(job_id = %job.id, "cron job triggered");

                    let result = run_cron_job(&job, &agents).await;
                    let duration_ms = current_timestamp_ms() - start_time;
                    drop(permit);

                    match &result {
                        Ok(output) => {
                            if let Err(e) =
                                send_delivery(&channels, &job, &default_delivery, output).await
                            {
                                warn!(job_id = %job.id, %e, "delivery failed");
                            }
                        }
                        Err(e) => {
                            error!(job_id = %job.id, %e, "cron job failed");
                        }
                    }

                    let entry = build_run_log_entry(
                        &job,
                        result.is_ok(),
                        result.as_ref().err().map(|e| anyhow::anyhow!("{e}")),
                    );
                    let _ = write_run_log(&run_log_dir, &job.id, entry).await;

                    (job.id, result.is_ok(), duration_ms)
                });

                handles.push(handle);
            }

            // Wait for all jobs to complete and update state
            for handle in handles {
                if let Ok((job_id, success, duration_ms)) = handle.await {
                    if let Some(job) = jobs.iter_mut().find(|j| j.id == job_id) {
                        if let Some(state) = job.state.as_mut() {
                            state.running_at_ms = None;
                            state.last_run_at_ms = Some(current_timestamp_ms());
                            state.last_duration_ms = Some(duration_ms);

                            if success {
                                state.consecutive_errors = 0;
                                state.last_run_status = Some("ok".to_string());
                                state.last_status = Some("ok".to_string());
                                state.last_error = None;
                            } else {
                                state.consecutive_errors += 1;
                                state.last_run_status = Some("error".to_string());
                                state.last_status = Some("error".to_string());
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
        let result = run_cron_job(job, &self.agents).await;
        let success = result.is_ok();
        if let Ok(output) = &result {
            if let Err(e) =
                send_delivery(&self.channels, job, &self.default_delivery, output).await
            {
                warn!(job_id = %job.id, %e, "delivery failed");
            }
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
    // Handle range: "9-15" means 9 through 15 inclusive
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

/// Compute the next UTC timestamp (ms) when a cron expression should fire,
/// starting from `from_ms`. Returns None if parsing fails.
fn compute_next_run(cron_expr: &str, from_ms: u64) -> Option<u64> {
    let fields: Vec<&str> = cron_expr.split_whitespace().collect();
    if fields.len() != 5 {
        warn!(expr = %cron_expr, "cron: expression must have exactly 5 fields");
        return None;
    }
    let [min_f, hr_f, dom_f, mon_f, dow_f] = fields[..] else {
        return None;
    };

    // Parse from_ms as UTC NaiveDateTime
    let from = match chrono::DateTime::from_timestamp_millis(from_ms as i64) {
        Some(dt) => dt.naive_utc(),
        None => return None,
    };

    // Current minute boundary (we trigger at the start of the minute)
    let mut cand = from.with_second(0).unwrap().with_nanosecond(0).unwrap();
    cand += chrono::Duration::minutes(1);

    // Search up to 1 year ahead
    let max_cand = cand + chrono::Duration::days(366);

    while cand < max_cand {
        if field_matches(mon_f, cand.month())
            && field_matches(dom_f, cand.day())
            && field_matches(dow_f, (cand.weekday().num_days_from_sunday() % 7) + 1)
            && field_matches(hr_f, cand.hour())
            && field_matches(min_f, cand.minute())
        {
            return Some(cand.and_utc().timestamp_millis() as u64);
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
        Some(d) => d,
        None => match default_delivery {
            Some(d) => d,
            None => {
                return Ok(());
            }
        },
    };

    let mode = delivery.mode.as_deref().unwrap_or("none");
    if mode == "none" {
        return Ok(());
    }

    let channel_name = match &delivery.channel {
        Some(c) => c,
        None => return Ok(()),
    };

    let to = match &delivery.to {
        Some(t) => t,
        None => return Ok(()),
    };

    let text = output_text.trim();
    if text.is_empty() {
        return Ok(());
    }

    let channel = match channels.get(channel_name) {
        Some(ch) => ch,
        None => return Ok(()),
    };

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
            info!(job_id = %job.id, channel = %channel_name, to = %to, "cron delivery sent");
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

/// Returns the cron store file path (openclaw-compatible).
/// Respects OPENCLAW_STATE_DIR env var (same as openclaw).
fn resolve_cron_store_path() -> PathBuf {
    let cron_dir = if let Some(state_dir) = std::env::var_os("OPENCLAW_STATE_DIR") {
        tracing::debug!("cron store: using OPENCLAW_STATE_DIR={}", state_dir.to_string_lossy());
        PathBuf::from(state_dir)
    } else {
        let home = dirs_next::home_dir().unwrap_or_default();
        tracing::debug!("cron store: OPENCLAW_STATE_DIR not set, using home={}", home.display());
        home.join(".openclaw")
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

    jobs_array
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect()
}

/// Save cron jobs to the cron store file (openclaw-compatible path).
pub fn save_cron_jobs(jobs: &[CronJob]) -> std::io::Result<()> {
    let cron_file = resolve_cron_store_path();

    let store = serde_json::json!({
        "version": 1,
        "jobs": jobs,
    });

    let json = serde_json::to_string_pretty(&store).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
    })?;

    // Ensure directory exists
    if let Some(parent) = cron_file.parent() {
        std::fs::create_dir_all(parent)?;
    }

    std::fs::write(&cron_file, json)?;
    Ok(())
}
