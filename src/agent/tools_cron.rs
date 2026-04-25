//! Cron job management tool handlers and file-based job storage helpers.
//!
//! Split from `tools_misc.rs` for maintainability.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different file).

use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

impl super::runtime::AgentRuntime {
    pub(crate) async fn tool_cron(&self, args: Value, ctx: &super::runtime::RunContext) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow!("cron: `action` required"))?;

        let cron_dir = crate::config::loader::base_dir();
        let cron_path = cron_dir.join("cron.json5");

        match action {
            "list" => {
                let jobs = read_cron_jobs(&cron_path).await;
                // Add 1-based index to each job for easier reference by LLMs
                let jobs_with_index: Vec<Value> = jobs
                    .iter()
                    .enumerate()
                    .map(|(i, j)| {
                        let mut indexed = j.clone();
                        indexed["_index"] = json!(i + 1);
                        indexed
                    })
                    .collect();
                Ok(
                    json!({"jobs": jobs_with_index, "hint": "Use index number (#1, #2, etc.) for removal to avoid ID truncation issues"}),
                )
            }
            "add" => {
                let message = args["message"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `message` required"))?;
                let name = args["name"].as_str();
                let tz = args["tz"].as_str();
                let agent_id = args["agent_id"].as_str().or(args["agentId"].as_str());

                let mut jobs = read_cron_jobs(&cron_path).await;

                let now_ms = Utc::now().timestamp_millis() as u64;
                let id = Uuid::new_v4().to_string();
                let mut job = json!({
                    "id": id,
                    "agentId": agent_id.unwrap_or("main"),
                    "enabled": true,
                    "createdAtMs": now_ms,
                    "updatedAtMs": now_ms,
                });

                // Schedule: support cron expr, delay (once), or interval.
                let delay_ms = args["delay_ms"].as_u64().or(args["delayMs"].as_u64());
                let schedule = args["schedule"].as_str();
                // Fixed-interval schedule: prefer `every_seconds` (friendly), accept `every_ms` too.
                let every_ms: Option<u64> = args["every_ms"]
                    .as_u64()
                    .or(args["everyMs"].as_u64())
                    .or_else(|| args["every_seconds"].as_u64().map(|s| s.saturating_mul(1000)))
                    .or_else(|| args["everySeconds"].as_u64().map(|s| s.saturating_mul(1000)));

                if let Some(delay) = delay_ms {
                    // Short delays (<=30min): use in-memory timer, skip cron.json5.
                    // Longer delays: persist to cron.json5 (survives restart).
                    let short_threshold_ms = 30 * 60 * 1000; // 30 minutes
                    if delay <= short_threshold_ms {
                        // Unified delivery path: route through notification_tx so the
                        // notification router forwards to the "desktop" channel bridge
                        // (same sink used by persisted cron jobs). Previously this
                        // path emitted AgentEvents on the event_bus which relied on a
                        // live WS subscriber at fire-time and session_id match — any
                        // reconnect or filter mismatch dropped the reminder.
                        let msg_text = message.to_owned();
                        let notif_tx = self.notification_tx.clone();
                        let peer_id = ctx.peer_id.clone();
                        let origin_channel = ctx.channel.clone();
                        let delivery_channel: String = if origin_channel == "ws" {
                            "desktop".to_owned()
                        } else {
                            origin_channel
                        };
                        let delay_dur = Duration::from_millis(delay);
                        let timer_name = name.unwrap_or("reminder").to_owned();
                        debug!(delay_ms = delay, name = %timer_name, channel = %delivery_channel, "cron: using in-memory timer (short delay)");
                        tokio::spawn(async move {
                            tokio::time::sleep(delay_dur).await;
                            if let Some(tx) = notif_tx {
                                let msg = crate::channel::OutboundMessage {
                                    target_id: peer_id,
                                    is_group: false,
                                    text: msg_text,
                                    reply_to: None,
                                    images: vec![],
                                    files: vec![],
                                    channel: Some(delivery_channel),
                                };
                                if let Err(e) = tx.send(msg) {
                                    tracing::warn!(error = %e, "cron in-memory timer: notification_tx send failed");
                                }
                            } else {
                                tracing::warn!("cron in-memory timer: no notification_tx wired up, reminder dropped");
                            }
                        });
                        return Ok(json!({"added": id, "type": "in-memory timer", "delay_ms": delay, "message": message}));
                    }

                    // Long delay: persist to cron.json5
                    let at_ms = now_ms + delay;
                    job["schedule"] = json!({"kind": "once", "atMs": at_ms});
                } else if let Some(interval_ms) = every_ms {
                    // Fixed-interval recurring schedule. Wins over `schedule` if both supplied.
                    if interval_ms == 0 {
                        return Err(anyhow!("cron add: `every_seconds`/`every_ms` must be > 0"));
                    }
                    if schedule.is_some() {
                        tracing::warn!(
                            interval_ms,
                            schedule = ?schedule,
                            "cron add: both `schedule` and `every_seconds`/`every_ms` provided; using interval and ignoring schedule"
                        );
                    }
                    // Anchor at now so the first fire is `now + interval_ms` (per CronSchedule::compute_next_run).
                    job["schedule"] = json!({"kind": "every", "everyMs": interval_ms, "anchorMs": now_ms});
                } else if let Some(sched) = schedule {
                    // Standard cron expression or interval.
                    // Always include timezone. Use LLM-provided, config, or auto-detected.
                    let tz_val = tz
                        .map(String::from)
                        .or_else(|| self.config.agents.defaults.timezone.clone())
                        .unwrap_or_else(|| {
                            // Auto-detect from system offset
                            let offset = chrono::Local::now().offset().local_minus_utc();
                            match offset {
                                25200 => "Asia/Bangkok",
                                28800 => "Asia/Shanghai",
                                32400 => "Asia/Tokyo",
                                -18000 => "US/Eastern",
                                -28800 => "US/Pacific",
                                _ => "UTC",
                            }.to_owned()
                        });
                    job["schedule"] = json!({"kind": "cron", "expr": sched, "tz": tz_val});
                } else {
                    return Err(anyhow!(
                        "cron add: `schedule`, `every_seconds`/`every_ms`, or `delay_ms` required"
                    ));
                }
                // Payload in OpenClaw format.
                job["payload"] = json!({"kind": "systemEvent", "text": message});
                if let Some(n) = name {
                    job["name"] = json!(n);
                }

                // Auto-set delivery to the originating channel+peer when not explicitly specified.
                // Special case: WS chat transport uses ctx.channel="ws", but the delivery
                // sink registered in ChannelManager is "desktop" (DesktopChannel broadcasts
                // to all connected WS clients). Remap so send_delivery can route.
                let channel = &ctx.channel;
                let peer_id = &ctx.peer_id;
                if !channel.is_empty() && channel != "system" && channel != "cron" && !peer_id.is_empty() {
                    let delivery_channel: &str = if channel == "ws" { "desktop" } else { channel.as_str() };
                    job["delivery"] = json!({
                        "channel": delivery_channel,
                        "to": peer_id,
                        "mode": "always"
                    });
                    debug!(channel = delivery_channel, peer_id, "cron add: auto-set delivery to originating channel");
                }

                jobs.push(job);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron add: failed to notify gateway reload");
                }

                let mut resp = json!({"added": id, "message": message});
                if let Some(interval_ms) = every_ms {
                    resp["every_ms"] = json!(interval_ms);
                } else if let Some(s) = schedule {
                    resp["schedule"] = json!(s);
                }
                Ok(resp)
            }
            "remove" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                // Support both `id` and `index` parameters (prefer index for reliability)
                let removed_job = if let Some(index) = args["index"].as_u64() {
                    // 1-based index
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron remove: invalid index {} (valid: 1-{})",
                            index,
                            jobs.len()
                        ));
                    }
                    let job = jobs.remove(idx - 1);
                    write_cron_jobs(&cron_path, &jobs).await?;
                    job
                } else if let Some(id) = args["id"].as_str() {
                    let before = jobs.len();
                    jobs.retain(|j| j["id"].as_str() != Some(id));
                    let removed = before - jobs.len();
                    if removed == 0 {
                        return Err(anyhow!("cron remove: job not found with id={}", id));
                    }
                    write_cron_jobs(&cron_path, &jobs).await?;
                    json!({"id": id, "count": removed})
                } else {
                    return Err(anyhow!(
                        "cron remove: `index` or `id` required (index is preferred)"
                    ));
                };

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron remove: failed to notify gateway reload");
                }

                Ok(json!({"removed": removed_job}))
            }
            "enable" | "disable" => {
                let enabled = action == "enable";
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron {}: invalid index {} (valid: 1-{})",
                            action, index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron {}: job not found with id={}", action, id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron {}: `index` or `id` required (index is preferred)",
                        action
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                jobs[idx]["enabled"] = json!(enabled);
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron {}: failed to notify gateway reload", action);
                }

                Ok(json!({action: id}))
            }
            "edit" => {
                let mut jobs = read_cron_jobs(&cron_path).await;

                let idx = if let Some(index) = args["index"].as_u64() {
                    let idx = index as usize;
                    if idx == 0 || idx > jobs.len() {
                        return Err(anyhow!(
                            "cron edit: invalid index {} (valid: 1-{})",
                            index, jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => return Err(anyhow!("cron edit: job not found with id={}", id)),
                    }
                } else {
                    return Err(anyhow!(
                        "cron edit: `index` or `id` required (index is preferred)"
                    ));
                };

                let id = jobs[idx]["id"].as_str().unwrap_or("?").to_string();
                if let Some(schedule) = args["schedule"].as_str() {
                    let tz = args["tz"].as_str();
                    if let Some(tz_val) = tz {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                    } else {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule});
                    }
                }
                if let Some(message) = args["message"].as_str() {
                    jobs[idx]["payload"] = json!({"kind": "systemEvent", "text": message});
                }
                if let Some(name) = args["name"].as_str() {
                    jobs[idx]["name"] = json!(name);
                }
                if let Some(agent_id) = args["agentId"].as_str().or(args["agent_id"].as_str()) {
                    jobs[idx]["agentId"] = json!(agent_id);
                }
                jobs[idx]["updatedAtMs"] = json!(Utc::now().timestamp_millis() as u64);
                write_cron_jobs(&cron_path, &jobs).await?;

                // Notify gateway to reload cron jobs
                let port = self.config.gateway.port;
                let client = reqwest::Client::new();
                if let Err(e) = client
                    .post(format!("http://127.0.0.1:{port}/api/v1/cron/reload"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                {
                    debug!(err = %e, "cron edit: failed to notify gateway reload");
                }

                Ok(json!({"edited": id}))
            }
            other => Err(anyhow!(
                "cron: unsupported action `{other}` (list, add, edit, remove, enable, disable)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Cron helpers (file-based job storage)
// ---------------------------------------------------------------------------

/// Read cron jobs from cron.json5.
/// Handles both bare array `[...]` and wrapped `{"version":1,"jobs":[...]}` formats.
/// Parses with json5 for comment support.
pub(crate) async fn read_cron_jobs(path: &std::path::Path) -> Vec<Value> {
    let data = tokio::fs::read_to_string(path)
        .await
        .unwrap_or_else(|_| "[]".to_owned());
    // Parse with json5 (falls back to serde_json).
    let wrapper: Value = json5::from_str(&data)
        .or_else(|_| serde_json::from_str(&data))
        .unwrap_or(Value::Array(vec![]));
    if let Some(jobs) = wrapper.get("jobs").and_then(|v| v.as_array()) {
        return jobs.clone();
    }
    if let Some(arr) = wrapper.as_array() {
        return arr.clone();
    }
    Vec::new()
}

/// Write cron jobs as JSON (readable by json5 parser).
pub(crate) async fn write_cron_jobs(path: &std::path::Path, jobs: &[Value]) -> Result<()> {
    let wrapper = json!({"version": 1, "jobs": jobs});
    tokio::fs::write(path, serde_json::to_string_pretty(&wrapper)?)
        .await
        .map_err(|e| anyhow!("cron: failed to write jobs: {e}"))?;
    Ok(())
}
