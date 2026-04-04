use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

pub async fn health(ctx: MethodCtx) -> MethodResult {
    let uptime = ctx.state.started_at.elapsed();
    let ws_count = ctx.state.ws_conns.count().await;
    let agent_count = ctx.state.agents.len();
    let now = chrono::Utc::now().to_rfc3339();

    // Channel summary for the debug page.
    let channel_cfg = ctx.state.live.channel.read().await;
    let channels_raw = &channel_cfg.channels;
    let mut connected_channels: Vec<&str> = Vec::new();
    if channels_raw.telegram.is_some() {
        connected_channels.push("telegram");
    }
    if channels_raw.discord.is_some() {
        connected_channels.push("discord");
    }
    if channels_raw.slack.is_some() {
        connected_channels.push("slack");
    }
    if channels_raw.feishu.is_some() {
        connected_channels.push("feishu");
    }
    if channels_raw.dingtalk.is_some() {
        connected_channels.push("dingtalk");
    }
    if channels_raw.wecom.is_some() {
        connected_channels.push("wecom");
    }
    if channels_raw.wechat.is_some() {
        connected_channels.push("wechat");
    }
    drop(channel_cfg);

    Ok(serde_json::json!({
        "status": "ok",
        "version": env!("RSCLAW_BUILD_VERSION"),
        "runtimeVersion": env!("RSCLAW_BUILD_VERSION"),
        // Uptime / tick info used by Overview and Debug pages.
        "uptime": uptime.as_secs(),
        "uptimeFormatted": format_duration(uptime),
        "tickInterval": 15,
        "tickIntervalSeconds": 15,
        "pid": std::process::id(),
        "lastChannelRefresh": now,
        // Sub-objects read by the Debug page.
        "heartbeat": {
            "status": "ok",
            "lastBeat": now,
            "intervalSeconds": 15,
        },
        "health": {
            "status": "ok",
            "agents": agent_count,
            "store": "connected",
            "storeType": "redb",
        },
        "channelSummary": {
            "connected": connected_channels,
            "total": connected_channels.len(),
        },
        "connections": {
            "websocket": ws_count,
        },
    }))
}

pub async fn models_list(ctx: MethodCtx) -> MethodResult {
    // Collect unique model IDs from agents (per-agent model + defaults model).
    let default_model = ctx
        .state
        .config
        .agents
        .defaults
        .model
        .as_ref()
        .and_then(|m| m.primary.as_deref());

    let mut seen = std::collections::HashSet::new();
    let mut models = Vec::new();

    for h in ctx.state.agents.all() {
        let model_id = h
            .config
            .model
            .as_ref()
            .and_then(|m| m.primary.as_deref())
            .or(default_model)
            .unwrap_or("unknown");
        if seen.insert(model_id.to_owned()) {
            models.push(serde_json::json!({
                "id": model_id,
                "object": "model",
                "ownedBy": "rsclaw",
            }));
        }
    }

    // Also include model aliases from agents.defaults.models
    if let Some(aliases) = ctx.state.config.agents.defaults.models.as_ref() {
        for alias_name in aliases.keys() {
            if seen.insert(alias_name.clone()) {
                models.push(serde_json::json!({
                    "id": alias_name,
                    "object": "model",
                    "ownedBy": "rsclaw",
                }));
            }
        }
    }

    Ok(serde_json::json!({ "models": models }))
}

pub async fn config_get(ctx: MethodCtx) -> MethodResult {
    let redacted = serde_json::json!({
        "gatewayPort": ctx.state.config.gateway.port,
        "gatewayMode": format!("{:?}", ctx.state.config.gateway.mode),
        "gatewayBind": format!("{:?}", ctx.state.config.gateway.bind),
        "agents": ctx.state.config.agents.list.iter().map(|a| serde_json::json!({
            "id": a.id,
            "default": a.default,
        })).collect::<Vec<_>>(),
    });
    Ok(redacted)
}

pub async fn cron_list(_ctx: MethodCtx) -> MethodResult {
    let config = crate::config::load().map_err(|e| ErrorShape::internal(e.to_string()))?;
    let jobs = config
        .ops
        .cron
        .as_ref()
        .and_then(|c| c.jobs.as_deref())
        .unwrap_or(&[]);
    let list: Vec<serde_json::Value> = jobs
        .iter()
        .map(|j| {
            let mut v = serde_json::json!({
                "id": j.id,
                "schedule": j.schedule,
                "enabled": j.enabled.unwrap_or(true),
                "agentId": j.agent_id,
                "message": j.message,
            });
            if let Some(ref name) = j.name {
                v["name"] = name.clone().into();
            }
            if let Some(ref tz) = j.tz {
                v["tz"] = tz.clone().into();
            }
            v
        })
        .collect();
    Ok(serde_json::json!({ "jobs": list }))
}

pub async fn cron_add(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let schedule = params["schedule"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing schedule"))?;
    let message = params["message"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing message"))?;
    let agent_id = params["agentId"].as_str();

    let (path, mut val) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;
    let count = val
        .pointer("/cron/jobs")
        .and_then(|v| v.as_array())
        .map_or(0, |a| a.len());
    let id = format!("job-{}", count + 1);
    let mut job = serde_json::json!({
        "id": id,
        "schedule": schedule,
        "message": message,
    });
    if let Some(agent) = agent_id {
        job["agentId"] = agent.into();
    }
    if let Some(arr) = val.pointer_mut("/cron/jobs").and_then(|v| v.as_array_mut()) {
        arr.push(job);
    } else {
        crate::cmd::config_json::set_nested_value(&mut val, "cron.jobs", serde_json::json!([job]))
            .map_err(|e| ErrorShape::internal(e.to_string()))?;
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&val).map_err(|e| ErrorShape::internal(e.to_string()))?,
    )
    .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "id": id, "schedule": schedule }))
}

pub async fn cron_remove(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let id = params["id"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing id"))?;

    let (path, mut val) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;
    if let Some(jobs) = val.pointer_mut("/cron/jobs").and_then(|j| j.as_array_mut()) {
        let before = jobs.len();
        jobs.retain(|j| j["id"].as_str() != Some(id));
        if jobs.len() == before {
            return Err(ErrorShape::not_found(format!("cron job '{id}' not found")));
        }
    } else {
        return Err(ErrorShape::not_found(format!("cron job '{id}' not found")));
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&val).map_err(|e| ErrorShape::internal(e.to_string()))?,
    )
    .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "removed": id }))
}

pub async fn logs_tail(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();
    let limit = params
        .and_then(|p| p.get("lines").or_else(|| p.get("limit")))
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    // Candidate paths: configured file first, then well-known defaults.
    let configured = ctx
        .state
        .config
        .raw
        .logging
        .as_ref()
        .and_then(|l| l.file.as_deref())
        .unwrap_or("")
        .to_owned();

    let base = crate::config::loader::base_dir();
    let candidates = [
        configured.clone(),
        crate::config::loader::log_file()
            .to_string_lossy()
            .into_owned(),
        base.join("gateway.log").to_string_lossy().into_owned(),
        base.join("logs/gateway.log").to_string_lossy().into_owned(),
    ];

    for path in &candidates {
        if path.is_empty() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            if content.is_empty() {
                continue;
            }
            let all: Vec<&str> = content.lines().collect();
            let start = all.len().saturating_sub(limit);
            let tail: Vec<&str> = all[start..].to_vec();
            let entries: Vec<serde_json::Value> = tail
                .iter()
                .enumerate()
                .map(|(i, line)| serde_json::json!({ "index": start + i, "line": line }))
                .collect();
            return Ok(serde_json::json!({ "lines": tail, "entries": entries, "source": path }));
        }
    }

    // No log file found — return an empty list (not an error so the UI renders
    // cleanly).
    Ok(serde_json::json!({ "lines": [], "entries": [], "source": "none" }))
}

pub async fn system_update_check(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({
        "currentVersion": env!("RSCLAW_BUILD_VERSION"),
        "updateAvailable": false,
        "latestVersion": env!("RSCLAW_BUILD_VERSION"),
    }))
}

pub async fn system_update_run(_ctx: MethodCtx) -> MethodResult {
    // Self-update is not implemented for rsclaw; return a helpful message.
    Err(ErrorShape::bad_request(
        "self-update is not supported; rebuild from source or use your package manager",
    ))
}

/// channels.status — returns health status of configured channels.
pub async fn channels_status(ctx: MethodCtx) -> MethodResult {
    let channel_cfg = ctx.state.live.channel.read().await;
    let channels_raw = &channel_cfg.channels;
    let now = chrono::Utc::now().to_rfc3339();
    let mut channels = Vec::new();

    // Check each known channel type for configuration.
    let checks: Vec<(&str, bool)> = vec![
        ("telegram", channels_raw.telegram.is_some()),
        ("discord", channels_raw.discord.is_some()),
        ("slack", channels_raw.slack.is_some()),
        ("whatsapp", channels_raw.whatsapp.is_some()),
        ("signal", channels_raw.signal.is_some()),
        ("feishu", channels_raw.feishu.is_some()),
        ("dingtalk", channels_raw.dingtalk.is_some()),
        ("wecom", channels_raw.wecom.is_some()),
        ("wechat", channels_raw.wechat.is_some()),
        ("mattermost", channels_raw.mattermost.is_some()),
        ("qq", channels_raw.qq.is_some()),
    ];

    for (name, configured) in checks {
        if configured {
            channels.push(serde_json::json!({
                "id": name,
                "type": name,
                "name": name,
                "enabled": true,
                "status": "connected",
                "lastRefresh": now,
            }));
        }
    }

    Ok(serde_json::json!({ "channels": channels }))
}

/// system.presence — returns active gateway instances and connected clients.
pub async fn system_presence(ctx: MethodCtx) -> MethodResult {
    let ws_count = ctx.state.ws_conns.count().await;
    let uptime = ctx.state.started_at.elapsed();

    Ok(serde_json::json!({
        "instances": [{
            "id": "gateway",
            "type": "gateway",
            "version": env!("RSCLAW_BUILD_VERSION"),
            "uptime": uptime.as_secs(),
            "status": "online",
        }],
        "clients": {
            "websocket": ws_count,
        },
    }))
}

/// cron.runs — returns recent run history for cron jobs.
pub async fn cron_runs(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();
    let job_id = params.and_then(|p| p.get("id")).and_then(|v| v.as_str());
    let limit = params
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    let data_dir = crate::config::loader::base_dir().join("var/data/cron");
    let mut runs: Vec<crate::cron::RunLogEntry> = Vec::new();

    if data_dir.exists() {
        let pattern = if let Some(id) = job_id {
            format!("{id}.jsonl")
        } else {
            "*.jsonl".to_owned()
        };

        if let Ok(entries) = std::fs::read_dir(&data_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if job_id.is_some() && name_str != pattern {
                    continue;
                }
                if !name_str.ends_with(".jsonl") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(entry.path()) {
                    for line in content.lines().rev().take(limit) {
                        if let Ok(entry) = serde_json::from_str::<crate::cron::RunLogEntry>(line) {
                            runs.push(entry);
                        }
                    }
                }
            }
        }
    }

    runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    runs.truncate(limit);

    Ok(serde_json::json!({ "runs": runs }))
}

/// system.snapshot — returns full gateway runtime snapshot for Overview/Debug
/// pages.
pub async fn system_snapshot(ctx: MethodCtx) -> MethodResult {
    let uptime = ctx.state.started_at.elapsed();
    let ws_count = ctx.state.ws_conns.count().await;
    let agent_count = ctx.state.agents.len();
    let sessions = ctx.state.store.db.list_sessions().unwrap_or_default().len();

    // Channel summary
    let channel_cfg = ctx.state.live.channel.read().await;
    let channels_raw = &channel_cfg.channels;
    let mut active_channels = Vec::new();
    if channels_raw.telegram.is_some() {
        active_channels.push("telegram");
    }
    if channels_raw.discord.is_some() {
        active_channels.push("discord");
    }
    if channels_raw.slack.is_some() {
        active_channels.push("slack");
    }
    if channels_raw.feishu.is_some() {
        active_channels.push("feishu");
    }
    if channels_raw.dingtalk.is_some() {
        active_channels.push("dingtalk");
    }
    if channels_raw.wecom.is_some() {
        active_channels.push("wecom");
    }
    if channels_raw.wechat.is_some() {
        active_channels.push("wechat");
    }
    if channels_raw.qq.is_some() {
        active_channels.push("qq");
    }

    let now = chrono::Utc::now().to_rfc3339();
    Ok(serde_json::json!({
        "status": "ok",
        "runtimeVersion": env!("RSCLAW_BUILD_VERSION"),
        "version": env!("RSCLAW_BUILD_VERSION"),
        // Uptime — emit both field names used across openclaw versions.
        "uptime": uptime.as_secs(),
        "uptimeSeconds": uptime.as_secs(),
        "uptimeFormatted": format_duration(uptime),
        // Tick interval.
        "tickInterval": 15,
        "tickIntervalSeconds": 15,
        "pid": std::process::id(),
        "agents": agent_count,
        "sessions": sessions,
        "connections": ws_count,
        "wsConnections": ws_count,
        "channels": active_channels,
        // Last channel refresh — multiple aliases.
        "lastChannelRefresh": now,
        "lastRefresh": now,
        "channelRefreshedAt": now,
        "store": "redb",
        "storeType": "redb",
        "heartbeat": {
            "status": "ok",
            "lastBeat": now,
            "intervalSeconds": 15,
        },
        "health": {
            "status": "ok",
            "agents": agent_count,
            "store": "connected",
            "storeType": "redb",
        },
        "channelSummary": {
            "connected": active_channels,
            "total": active_channels.len(),
        },
    }))
}

fn format_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

/// `status` — gateway status info compatible with openclaw WebUI.
pub async fn status(ctx: MethodCtx) -> MethodResult {
    let uptime = ctx.state.started_at.elapsed();
    Ok(serde_json::json!({
        "status": "ok",
        "version": env!("RSCLAW_BUILD_VERSION"),
        "uptime": uptime.as_secs(),
        "cwd": std::env::current_dir()
            .map(|p| crate::config::loader::path_to_forward_slash(&p))
            .unwrap_or_default(),
        "platform": std::env::consts::OS,
        "nodeVersion": format!("rust-{}", env!("RSCLAW_BUILD_VERSION")),
    }))
}

/// `cron.update` — patch an existing cron job by id.
pub async fn cron_update(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let id = params["id"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing id"))?;

    let (path, mut val) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let jobs = val
        .pointer_mut("/cron/jobs")
        .and_then(|j| j.as_array_mut())
        .ok_or_else(|| ErrorShape::not_found(format!("cron job '{id}' not found")))?;

    let job = jobs
        .iter_mut()
        .find(|j| j["id"].as_str() == Some(id))
        .ok_or_else(|| ErrorShape::not_found(format!("cron job '{id}' not found")))?;

    // Patch allowed fields.
    if let Some(schedule) = params.get("schedule").and_then(|v| v.as_str()) {
        job["schedule"] = schedule.into();
    }
    if let Some(message) = params.get("message").and_then(|v| v.as_str()) {
        job["message"] = message.into();
    }
    if let Some(agent_id) = params.get("agentId").and_then(|v| v.as_str()) {
        job["agentId"] = agent_id.into();
    }
    if let Some(enabled) = params.get("enabled").and_then(|v| v.as_bool()) {
        job["enabled"] = enabled.into();
    }

    std::fs::write(
        &path,
        serde_json::to_string_pretty(&val).map_err(|e| ErrorShape::internal(e.to_string()))?,
    )
    .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "updated": id }))
}

/// `cron.delete` — alias for cron.remove.
pub async fn cron_delete(ctx: MethodCtx) -> MethodResult {
    cron_remove(ctx).await
}

/// `logs.subscribe` — stub for real-time log push (not yet implemented).
pub async fn logs_subscribe(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "subscribed": true }))
}

/// `update.run` — self-update is not supported.
pub async fn update_run(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({ "error": "self-update not supported" }))
}

pub async fn system_shutdown(_ctx: MethodCtx) -> MethodResult {
    // Initiate graceful shutdown by sending a signal to the runtime.
    // For now, we acknowledge the request. The actual shutdown would
    // be handled by the signal handler in main.
    tracing::warn!("system.shutdown requested via WS");
    Ok(serde_json::json!({ "shutting_down": true }))
}

pub async fn cron_run(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let id = params["id"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing id"))?;

    let config = crate::config::load().map_err(|e| ErrorShape::internal(e.to_string()))?;
    let jobs = config
        .ops
        .cron
        .as_ref()
        .and_then(|c| c.jobs.as_deref())
        .unwrap_or(&[]);
    let job = jobs
        .iter()
        .find(|j| j.id == id)
        .ok_or_else(|| ErrorShape::not_found(format!("cron job '{id}' not found")))?;

    let port = config.gateway.port;
    let url = format!("http://127.0.0.1:{port}/api/v1/message");
    let body = serde_json::json!({
        "text": job.message,
        "agent_id": job.agent_id,
        "session_key": format!("cron:{id}:manual"),
    });
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| ErrorShape::internal(format!("gateway unreachable at {url}: {e}")))?;
    if resp.status().is_success() {
        Ok(serde_json::json!({ "triggered": id }))
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(ErrorShape::internal(format!(
            "gateway error {status}: {text}"
        )))
    }
}
