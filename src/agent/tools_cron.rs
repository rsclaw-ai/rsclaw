//! Cron job management tool handlers and file-based job storage helpers.
//!
//! Split from `tools_misc.rs` for maintainability.  All methods live in
//! `impl AgentRuntime` via the split-impl pattern (same struct, different
//! file).

use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::Utc;
use serde_json::{Value, json};
use tracing::debug;
use uuid::Uuid;

impl super::runtime::AgentRuntime {
    pub(crate) async fn tool_cron(
        &self,
        args: Value,
        ctx: &super::runtime::RunContext,
    ) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| {
                anyhow!("cron: `action` must be a string — one of: list, add, edit, remove, enable, disable")
            })?;

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
                // Fixed-interval schedule: prefer `every_seconds` (friendly), accept `every_ms`
                // too.
                let every_ms: Option<u64> = args["every_ms"]
                    .as_u64()
                    .or(args["everyMs"].as_u64())
                    .or_else(|| {
                        args["every_seconds"]
                            .as_u64()
                            .map(|s| s.saturating_mul(1000))
                    })
                    .or_else(|| {
                        args["everySeconds"]
                            .as_u64()
                            .map(|s| s.saturating_mul(1000))
                    });

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
                                    account: None,
                                };
                                if let Err(e) = tx.send(msg) {
                                    tracing::warn!(error = %e, "cron in-memory timer: notification_tx send failed");
                                }
                            } else {
                                tracing::warn!(
                                    "cron in-memory timer: no notification_tx wired up, reminder dropped"
                                );
                            }
                        });
                        return Ok(
                            json!({"added": id, "type": "in-memory timer", "delay_ms": delay, "message": message}),
                        );
                    }

                    // Long delay: persist to cron.json5
                    let at_ms = now_ms + delay;
                    job["schedule"] = json!({"kind": "once", "atMs": at_ms});
                } else if let Some(interval_ms) = every_ms {
                    // Fixed-interval recurring schedule. Wins over `schedule` if both supplied.
                    if interval_ms == 0 {
                        return Err(anyhow!("cron add: `every_seconds`/`every_ms` must be > 0"));
                    }
                    // Floor: 2s. Below this the scheduler busy-loops (matches
                    // crate::cron::MIN_REFIRE_GAP_MS).
                    const MIN_INTERVAL_MS: u64 = 2_000;
                    // Ceiling: 10 years. Beyond this `anchor_ms + n * every_ms`
                    // can overflow u64 inside compute_next_run.
                    const MAX_INTERVAL_MS: u64 = 10 * 365 * 24 * 3600 * 1000;
                    if interval_ms < MIN_INTERVAL_MS {
                        return Err(anyhow!(
                            "cron add: `every_seconds`/`every_ms` must be >= 2s ({MIN_INTERVAL_MS}ms) — shorter intervals will busy-loop the scheduler"
                        ));
                    }
                    if interval_ms > MAX_INTERVAL_MS {
                        return Err(anyhow!(
                            "cron add: `every_seconds`/`every_ms` must be <= 10 years ({MAX_INTERVAL_MS}ms)"
                        ));
                    }
                    if schedule.is_some() {
                        tracing::warn!(
                            interval_ms,
                            schedule = ?schedule,
                            "cron add: both `schedule` and `every_seconds`/`every_ms` provided; using interval and ignoring schedule"
                        );
                    }
                    // Anchor at now so the first fire is `now + interval_ms` (per
                    // CronSchedule::compute_next_run).
                    job["schedule"] =
                        json!({"kind": "every", "everyMs": interval_ms, "anchorMs": now_ms});
                } else if let Some(sched) = schedule {
                    // Validate BEFORE persisting — the WS path always did this,
                    // but the tool path used to write bad expressions straight
                    // to cron.json5 where they failed silently at reload. The
                    // validator's message teaches the 5-field format (incl. the
                    // "017 * * *" missing-space hint), so the model can fix the
                    // expression on its next try.
                    if let Err(msg) = crate::cron::validate_cron_expr(sched) {
                        return Err(anyhow!("cron add: invalid schedule: {msg}"));
                    }
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
                            }
                            .to_owned()
                        });
                    job["schedule"] = json!({"kind": "cron", "expr": sched, "tz": tz_val});
                } else {
                    return Err(anyhow!(
                        "cron add: no schedule found — pass exactly one of `schedule` (cron expr string), `every_seconds`/`every_ms`, or `delay_ms`. Numeric fields must be JSON numbers, not quoted strings; if you already passed one, fix its type and retry"
                    ));
                }
                // `kind` decides what fires at schedule time:
                //   agentTurn (default) — dispatch to agent, run LLM + tools,
                //     deliver agent's response. Use for monitoring / queries.
                //   systemEvent — deliver `text` verbatim, no agent run, no
                //     LLM cost. Use for fixed reminders.
                // The LLM picks via the `kind` arg; we default to agentTurn
                // when omitted (safer — agent always runs, even if wasteful).
                let kind = args["kind"].as_str().unwrap_or("agentTurn");
                let kind = if kind == "systemEvent" || kind == "agentTurn" {
                    kind
                } else {
                    "agentTurn"
                };
                job["payload"] = json!({"kind": kind, "text": message});
                if let Some(n) = name {
                    job["name"] = json!(n);
                }

                // Round-robin iter — scheduler picks `items[cursor]` per fire,
                // substitutes `{current}`/`{next}`/`{index}`/`{total}` in the
                // message, and advances+persists the cursor before dispatch.
                if let Some(arr) = args["iter"].as_array() {
                    let items: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    if items.is_empty() {
                        return Err(anyhow!(
                            "cron add: `iter` must contain at least one string item"
                        ));
                    }
                    job["iter"] = json!({"items": items, "cursor": 0});
                }

                // Auto-set delivery to the originating channel+peer when not explicitly
                // specified. Special case: WS chat transport uses
                // ctx.channel="ws", but the delivery sink registered in
                // ChannelManager is "desktop" (DesktopChannel broadcasts to all
                // connected WS clients). Remap so send_delivery can route.
                let channel = &ctx.channel;
                let peer_id = &ctx.peer_id;
                if !channel.is_empty()
                    && channel != "system"
                    && channel != "cron"
                    && !peer_id.is_empty()
                {
                    let delivery_channel: &str = if channel == "ws" {
                        "desktop"
                    } else {
                        channel.as_str()
                    };
                    job["delivery"] = json!({
                        "channel": delivery_channel,
                        "to": peer_id,
                        "mode": "always"
                    });
                    debug!(
                        channel = delivery_channel,
                        peer_id, "cron add: auto-set delivery to originating channel"
                    );
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
                // Echo back WHAT was scheduled in plain words. The two
                // historical cron mistakes (one-time intent built as a
                // forever-recurring job; systemEvent echoing an instruction
                // instead of executing it) are both invisible in a bare
                // {"added": id} ack — but obvious in this summary, so the
                // model can remove+re-add in the same turn. This echo is
                // what allowed the tool description to drop its 1.3k-token
                // prevention manual.
                if let Some(interval_ms) = every_ms {
                    resp["every_ms"] = json!(interval_ms);
                    resp["fires"] = json!(format!(
                        "every {}s, repeating forever — if the user wanted this once, remove and re-add with delay_ms",
                        interval_ms / 1000
                    ));
                } else if let Some(s) = schedule {
                    resp["schedule"] = json!(s);
                    resp["fires"] = json!(format!(
                        "recurring per '{s}', repeating forever — if the user asked for ONE specific time (today/tomorrow), remove this and re-add with delay_ms"
                    ));
                } else {
                    resp["fires"] = json!("once, then auto-removes");
                }
                if kind == "systemEvent" {
                    resp["note"] = json!(
                        "systemEvent delivers the message text VERBATIM each fire — no agent, no tools. If the message describes an action to perform (query/check/fetch), remove and re-add with kind=agentTurn."
                    );
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
                            "cron remove: index {} out of range — {} job(s) exist; run action=list to get fresh 1-based indexes (if 0 jobs, there is nothing to remove)",
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
                        return Err(anyhow!(
                            "cron remove: no job with id={} — ids are often truncated in chat; run action=list and remove by `index` instead",
                            id
                        ));
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
                            "cron {}: index {} out of range — {} job(s) exist; run action=list to get fresh 1-based indexes",
                            action,
                            index,
                            jobs.len()
                        ));
                    }
                    idx - 1
                } else if let Some(id) = args["id"].as_str() {
                    match jobs.iter().position(|j| j["id"].as_str() == Some(id)) {
                        Some(pos) => pos,
                        None => {
                            return Err(anyhow!("cron {}: job not found with id={}", action, id));
                        }
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
                            "cron edit: index {} out of range — {} job(s) exist; run action=list to get fresh 1-based indexes",
                            index,
                            jobs.len()
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
                    // Same teaching validation as the add path.
                    if let Err(msg) = crate::cron::validate_cron_expr(schedule) {
                        return Err(anyhow!("cron edit: invalid schedule: {msg}"));
                    }
                    let tz = args["tz"].as_str();
                    if let Some(tz_val) = tz {
                        jobs[idx]["schedule"] =
                            json!({"kind": "cron", "expr": schedule, "tz": tz_val});
                    } else {
                        jobs[idx]["schedule"] = json!({"kind": "cron", "expr": schedule});
                    }
                }
                if let Some(message) = args["message"].as_str() {
                    // `kind` selectable like the create branch — see comment there.
                    let kind = args["kind"].as_str().unwrap_or("agentTurn");
                    let kind = if kind == "systemEvent" || kind == "agentTurn" {
                        kind
                    } else {
                        "agentTurn"
                    };
                    jobs[idx]["payload"] = json!({"kind": kind, "text": message});
                }
                if let Some(name) = args["name"].as_str() {
                    jobs[idx]["name"] = json!(name);
                }
                if let Some(agent_id) = args["agentId"].as_str().or(args["agent_id"].as_str()) {
                    jobs[idx]["agentId"] = json!(agent_id);
                }
                // Iter list edit. Pass `iter` to replace the items array;
                // pass `iter_cursor` (default 0) to set the cursor explicitly.
                // Pass `iter` as null or an empty array to clear iter mode.
                if args.get("iter").is_some() {
                    let raw = &args["iter"];
                    if raw.is_null() || raw.as_array().is_some_and(|a| a.is_empty()) {
                        jobs[idx].as_object_mut().map(|o| o.remove("iter"));
                    } else if let Some(arr) = raw.as_array() {
                        let items: Vec<String> = arr
                            .iter()
                            .filter_map(|v| v.as_str().map(str::to_owned))
                            .collect();
                        if items.is_empty() {
                            return Err(anyhow!(
                                "cron edit: `iter` must be a non-empty array of strings, or null/[] to clear"
                            ));
                        }
                        let cursor = args
                            .get("iter_cursor")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                            .min(items.len() as u64 - 1);
                        jobs[idx]["iter"] = json!({"items": items, "cursor": cursor});
                    } else {
                        return Err(anyhow!("cron edit: `iter` must be an array of strings"));
                    }
                } else if let Some(cursor) = args.get("iter_cursor").and_then(|v| v.as_u64()) {
                    // Cursor-only edit: must already have iter configured.
                    let len = jobs[idx]
                        .get("iter")
                        .and_then(|i| i.get("items"))
                        .and_then(|a| a.as_array())
                        .map(|a| a.len() as u64)
                        .unwrap_or(0);
                    if len == 0 {
                        return Err(anyhow!(
                            "cron edit: `iter_cursor` requires job to already have an `iter` configured"
                        ));
                    }
                    jobs[idx]["iter"]["cursor"] = json!(cursor.min(len - 1));
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

/// Format a list of cron jobs into the canonical multi-line text used by
/// `/cron list` and the agent's `__CRON_LIST__` marker.
///
/// Returns "No cron jobs configured." when the slice is empty so callers don't
/// have to special-case that themselves. Both the desktop (agent runtime) and
/// gateway preparse paths route here so feishu/wechat/etc. see the same output
/// format as `/cron list` from the desktop console.
pub(crate) fn format_cron_jobs(jobs: &[Value]) -> String {
    if jobs.is_empty() {
        return "No cron jobs configured.".to_owned();
    }
    let mut lines = vec!["Cron jobs:".to_owned()];
    for (i, job) in jobs.iter().enumerate() {
        let id = job.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let enabled = job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        let status = if enabled { "" } else { " (disabled)" };
        let agent = job
            .get("agentId")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let name = job.get("name").and_then(|v| v.as_str());
        let schedule = match job.get("schedule") {
            Some(s) if s.is_object() => {
                if let Some(expr) = s.get("expr").and_then(|v| v.as_str()) {
                    expr.to_owned()
                } else if let Some(at) = s.get("atMs").and_then(|v| v.as_u64()) {
                    format!("once@{at}")
                } else {
                    s.to_string()
                }
            }
            Some(s) if s.is_string() => s.as_str().unwrap_or("?").to_owned(),
            _ => "?".to_owned(),
        };
        let message = job
            .get("payload")
            .and_then(|p| p.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let msg_preview = if message.chars().count() > 50 {
            let end = message
                .char_indices()
                .nth(47)
                .map(|(idx, _)| idx)
                .unwrap_or(message.len());
            format!("{}...", &message[..end])
        } else {
            message.to_owned()
        };
        let label = name.map(|n| format!(" {n}")).unwrap_or_default();
        lines.push(format!(
            "  #{} [{}] {} -> {}{} \"{}\"{}",
            i + 1,
            id,
            schedule,
            agent,
            label,
            msg_preview,
            status
        ));
    }
    lines.join("\n")
}

/// Read cron jobs.
///
/// Authoritative source is redb once the gateway has initialised the cron store
/// (mirrors `crate::cron::load_cron_jobs`). Without this, the agent tool read
/// from `cron.json5` while the rest of the system used redb — so a job added
/// here was invisible to the runner and got clobbered when the runner
/// re-exported redb→file on the next reload (add succeeded, list came back
/// empty). Falls back to the file for tests / standalone tools where redb
/// isn't wired up. Handles both bare array `[...]` and wrapped
/// `{"version":1,"jobs":[...]}` formats; parses with json5 for comment support.
pub(crate) async fn read_cron_jobs(path: &std::path::Path) -> Vec<Value> {
    if let Some(store) = crate::cron::cron_store() {
        match store.cron_list() {
            Ok(entries) => {
                return entries
                    .into_iter()
                    .filter_map(|(_, json)| serde_json::from_str::<Value>(&json).ok())
                    .collect();
            }
            Err(e) => {
                tracing::warn!(err = %e, "cron: redb list failed in agent tool; falling back to file");
            }
        }
    }
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

/// Write cron jobs.
///
/// Authoritative target is redb once initialised (mirrors
/// `crate::cron::save_cron_jobs`): the agent tool must write the same store the
/// runner reads, or the runner's redb→file re-export on reload wipes the
/// agent's write. The `cron.json5` file is still (re)written as a best-effort
/// export so `cat` / git-diff / hand-edit / the pre-redb fallback keep working.
pub(crate) async fn write_cron_jobs(path: &std::path::Path, jobs: &[Value]) -> Result<()> {
    if let Some(store) = crate::cron::cron_store() {
        let entries: Vec<(String, String)> = jobs
            .iter()
            .filter_map(|j| {
                let id = j.get("id").and_then(|v| v.as_str())?.to_owned();
                serde_json::to_string(j).ok().map(|s| (id, s))
            })
            .collect();
        store
            .cron_bulk_replace(&entries)
            .map_err(|e| anyhow!("cron: redb bulk_replace failed: {e}"))?;
    }
    let wrapper = json!({"version": 1, "jobs": jobs});
    let json = serde_json::to_string_pretty(&wrapper)?;
    let tmp = format!("{}.tmp", path.display());
    tokio::fs::write(&tmp, json)
        .await
        .map_err(|e| anyhow!("cron: failed to write jobs tmp: {e}"))?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| anyhow!("cron: failed to rename jobs file: {e}"))?;
    Ok(())
}
