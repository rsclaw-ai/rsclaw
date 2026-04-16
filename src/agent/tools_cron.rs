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
                let schedule = args["schedule"]
                    .as_str()
                    .ok_or_else(|| anyhow!("cron add: `schedule` required"))?;
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
                // Schedule: use nested format if tz provided, flat otherwise.
                if let Some(tz_val) = tz {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                } else {
                    job["schedule"] = json!({"kind": "cron", "expr": schedule});
                }
                // Payload in OpenClaw format.
                job["payload"] = json!({"kind": "systemEvent", "text": message});
                if let Some(n) = name {
                    job["name"] = json!(n);
                }

                // Auto-set delivery to the originating channel+peer when not explicitly specified.
                let channel = &ctx.channel;
                let peer_id = &ctx.peer_id;
                if !channel.is_empty() && channel != "system" && channel != "cron" && !peer_id.is_empty() {
                    job["delivery"] = json!({
                        "channel": channel,
                        "to": peer_id,
                        "mode": "always"
                    });
                    debug!(channel, peer_id, "cron add: auto-set delivery to originating channel");
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

                Ok(json!({"added": id, "schedule": schedule, "message": message}))
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
