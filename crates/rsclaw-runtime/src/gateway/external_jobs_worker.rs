//! Background worker that drives `ExternalJob` rows to completion and
//! delivers the resulting artifact through the standard channel
//! notification path.
//!
//! Design lives in `external_jobs.rs`. This file holds the runtime loop
//! plus per-provider HTTP adapters (`submit_*` and internal `poll_*`).
//! Adding a new async provider means: extend the `match` in
//! `dispatch_poll`, add the corresponding `submit_*` for the tool side,
//! and add the URL → Done outcome mapping.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, anyhow};
use rsclaw_channel::OutboundMessage;
use rsclaw_config::runtime::RuntimeConfig;
use rsclaw_store::RedbStore;
use serde_json::json;
use tokio::sync::{Semaphore, broadcast};
use tracing::{debug, error, info, warn};

use super::{
    external_jobs::{ExternalJob, ExternalJobKind, ExternalJobStatus, PollOutcome},
    shutdown::ShutdownCoordinator,
    task_queue::{Priority, QueuedMessage},
};

/// Seconds between worker ticks when nothing is due — small enough that
/// new jobs start polling promptly, large enough to keep redb scans cheap.
const TICK_SECS: u64 = 5;

/// Retention window for terminal jobs before they get GC'd.
const FINISHED_RETENTION_SECS: i64 = 24 * 3600;

/// Concurrent in-flight provider HTTP calls (poll + delivery). Caps the
/// thundering-herd risk after a long restart window when many jobs become
/// due simultaneously. Provider rate limits typically tolerate this.
const MAX_CONCURRENT_OPS: usize = 8;

/// Back-off seconds between failed delivery attempts. Constant for now —
/// `notification_tx.send` failures are nearly always "no live receivers"
/// during a brief startup gap, not provider issues that need exponential
/// back-off.
const DELIVERY_RETRY_DELAY_SECS: u64 = 30;

/// Lease stamped into `next_poll_at` when a job is claimed for a cycle, so
/// later ticks don't re-select it while a long poll / download is running.
/// Every successful cycle overwrites `next_poll_at` with the real schedule;
/// the lease only matters if the process dies mid-cycle.
const CLAIM_LEASE_SECS: i64 = 600;

pub struct ExternalJobsWorker {
    store: Arc<RedbStore>,
    notification_tx: broadcast::Sender<OutboundMessage>,
    /// Session event bus (`/api/v1/stream`). A finished artifact is also
    /// pushed here so clients that chat over the sync-only `api` channel,
    /// where the notification router drops deliveries, still receive it.
    event_tx: Option<broadcast::Sender<rsclaw_events::AgentEvent>>,
    shutdown: ShutdownCoordinator,
    config: Arc<RuntimeConfig>,
    client: reqwest::Client,
    /// Cap on concurrent per-job operations — see MAX_CONCURRENT_OPS.
    op_semaphore: Arc<Semaphore>,
    /// Job ids currently being processed by a spawned task. Guards against
    /// re-spawning the same job on the next tick.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

/// Removes a job id from the in-flight set when the processing task ends.
struct InFlightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut set = self.set.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(&self.id);
    }
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
            event_tx: None,
            shutdown,
            config,
            client,
            op_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_OPS)),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Also push finished artifacts onto the session event bus.
    pub fn with_event_bus(
        mut self,
        event_tx: broadcast::Sender<rsclaw_events::AgentEvent>,
    ) -> Self {
        self.event_tx = Some(event_tx);
        self
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
                    for mut job in jobs {
                        // Skip jobs a previous tick already spawned.
                        {
                            let set = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
                            if set.contains(&job.id) {
                                continue;
                            }
                        }
                        // Don't queue waiters behind a saturated semaphore;
                        // remaining due jobs get picked up on a later tick.
                        let permit = match Arc::clone(&self.op_semaphore).try_acquire_owned() {
                            Ok(p) => p,
                            Err(_) => {
                                debug!("external jobs: all op slots busy, deferring rest of tick");
                                break;
                            }
                        };
                        // Claim: push next_poll_at out by a lease so the row
                        // isn't re-selected while this cycle runs.
                        job.next_poll_at = now + CLAIM_LEASE_SECS;
                        if let Err(e) = self.store.update_external_job(&job) {
                            error!(job_id = %job.id, "external jobs: claim failed: {e:#}");
                            continue;
                        }
                        self.in_flight
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(job.id.clone());
                        let in_flight = InFlightGuard {
                            set: Arc::clone(&self.in_flight),
                            id: job.id.clone(),
                        };
                        let worker = Arc::clone(&self);
                        let guard = self.shutdown.begin_work();
                        tokio::spawn(async move {
                            let _permit = permit;
                            let _in_flight = in_flight;
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
                if let Err(e) = self
                    .store
                    .cleanup_finished_external_jobs(FINISHED_RETENTION_SECS)
                {
                    warn!("external jobs: cleanup_finished failed: {e:#}");
                }
            }

            tokio::time::sleep(Duration::from_secs(TICK_SECS)).await;
        }
        info!("external jobs worker exited");
    }

    /// Decide whether the job needs a polling cycle (still in flight) or
    /// a delivery retry (already terminal but `notification_tx.send`
    /// failed last time), then dispatch.
    async fn process_job(&self, job: ExternalJob) {
        if job.needs_delivery() {
            self.retry_delivery(job).await;
        } else if matches!(
            job.status,
            ExternalJobStatus::Pending | ExternalJobStatus::Polling
        ) {
            self.poll_cycle(job).await;
        }
        // Else: row is in a state we shouldn't be acting on (e.g. already
        // delivered terminal). due_external_jobs already filtered, so
        // hitting this branch means a concurrent state change — skip.
    }

    /// Full poll cycle: timeout sweep → poll → on terminal, attempt first
    /// delivery. If delivery fails, the row stays in a terminal status
    /// with `delivered_at = None` and gets picked up by `retry_delivery`
    /// on later ticks.
    async fn poll_cycle(&self, mut job: ExternalJob) {
        if job.is_timed_out() {
            job.status = ExternalJobStatus::TimedOut;
            job.error = Some(format!(
                "timed out after {}s",
                chrono::Utc::now().timestamp() - job.submitted_at
            ));
            if let Err(e) = self.store.update_external_job(&job) {
                error!(job_id = %job.id, "update failed: {e:#}");
            }
            self.attempt_delivery(&mut job).await;
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
            Ok(PollOutcome::InProgress(progress)) => {
                let now = chrono::Utc::now().timestamp();
                job.next_poll_at = now + job.next_poll_delay_secs() as i64;
                job.status = ExternalJobStatus::Pending;
                job.error = None;
                if let Some(progress) = progress {
                    job.progress = Some(progress);
                }
                if let Err(e) = self.store.update_external_job(&job) {
                    error!(job_id = %job.id, "update (in-progress) failed: {e:#}");
                }
            }
            Ok(PollOutcome::Done(url)) => {
                job.progress = Some(1.0);
                job.result_url = Some(url.clone());
                // Native jobs and current legacy responses return signed URLs;
                // never send Bearer to those URLs. Only a legacy fallback to a
                // bare `/content` endpoint needs the authenticated downloader.
                let signed = url.contains("exp=") && url.contains("sig=");
                let dl = if job.provider == "rsclaw_legacy" && !signed {
                    let key = self
                        .resolve_provider_key("rsclaw", "RSCLAW_API_KEY")
                        .unwrap_or_default();
                    rsclaw_jobs::download_artifact_authed(&url, &key, job.kind).await
                } else {
                    rsclaw_jobs::download_artifact(&self.client, &url, job.kind).await
                };
                match dl {
                    Ok(local_path) => {
                        job.result_path = Some(local_path);
                        job.status = ExternalJobStatus::Done;
                        job.error = None;
                        if let Err(e) = self.store.update_external_job(&job) {
                            error!(job_id = %job.id, "update (done) failed: {e:#}");
                        }
                        self.attempt_delivery(&mut job).await;
                    }
                    Err(e) => {
                        job.status = ExternalJobStatus::Failed;
                        job.error = Some(format!("download: {e:#}"));
                        if let Err(e2) = self.store.update_external_job(&job) {
                            error!(job_id = %job.id, "update (download-fail) failed: {e2:#}");
                        }
                        self.attempt_delivery(&mut job).await;
                    }
                }
            }
            Ok(PollOutcome::Failed(msg)) => {
                job.status = ExternalJobStatus::Failed;
                job.error = Some(msg);
                if let Err(e) = self.store.update_external_job(&job) {
                    error!(job_id = %job.id, "update (failed) failed: {e:#}");
                }
                self.attempt_delivery(&mut job).await;
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

    /// Re-attempt delivery for a terminal job whose previous delivery
    /// attempt failed. Reuses the same `attempt_delivery` path so the
    /// success / failure bookkeeping is identical.
    async fn retry_delivery(&self, mut job: ExternalJob) {
        info!(
            job_id = %job.id,
            attempts = job.delivery_attempts,
            "external jobs: retrying delivery"
        );
        self.attempt_delivery(&mut job).await;
    }

    /// Try to push the artifact (or failure notice) through
    /// `notification_tx`. On success stamp `delivered_at` and write the
    /// outcome to session history; on failure schedule a retry by
    /// bumping `next_poll_at`.
    async fn attempt_delivery(&self, job: &mut ExternalJob) {
        job.delivery_attempts = job.delivery_attempts.saturating_add(1);
        let success = if matches!(job.status, ExternalJobStatus::Done) {
            self.deliver_success(job).await
        } else {
            self.deliver_failure(job).await
        };

        if success {
            job.delivered_at = Some(chrono::Utc::now().timestamp());
            if let Err(e) = self.store.update_external_job(job) {
                error!(job_id = %job.id, "update (delivered) failed: {e:#}");
            }
            // Best-effort session-history writeback so the agent sees the
            // result on the next turn ("hey, your video from earlier...").
            // Failures here don't affect the user's actual delivery.
            if let Err(e) = self.write_back_to_session(job) {
                debug!(job_id = %job.id, "session writeback skipped: {e:#}");
            }
            self.emit_session_event(job);
            // Runs once per job: this branch is only reached while
            // `delivered_at` was still unset.
            wake_session(job);
        } else {
            let now = chrono::Utc::now().timestamp();
            job.next_poll_at = now + DELIVERY_RETRY_DELAY_SECS as i64;
            warn!(
                job_id = %job.id,
                attempts = job.delivery_attempts,
                "external jobs: delivery failed, will retry"
            );
            if let Err(e) = self.store.update_external_job(job) {
                error!(job_id = %job.id, "update (delivery-retry) failed: {e:#}");
            }
        }
    }

    /// Append a synthetic assistant message to the original session so the
    /// agent's next turn knows the artifact landed. Best-effort — the
    /// channel-level delivery is the source of truth for the user.
    fn write_back_to_session(&self, job: &ExternalJob) -> Result<()> {
        let mut msg = json!({
            "role": "assistant",
            "content": writeback_text(job),
            "external_job_id": job.id,
            "sent_at": chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
        });
        // Same attachments as the live delivery, so a client reloading the
        // history renders the image or video rather than the sentence.
        let files = result_files(job);
        if !files.is_empty() {
            msg["files"] = rsclaw_events::media::files_json(&files);
        }
        self.store
            .append_message(&job.session_key, &msg)
            .map(|_| ())
            .map_err(|e| anyhow!("append_message: {e}"))
    }

    /// Push the finished artifact to `/api/v1/stream` subscribers of the
    /// job's session. `done` stays false: a done event would end a turn that
    /// is streaming in the same session right now.
    fn emit_session_event(&self, job: &ExternalJob) {
        let Some(tx) = &self.event_tx else { return };
        let files = result_files(job);
        if files.is_empty() {
            return;
        }
        let _ = tx.send(rsclaw_events::AgentEvent {
            session_id: job.session_key.clone(),
            files,
            ..Default::default()
        });
    }

    /// Pick the right per-provider polling adapter.
    async fn dispatch_poll(&self, job: &ExternalJob) -> Result<PollOutcome> {
        match job.provider.as_str() {
            "seedance" => {
                let key = self
                    .resolve_provider_key("doubao", "ARK_API_KEY")
                    .ok_or_else(|| anyhow!("seedance: no API key configured"))?;
                rsclaw_jobs::poll_seedance(&self.client, &key, &job.external_task_id).await
            }
            "minimax" => {
                let key = self
                    .resolve_provider_key("minimax", "MINIMAX_API_KEY")
                    .ok_or_else(|| anyhow!("minimax: no API key configured"))?;
                rsclaw_jobs::poll_minimax(&self.client, &key, &job.external_task_id).await
            }
            "kling" => {
                let (ak, sk) = self.resolve_kling_keys().ok_or_else(|| {
                    anyhow!("kling: KLING_ACCESS_KEY / KLING_SECRET_KEY not configured")
                })?;
                rsclaw_jobs::poll_kling(&self.client, &ak, &sk, &job.external_task_id).await
            }
            "rsclaw_native" => {
                let key = self
                    .resolve_provider_key("rsclaw", "RSCLAW_API_KEY")
                    .ok_or_else(|| anyhow!("rsclaw: no API key configured"))?;
                rsclaw_jobs::poll_rsclaw_native(&key, &job.external_task_id).await
            }
            "rsclaw_legacy" => {
                let key = self
                    .resolve_provider_key("rsclaw", "RSCLAW_API_KEY")
                    .ok_or_else(|| anyhow!("rsclaw: no API key configured"))?;
                rsclaw_jobs::poll_rsclaw_legacy(&key, &job.external_task_id).await
            }
            "agnes" => {
                let key = self
                    .resolve_provider_key("agnes", "AGNES_API_KEY")
                    .ok_or_else(|| anyhow!("agnes: no API key configured"))?;
                rsclaw_jobs::poll_agnes(&self.client, &key, &job.external_task_id).await
            }
            "openai" => {
                let key = self
                    .resolve_provider_key("openai", "OPENAI_API_KEY")
                    .ok_or_else(|| anyhow!("openai: no API key configured"))?;
                let base = self.resolve_provider_base_url("openai");
                rsclaw_jobs::poll_openai_video(&self.client, &base, &key, &job.external_task_id)
                    .await
            }
            // Async rsclaw image (image-edit / t2i-v2). external_task_id is the
            // signed, authless poll URL; no provider key needed.
            "rsclaw_image" => {
                rsclaw_jobs::poll_rsclaw_image(&self.client, &job.external_task_id).await
            }
            other => Err(anyhow!("no async polling adapter for provider: {other}")),
        }
    }

    /// Resolve a provider's configured `base_url`, falling back to the builtin
    /// default for that provider (e.g. openai → https://api.openai.com/v1).
    fn resolve_provider_base_url(&self, provider: &str) -> String {
        self.config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(provider))
            .and_then(|p| p.base_url.clone())
            .unwrap_or_else(|| rsclaw_provider::defaults::resolve_base_url(provider).0)
    }

    /// Resolve a single-bearer-token provider key from rsclaw.json5
    /// (`models.providers.<name>.api_key`) with env-var fallback.
    fn resolve_provider_key(&self, provider: &str, env_var: &str) -> Option<String> {
        self.config
            .model
            .models
            .as_ref()
            .and_then(|m| m.providers.get(provider))
            .and_then(|p| p.api_key.as_ref())
            .and_then(|k| k.resolve_full(self.config.ops.secrets.as_ref()))
            .or_else(|| std::env::var(env_var).ok())
    }

    /// Resolve the Kling access/secret pair. Both must be present, otherwise
    /// `None` so the caller can fail loudly rather than build a bad JWT.
    fn resolve_kling_keys(&self) -> Option<(String, String)> {
        let ak = self.resolve_provider_key("kling", "KLING_ACCESS_KEY")?;
        let sk = std::env::var("KLING_SECRET_KEY").ok()?;
        Some((ak, sk))
    }

    /// Returns true iff the broadcast accepted the message. The caller
    /// (`attempt_delivery`) uses the boolean to decide whether to stamp
    /// `delivered_at` or schedule another retry tick.
    async fn deliver_success(&self, job: &ExternalJob) -> bool {
        let kind_label = match job.kind {
            ExternalJobKind::VideoGen => "video",
            ExternalJobKind::ImageGen => "image",
        };
        let prompt_preview: String = job.prompt.chars().take(80).collect();
        let out = OutboundMessage {
            target_id: job.delivery.target_id.clone(),
            is_group: job.delivery.is_group,
            text: format!("[{kind_label}] {prompt_preview}"),
            reply_to: job.delivery.reply_to.clone(),
            images: vec![],
            files: result_files(job),
            channel: Some(job.delivery.channel.clone()),
            account: job.delivery.account.clone(),
        };
        match self.notification_tx.send(out) {
            Ok(_) => true,
            Err(e) => {
                warn!(job_id = %job.id, "deliver_success: notification_tx failed: {e}");
                false
            }
        }
    }

    async fn deliver_failure(&self, job: &ExternalJob) -> bool {
        let kind_label = match job.kind {
            ExternalJobKind::VideoGen => "video",
            ExternalJobKind::ImageGen => "image",
        };
        let reason = job.error.as_deref().unwrap_or("unknown error");
        let prompt_preview: String = job.prompt.chars().take(80).collect();
        let text = format!("[{kind_label} failed] {prompt_preview}: {reason}");
        let out = OutboundMessage {
            target_id: job.delivery.target_id.clone(),
            is_group: job.delivery.is_group,
            text,
            reply_to: job.delivery.reply_to.clone(),
            images: vec![],
            files: vec![],
            channel: Some(job.delivery.channel.clone()),
            account: job.delivery.account.clone(),
        };
        match self.notification_tx.send(out) {
            Ok(_) => true,
            Err(e) => {
                warn!(job_id = %job.id, "deliver_failure: notification_tx failed: {e}");
                false
            }
        }
    }
}

/// The history line for a finished job: one sentence the model can act on
/// in a later turn, with the path to hand to the next tool and the `@token`
/// the user can refer to. Clients render the attachment from `files`.
fn writeback_text(job: &ExternalJob) -> String {
    let kind = match job.kind {
        ExternalJobKind::VideoGen => "video",
        ExternalJobKind::ImageGen => "image",
    };
    if !matches!(job.status, ExternalJobStatus::Done) {
        let status = match job.status {
            ExternalJobStatus::TimedOut => "timed out",
            _ => "failed",
        };
        let reason = job.error.as_deref().unwrap_or("unknown error");
        return format!("The {kind} generation {status}: {reason}");
    }
    let path = job.result_path.as_deref().unwrap_or("");
    match rsclaw_events::media::token_for_path(path) {
        Some(token) => format!("The {kind} is ready at {path} (@{token})."),
        None => format!("The {kind} is ready at {path}."),
    }
}

/// Channels whose sessions get no follow-up turn when a job ends: a
/// scheduled run fires again on its own schedule, and heartbeat / system
/// turns have no conversation to continue.
const NO_WAKE_CHANNELS: &[&str] = &["cron", "heartbeat", "system"];

fn should_wake(job: &ExternalJob) -> bool {
    !NO_WAKE_CHANNELS.contains(&job.delivery.channel.as_str())
        && !job.session_key.starts_with("cron:")
}

/// Queue one turn on the job's session so the agent can carry a multi-step
/// request on ("make an image, then a video from it") instead of waiting
/// for the user to speak again. Best-effort, like the history writeback.
fn wake_session(job: &ExternalJob) {
    if !should_wake(job) {
        return;
    }
    let Some(tq) = super::task_queue::get_task_queue() else {
        debug!(job_id = %job.id, "external jobs: task queue not installed, no follow-up turn");
        return;
    };
    // Desktop chats run on the `ws` channel but reply through the `desktop`
    // sender, the same mapping `/goal` uses.
    let channel = match job.delivery.channel.as_str() {
        "ws" => "desktop",
        other => other,
    };
    let message = QueuedMessage {
        text: wake_text(job),
        sender: job.delivery.target_id.clone(),
        channel: channel.to_owned(),
        account: job.delivery.account.clone(),
        chat_id: job.delivery.target_id.clone(),
        is_group: job.delivery.is_group,
        reply_to: None,
        timestamp: chrono::Utc::now().timestamp(),
        images: vec![],
        files: vec![],
    };
    match tq.submit(&job.session_key, message, Priority::System) {
        Ok((task_id, merged)) => info!(
            job_id = %job.id,
            session = %job.session_key,
            %task_id,
            merged,
            "external jobs: follow-up turn queued"
        ),
        Err(e) => warn!(job_id = %job.id, "external jobs: follow-up turn not queued: {e:#}"),
    }
}

/// The notice the agent wakes up to. It asks for NO_REPLY when nothing is
/// left, which the runtime turns into an empty reply that never reaches the
/// user. A failure must not be resubmitted, or a job that keeps failing
/// would wake the agent forever.
fn wake_text(job: &ExternalJob) -> String {
    let kind = match job.kind {
        ExternalJobKind::VideoGen => "video",
        ExternalJobKind::ImageGen => "image",
    };
    let id = &job.id;
    if matches!(job.status, ExternalJobStatus::Done) {
        let path = job.result_path.as_deref().unwrap_or("");
        let notice = rsclaw_events::notice::AUTO_NOTICE;
        format!(
            "[{kind} generation complete] job {id}: {path}\n\
             {notice} The user has \
             already received the {kind}. If the request you were working on has steps \
             that need it (for example making a video from this image, or publishing \
             it), carry them out now using the path above. Do not resend or describe \
             the {kind}. If nothing is left to do, or you already used this result, \
             reply with exactly NO_REPLY."
        )
    } else {
        let status = match job.status {
            ExternalJobStatus::TimedOut => "timed out",
            _ => "failed",
        };
        let reason = job.error.as_deref().unwrap_or("unknown error");
        let notice = rsclaw_events::notice::AUTO_NOTICE;
        format!(
            "[{kind} generation {status}] job {id}: {reason}\n\
             {notice} The user has \
             already been told. Do not resubmit this job. If the request you were \
             working on can still finish without it, finish it; if the user needs to \
             decide what to do next, ask them briefly. Otherwise reply with exactly \
             NO_REPLY."
        )
    }
}

/// The finished artifact as a `(filename, mime, path)` attachment; empty
/// unless the job succeeded with a result file.
fn result_files(job: &ExternalJob) -> Vec<(String, String, String)> {
    if !matches!(job.status, ExternalJobStatus::Done) {
        return Vec::new();
    }
    let Some(path) = job.result_path.as_deref().filter(|p| !p.is_empty()) else {
        return Vec::new();
    };
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);
    let mime = match job.kind {
        ExternalJobKind::VideoGen => "video/mp4",
        ExternalJobKind::ImageGen => "image/png",
    };
    vec![(filename.to_string(), mime.to_string(), path.to_string())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::external_jobs::{ExternalJobDelivery, ExternalJobOrigin};

    fn job(channel: &str, session_key: &str) -> ExternalJob {
        ExternalJob::new_submitted(
            session_key,
            ExternalJobDelivery {
                channel: channel.to_owned(),
                target_id: "u1".to_owned(),
                is_group: false,
                reply_to: None,
                account: None,
            },
            ExternalJobOrigin::Agent,
            "rsclaw_image",
            "task",
            ExternalJobKind::ImageGen,
            "a cat",
        )
    }

    #[test]
    fn wake_skips_scheduled_and_internal_sessions() {
        assert!(should_wake(&job("wechat", "agent:main:wechat:u1")));
        assert!(should_wake(&job("ws", "desk-1")));
        assert!(!should_wake(&job("cron", "agent:main:custom")));
        assert!(!should_wake(&job("heartbeat", "hb")));
        assert!(!should_wake(&job("wechat", "cron:job1")));
    }

    #[test]
    fn writeback_is_a_sentence_with_path_and_token() {
        let mut done = job("wechat", "s");
        done.status = ExternalJobStatus::Done;
        done.result_path = Some("/d/rsclaw/images/dl_i_202610271200abc.png".to_owned());
        assert_eq!(
            writeback_text(&done),
            "The image is ready at /d/rsclaw/images/dl_i_202610271200abc.png \
             (@dl_i_202610271200abc.png)."
        );
        let mut failed = job("wechat", "s");
        failed.status = ExternalJobStatus::Failed;
        failed.error = Some("quota".to_owned());
        assert_eq!(writeback_text(&failed), "The image generation failed: quota");
    }

    #[test]
    fn wake_text_names_the_result_and_the_way_out() {
        let mut done = job("wechat", "s");
        done.status = ExternalJobStatus::Done;
        done.result_path = Some("/tmp/cat.png".to_owned());
        let text = wake_text(&done);
        assert!(text.starts_with("[image generation complete]"));
        assert!(text.contains("/tmp/cat.png"));
        assert!(text.contains("NO_REPLY"));

        let mut failed = job("wechat", "s");
        failed.status = ExternalJobStatus::TimedOut;
        failed.error = Some("timed out after 1800s".to_owned());
        let text = wake_text(&failed);
        assert!(text.starts_with("[image generation timed out]"));
        assert!(text.contains("Do not resubmit"));
    }
}
