//! Background worker that drives `ExternalJob` rows to completion and
//! delivers the resulting artifact through the standard channel
//! notification path.
//!
//! Design lives in `external_jobs.rs`. This file holds the runtime loop
//! plus per-provider HTTP adapters (`submit_*` and internal `poll_*`).
//! Adding a new async provider means: extend the `match` in
//! `dispatch_poll`, add the corresponding `submit_*` for the tool side,
//! and add the URL → Done outcome mapping.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use super::external_jobs::{
    ExternalJob, ExternalJobKind, ExternalJobStatus, PollOutcome,
};
use super::shutdown::ShutdownCoordinator;
use crate::channel::OutboundMessage;
use crate::config::runtime::RuntimeConfig;
use crate::store::RedbStore;

/// Seconds between worker ticks when nothing is due — small enough that
/// new jobs start polling promptly, large enough to keep redb scans cheap.
const TICK_SECS: u64 = 5;

/// Retention window for terminal jobs before they get GC'd.
const FINISHED_RETENTION_SECS: i64 = 24 * 3600;

pub struct ExternalJobsWorker {
    store: Arc<RedbStore>,
    notification_tx: broadcast::Sender<OutboundMessage>,
    shutdown: ShutdownCoordinator,
    config: Arc<RuntimeConfig>,
    client: reqwest::Client,
}

impl ExternalJobsWorker {
    pub fn new(
        store: Arc<RedbStore>,
        notification_tx: broadcast::Sender<OutboundMessage>,
        shutdown: ShutdownCoordinator,
        config: Arc<RuntimeConfig>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            store,
            notification_tx,
            shutdown,
            config,
            client,
        }
    }

    /// Main loop. Each tick: list due jobs, poll each in a spawned task,
    /// GC finished rows.
    pub async fn run(self: Arc<Self>) {
        info!("external jobs worker started");
        let mut gc_counter: u32 = 0;
        loop {
            if self.shutdown.is_draining() {
                info!("external jobs worker: drain signaled, stopping");
                break;
            }
            let now = chrono::Utc::now().timestamp();
            match self.store.due_external_jobs(now) {
                Ok(jobs) if !jobs.is_empty() => {
                    debug!(count = jobs.len(), "external jobs: due tick");
                    for job in jobs {
                        let worker = Arc::clone(&self);
                        let guard = self.shutdown.begin_work();
                        tokio::spawn(async move {
                            worker.process_job(job).await;
                            drop(guard);
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => error!("external jobs: due query failed: {e:#}"),
            }

            // GC every ~12 ticks (~1 minute) — terminal rows older than the
            // retention window get dropped.
            gc_counter = gc_counter.wrapping_add(1);
            if gc_counter % 12 == 0 {
                if let Err(e) = self.store.cleanup_finished_external_jobs(FINISHED_RETENTION_SECS) {
                    warn!("external jobs: cleanup_finished failed: {e:#}");
                }
            }

            tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
        }
        info!("external jobs worker exited");
    }

    /// Process one job: hard-timeout sweep → poll → record outcome →
    /// deliver if terminal.
    async fn process_job(&self, mut job: ExternalJob) {
        if job.is_timed_out() {
            job.status = ExternalJobStatus::TimedOut;
            job.error = Some(format!(
                "timed out after {}s",
                chrono::Utc::now().timestamp() - job.submitted_at
            ));
            if let Err(e) = self.store.update_external_job(&job) {
                error!(job_id = %job.id, "update failed: {e:#}");
            }
            self.deliver_failure(&job).await;
            return;
        }

        // Mark `Polling` so a concurrent restart sweep sees it as in-flight.
        job.status = ExternalJobStatus::Polling;
        job.poll_count += 1;
        if let Err(e) = self.store.update_external_job(&job) {
            error!(job_id = %job.id, "update (polling) failed: {e:#}");
            return;
        }

        let outcome = self.dispatch_poll(&job).await;
        match outcome {
            Ok(PollOutcome::Pending) => {
                let now = chrono::Utc::now().timestamp();
                job.next_poll_at = now + job.next_poll_delay_secs() as i64;
                job.status = ExternalJobStatus::Pending;
                job.error = None;
                if let Err(e) = self.store.update_external_job(&job) {
                    error!(job_id = %job.id, "update (pending) failed: {e:#}");
                }
            }
            Ok(PollOutcome::Done(url)) => {
                job.result_url = Some(url.clone());
                match download_artifact(&self.client, &url, job.kind).await {
                    Ok(local_path) => {
                        job.result_path = Some(local_path);
                        job.status = ExternalJobStatus::Done;
                        job.error = None;
                        if let Err(e) = self.store.update_external_job(&job) {
                            error!(job_id = %job.id, "update (done) failed: {e:#}");
                        }
                        self.deliver_success(&job).await;
                    }
                    Err(e) => {
                        job.status = ExternalJobStatus::Failed;
                        job.error = Some(format!("download: {e:#}"));
                        if let Err(e2) = self.store.update_external_job(&job) {
                            error!(job_id = %job.id, "update (download-fail) failed: {e2:#}");
                        }
                        self.deliver_failure(&job).await;
                    }
                }
            }
            Ok(PollOutcome::Failed(msg)) => {
                job.status = ExternalJobStatus::Failed;
                job.error = Some(msg);
                if let Err(e) = self.store.update_external_job(&job) {
                    error!(job_id = %job.id, "update (failed) failed: {e:#}");
                }
                self.deliver_failure(&job).await;
            }
            Err(e) => {
                // Transient error — schedule next poll, keep job alive.
                let now = chrono::Utc::now().timestamp();
                job.next_poll_at = now + job.next_poll_delay_secs() as i64;
                job.status = ExternalJobStatus::Pending;
                job.error = Some(format!("poll: {e:#}"));
                warn!(job_id = %job.id, error = %e, "external jobs: transient poll error");
                if let Err(e2) = self.store.update_external_job(&job) {
                    error!(job_id = %job.id, "update (transient) failed: {e2:#}");
                }
            }
        }
    }

    /// Pick the right per-provider polling adapter.
    async fn dispatch_poll(&self, job: &ExternalJob) -> Result<PollOutcome> {
        match job.provider.as_str() {
            "seedance" => {
                let key = self
                    .seedance_key()
                    .ok_or_else(|| anyhow!("seedance: no API key configured"))?;
                poll_seedance(&self.client, &key, &job.external_task_id).await
            }
            other => Err(anyhow!("no async polling adapter for provider: {other}")),
        }
    }

    fn seedance_key(&self) -> Option<String> {
        self.config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get("doubao"))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.as_plain().map(str::to_owned))
            .or_else(|| std::env::var("ARK_API_KEY").ok())
    }

    async fn deliver_success(&self, job: &ExternalJob) {
        let path = job.result_path.as_deref().unwrap_or("");
        let kind_label = match job.kind {
            ExternalJobKind::VideoGen => "video",
            ExternalJobKind::ImageGen => "image",
        };
        let filename = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path);
        let mime = match job.kind {
            ExternalJobKind::VideoGen => "video/mp4",
            ExternalJobKind::ImageGen => "image/png",
        };
        let prompt_preview: String = job.prompt.chars().take(80).collect();
        let out = OutboundMessage {
            target_id: job.delivery.target_id.clone(),
            is_group: job.delivery.is_group,
            text: format!("[{kind_label}] {prompt_preview}"),
            reply_to: job.delivery.reply_to.clone(),
            images: vec![],
            files: vec![(filename.to_string(), mime.to_string(), path.to_string())],
            channel: Some(job.delivery.channel.clone()),
        };
        if let Err(e) = self.notification_tx.send(out) {
            error!(job_id = %job.id, "deliver_success: notification_tx failed: {e}");
        }
    }

    async fn deliver_failure(&self, job: &ExternalJob) {
        let kind_label = match job.kind {
            ExternalJobKind::VideoGen => "video",
            ExternalJobKind::ImageGen => "image",
        };
        let reason = job.error.as_deref().unwrap_or("unknown error");
        let prompt_preview: String = job.prompt.chars().take(80).collect();
        let out = OutboundMessage {
            target_id: job.delivery.target_id.clone(),
            is_group: job.delivery.is_group,
            text: format!("[{kind_label}] generation failed for: {prompt_preview}\n{reason}"),
            reply_to: job.delivery.reply_to.clone(),
            images: vec![],
            files: vec![],
            channel: Some(job.delivery.channel.clone()),
        };
        if let Err(e) = self.notification_tx.send(out) {
            error!(job_id = %job.id, "deliver_failure: notification_tx failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Seedance (ByteDance ARK) — async submit + poll
// ---------------------------------------------------------------------------

const SEEDANCE_BASE: &str = "https://ark.cn-beijing.volces.com/api/v3";
const SEEDANCE_DEFAULT_MODEL: &str = "doubao-seedance-2-0-260128";

/// Submit a Seedance video generation task and return the provider's
/// `task_id`. The caller is responsible for persisting an `ExternalJob`
/// referencing this id so the worker can pick up polling.
pub async fn submit_seedance(
    client: &reqwest::Client,
    api_key: &str,
    prompt: &str,
    duration: u64,
    aspect_ratio: &str,
    model_override: Option<&str>,
) -> Result<String> {
    let model = model_override.unwrap_or(SEEDANCE_DEFAULT_MODEL);
    let body = json!({
        "model": model,
        "content": [{"type": "text", "text": prompt}],
        "ratio": aspect_ratio,
        "duration": duration,
        "watermark": false,
    });
    let resp: serde_json::Value = client
        .post(format!("{SEEDANCE_BASE}/contents/generations/tasks"))
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("seedance: submit failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("seedance: submit parse failed: {e}"))?;
    let task_id = resp["id"]
        .as_str()
        .ok_or_else(|| anyhow!("seedance: no task id in response: {resp}"))?
        .to_owned();
    Ok(task_id)
}

async fn poll_seedance(
    client: &reqwest::Client,
    api_key: &str,
    task_id: &str,
) -> Result<PollOutcome> {
    let resp: serde_json::Value = client
        .get(format!("{SEEDANCE_BASE}/contents/generations/tasks/{task_id}"))
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| anyhow!("seedance: poll failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow!("seedance: poll parse failed: {e}"))?;
    let status = resp["status"].as_str().unwrap_or("unknown");
    match status {
        "succeeded" => {
            let url = resp
                .pointer("/content/video_url")
                .or_else(|| resp.pointer("/content/0/video_url/url"))
                .or_else(|| resp.pointer("/content/0/url"))
                .or_else(|| resp.pointer("/result/video_url/url"))
                .or_else(|| resp.pointer("/output/url"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("seedance: no video URL in result: {resp}"))?
                .to_owned();
            Ok(PollOutcome::Done(url))
        }
        "failed" | "cancelled" => {
            let msg = resp["error"]["message"]
                .as_str()
                .or_else(|| resp["message"].as_str())
                .unwrap_or("task failed");
            Ok(PollOutcome::Failed(format!("{status}: {msg}")))
        }
        _ => Ok(PollOutcome::Pending),
    }
}

// ---------------------------------------------------------------------------
// Artifact download
// ---------------------------------------------------------------------------

/// Download the provider URL into `~/Downloads/rsclaw/<category>/dl_<X>_<ts><abc>.<ext>`
/// using the same canonical naming as the synchronous tool path. Returns
/// the absolute local path.
async fn download_artifact(
    client: &reqwest::Client,
    url: &str,
    kind: ExternalJobKind,
) -> Result<String> {
    let bytes = client
        .get(url)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|e| anyhow!("download: {e}"))?
        .bytes()
        .await
        .map_err(|e| anyhow!("download read: {e}"))?;
    let ext = match kind {
        ExternalJobKind::VideoGen => "mp4",
        ExternalJobKind::ImageGen => "png",
    };
    let kind_letter = crate::channel::kind_from_extension(ext);
    let category = crate::channel::category_for_kind(kind_letter);
    let dir = dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(crate::config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw")
        .join(category);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| anyhow!("download: create_dir: {e}"))?;
    let ts = chrono::Local::now().format("%Y%m%d%H%M").to_string();
    let abc: String = (0..3)
        .map(|_| (rand::random::<u8>() % 26 + b'a') as char)
        .collect();
    let path = dir.join(format!("dl_{kind_letter}_{ts}{abc}.{ext}"));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| anyhow!("download: write: {e}"))?;
    Ok(path.to_string_lossy().to_string())
}
