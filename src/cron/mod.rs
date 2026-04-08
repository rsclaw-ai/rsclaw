//! Cron job scheduler — runs periodic agent tasks (AGENTS.md §16).
//!
//! Schedules are parsed as cron expressions (5-field: min/hr/dom/mon/dow).
//! Internally converted to 6-field (sec/min/hr/dom/mon/dow) for
//! tokio-cron-scheduler which requires a leading seconds field.
//!
//! Each job runs in an isolated session (`cron:<jobId>`) or a persistent
//! session (`session:<key>`). Concurrent runs are capped by
//! `max_concurrent_runs`.
//!
//! Fully compatible with OpenClaw cron format (name, timezone, timestamps).

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt as _, sync::Semaphore};
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{debug, error, info, warn};

use crate::{
    agent::{AgentMessage, AgentRegistry},
    channel::{ChannelManager, OutboundMessage},
    config::schema::{CronConfig, CronDelivery, CronJobConfig},
};

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
    /// Return the cron expression string.
    pub fn expr(&self) -> &str {
        match self {
            CronSchedule::Flat(s) => s,
            CronSchedule::Nested { expr, .. } => expr,
        }
    }

    /// Return the timezone, if any.
    pub fn tz(&self) -> Option<&str> {
        match self {
            CronSchedule::Flat(_) => None,
            CronSchedule::Nested { tz, .. } => tz.as_deref(),
        }
    }
}

/// Payload descriptor (OpenClaw compat).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CronPayload {
    /// Plain text message (rsclaw native).
    Text(String),
    /// Structured payload: { kind: "systemEvent", text: "..." } (OpenClaw compat).
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJob {
    pub id: String,
    /// Human-readable name (OpenClaw compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Agent ID (OpenClaw: agentId).
    #[serde(default)]
    pub agent_id: String,
    /// Session key for persistent context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    pub enabled: bool,
    /// Schedule: flat string or nested {kind, expr, tz} object.
    pub schedule: CronSchedule,
    /// Message/payload: flat string or nested {kind, text} object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronPayload>,
    /// Plain message field (rsclaw native, takes precedence if payload is absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Delivery target for notifications when job completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<CronDelivery>,
    // -- OpenClaw compat fields --
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
    /// Get the effective message text (payload.text > message).
    pub fn effective_message(&self) -> &str {
        if let Some(ref payload) = self.payload {
            return payload.text();
        }
        self.message.as_deref().unwrap_or("")
    }

    /// Get the cron expression.
    pub fn cron_expr(&self) -> &str {
        self.schedule.expr()
    }

    /// Get the timezone, if configured.
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
            agent_id: cfg
                .agent_id
                .clone()
                .unwrap_or_else(|| "default".to_string()),
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
// RunLogEntry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogEntry {
    pub id: String,
    pub job_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
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
    #[allow(dead_code)]
    max_concurrent: usize,
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
        let max_concurrent = config.max_concurrent_runs.unwrap_or(4) as usize;
        let run_log_dir = data_dir.join("cron");
        let _ = std::fs::create_dir_all(&run_log_dir);
        Self {
            jobs,
            agents,
            channels,
            run_log_dir,
            max_concurrent,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            default_delivery: config.default_delivery.clone(),
        }
    }

    pub fn jobs(&self) -> &[CronJob] {
        &self.jobs
    }

    /// Start all enabled cron jobs and block until Ctrl-C.
    pub async fn run(&self) -> Result<()> {
        let mut scheduler = JobScheduler::new()
            .await
            .context("failed to create cron scheduler")?;

        for cron_job in &self.jobs {
            if !cron_job.enabled {
                debug!(job_id = %cron_job.id, "cron job disabled, skipping");
                continue;
            }

            // tokio-cron-scheduler requires 6-field cron (leading seconds field).
            // Convert standard 5-field "min hr dom mon dow" → "0 min hr dom mon dow".
            let schedule = to_six_field(cron_job.cron_expr());

            let job_clone = cron_job.clone();
            let agents = Arc::clone(&self.agents);
            let channels = Arc::clone(&self.channels);
            let run_log_dir = self.run_log_dir.clone();
            let sem = Arc::clone(&self.semaphore);
            let default_delivery = self.default_delivery.clone();

            let tokio_job = if let Some(tz_str) = cron_job.timezone() {
                // Timezone-aware scheduling.
                let tz: chrono_tz::Tz = tz_str
                    .parse()
                    .with_context(|| format!("invalid timezone `{tz_str}` for job `{}`", cron_job.id))?;
                Job::new_async_tz(schedule.as_str(), tz, move |_uuid, _scheduler| {
                    let job = job_clone.clone();
                    let agents = Arc::clone(&agents);
                    let channels = Arc::clone(&channels);
                    let run_log_dir = run_log_dir.clone();
                    let sem = Arc::clone(&sem);
                    let default_delivery = default_delivery.clone();
                    Box::pin(async move {
                        let Ok(_permit) = sem.acquire().await else {
                            return;
                        };
                        info!(job_id = %job.id, "cron job triggered");
                        let result = run_cron_job(&job, &agents).await;
                        match &result {
                            Ok(output) => {
                                // Send delivery notification if configured
                                if let Err(e) = send_delivery(&channels, &job, &default_delivery, output).await {
                                    warn!(job_id = %job.id, %e, "delivery failed");
                                }
                            }
                            Err(e) => {
                                error!(job_id = %job.id, %e, "cron job failed");
                            }
                        }
                        let entry = build_run_log_entry(&job, result.is_ok(), result.as_ref().err().map(|e| anyhow::anyhow!("{e}")));
                        let _ = write_run_log(&run_log_dir, &job.id, entry).await;
                    })
                })
                .with_context(|| {
                    format!(
                        "invalid cron schedule for job `{}`: {}",
                        cron_job.id, schedule
                    )
                })?
            } else {
                // System-local scheduling (no timezone).
                Job::new_async(schedule.as_str(), move |_uuid, _scheduler| {
                    let job = job_clone.clone();
                    let agents = Arc::clone(&agents);
                    let channels = Arc::clone(&channels);
                    let run_log_dir = run_log_dir.clone();
                    let sem = Arc::clone(&sem);
                    let default_delivery = default_delivery.clone();
                    Box::pin(async move {
                        let Ok(_permit) = sem.acquire().await else {
                            return;
                        };
                        info!(job_id = %job.id, "cron job triggered");
                        let result = run_cron_job(&job, &agents).await;
                        match &result {
                            Ok(output) => {
                                // Send delivery notification if configured
                                if let Err(e) = send_delivery(&channels, &job, &default_delivery, output).await {
                                    warn!(job_id = %job.id, %e, "delivery failed");
                                }
                            }
                            Err(e) => {
                                error!(job_id = %job.id, %e, "cron job failed");
                            }
                        }
                        let entry = build_run_log_entry(&job, result.is_ok(), result.as_ref().err().map(|e| anyhow::anyhow!("{e}")));
                        let _ = write_run_log(&run_log_dir, &job.id, entry).await;
                    })
                })
                .with_context(|| {
                    format!(
                        "invalid cron schedule for job `{}`: {}",
                        cron_job.id, schedule
                    )
                })?
            };

            scheduler
                .add(tokio_job)
                .await
                .with_context(|| format!("failed to schedule job `{}`", cron_job.id))?;

            let label = cron_job.name.as_deref().unwrap_or(&cron_job.id);
            let tz_info = cron_job.timezone().unwrap_or("local");
            info!(job_id = %cron_job.id, name = label, tz = tz_info, "cron job scheduled");
        }

        scheduler
            .start()
            .await
            .context("failed to start cron scheduler")?;

        info!("cron scheduler started with {} job(s)", self.jobs.len());

        tokio::signal::ctrl_c().await?;

        info!("cron scheduler shutting down");
        scheduler
            .shutdown()
            .await
            .context("error during cron scheduler shutdown")?;

        Ok(())
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
            // Send delivery notification if configured
            if let Err(e) = send_delivery(&self.channels, job, &self.default_delivery, output).await {
                warn!(job_id = %job.id, %e, "delivery failed");
            }
        }
        // Re-create an equivalent error for the log entry (result is consumed by `?`
        // below).
        let log_err = if success {
            None
        } else {
            result.as_ref().err().map(|e| anyhow::anyhow!("{e:#}"))
        };
        let entry = build_run_log_entry(job, success, log_err);
        write_run_log(&self.run_log_dir, &job.id, entry).await?;
        result.map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert a 5-field cron expression to 6-field by prepending a seconds field.
/// If the expression already has 6+ fields it is returned unchanged.
fn to_six_field(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields >= 6 {
        expr.to_string()
    } else {
        format!("0 {expr}")
    }
}

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

    // Wait for the reply with a generous timeout.
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

/// Send delivery notification for a completed cron job.
/// Uses job.delivery if present, otherwise falls back to default_delivery from config.
async fn send_delivery(
    channels: &ChannelManager,
    job: &CronJob,
    default_delivery: &Option<CronDelivery>,
    output_text: &str,
) -> Result<()> {
    // Use job-specific delivery if present, otherwise fall back to default
    let delivery = match &job.delivery {
        Some(d) => d,
        None => match default_delivery {
            Some(d) => d,
            None => {
                info!(job_id = %job.id, name = ?job.name, "cron job completed but no delivery configured (job or default) - notification not sent");
                return Ok(());
            }
        },
    };

    // Check if delivery is enabled
    let mode = delivery.mode.as_deref().unwrap_or("none");
    if mode == "none" {
        info!(job_id = %job.id, mode = %mode, "delivery disabled, skipping notification");
        return Ok(());
    }

    let channel_name = match &delivery.channel {
        Some(c) => c,
        None => {
            warn!(job_id = %job.id, "delivery configured but no channel specified");
            return Ok(());
        }
    };

    let to = match &delivery.to {
        Some(t) => t,
        None => {
            warn!(job_id = %job.id, "delivery configured but no 'to' specified");
            return Ok(());
        }
    };

    // Skip empty output
    let text = output_text.trim();
    if text.is_empty() {
        info!(job_id = %job.id, "skipping delivery for empty output");
        return Ok(());
    }

    // Get the channel from manager
    let channel = match channels.get(channel_name) {
        Some(ch) => ch,
        None => {
            warn!(job_id = %job.id, channel = %channel_name, "channel not found for delivery");
            return Ok(());
        }
    };

    // Build outbound message
    let msg = OutboundMessage {
        target_id: to.clone(),
        is_group: false, // Cron delivery typically targets a specific user/chat
        text: text.to_owned(),
        reply_to: delivery.thread_id.clone(),
        images: vec![],
        channel: Some(channel_name.to_owned()),
    };

    // Send the message through the channel
    info!(
        job_id = %job.id,
        channel = %channel_name,
        to = %to,
        thread_id = ?delivery.thread_id,
        text_len = text.len(),
        "[CRON DELIVERY] Sending notification"
    );

    match channel.send(msg).await {
        Ok(()) => {
            info!(job_id = %job.id, channel = %channel_name, to = %to, "[CRON DELIVERY] Notification sent successfully");
            Ok(())
        }
        Err(e) => {
            if delivery.best_effort.unwrap_or(false) {
                warn!(job_id = %job.id, channel = %channel_name, to = %to, error = %e, "[CRON DELIVERY] Send failed (best_effort=true, ignoring)");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn build_run_log_entry(job: &CronJob, success: bool, error: Option<anyhow::Error>) -> RunLogEntry {
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
