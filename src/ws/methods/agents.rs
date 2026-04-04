use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

pub async fn agents_list(ctx: MethodCtx) -> MethodResult {
    let handles = ctx.state.agents.all();

    let default_agent = ctx.state.agents.default_agent().ok();
    let default_id = default_agent.as_ref().map(|a| a.id.as_str());

    let agents: Vec<serde_json::Value> = handles
        .iter()
        .map(|h| {
            let model_name = h
                .config
                .model
                .as_ref()
                .and_then(|m| m.primary.as_deref())
                .unwrap_or("unknown");

            let toolset = h
                .config
                .model
                .as_ref()
                .and_then(|m| m.toolset.as_deref())
                .unwrap_or("standard");

            serde_json::json!({
                "id": h.id,
                "name": h.config.name.as_deref().unwrap_or(""),
                "model": model_name,
                "channels": h.config.channels.as_deref().unwrap_or(&[]),
                "toolset": toolset,
                "default": default_id == Some(h.id.as_str()),
                "status": "online"
            })
        })
        .collect();

    Ok(serde_json::json!({ "agents": agents }))
}

/// Returns the identity/config of the agent handling a given session.
/// OpenClaw WebUI calls this on every session open.
pub async fn agent_identity_get(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();
    let session_key = params
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // Try to find agent by session prefix or just return default agent info.
    let handle = ctx
        .state
        .agents
        .default_agent()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let model = handle
        .config
        .model
        .as_ref()
        .and_then(|m| m.primary.as_deref())
        .unwrap_or("unknown");

    Ok(serde_json::json!({
        "agentId": handle.id,
        "sessionKey": session_key,
        "model": model,
        "name": handle.config.name.as_deref().unwrap_or(&handle.id),
        "status": "online"
    }))
}

/// agents.files.list — list files in an agent's workspace directory.
pub async fn agents_files_list(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();
    let agent_id = params
        .and_then(|p| p.get("agentId"))
        .or_else(|| params.and_then(|p| p.get("id")))
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    let base = crate::config::loader::base_dir();
    let workspace = base.join(format!("workspace-{agent_id}"));

    let mut files = Vec::new();
    if workspace.exists()
        && let Ok(entries) = std::fs::read_dir(&workspace)
    {
        for entry in entries.flatten() {
            let meta = entry.metadata().ok();
            files.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "path": crate::config::loader::path_to_forward_slash(&entry.path()),
                "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                "isDir": meta.as_ref().is_some_and(|m| m.is_dir()),
            }));
        }
    }

    Ok(serde_json::json!({
        "agentId": agent_id,
        "workspace": crate::config::loader::path_to_forward_slash(&workspace),
        "files": files,
    }))
}

pub async fn agents_create(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: id"))?
        .to_owned();

    let (path, mut config) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    // Navigate to agents.list array.
    let agents_list = config
        .pointer_mut("/agents/list")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| ErrorShape::internal("config missing agents.list array"))?;

    // Check for duplicate.
    let exists = agents_list
        .iter()
        .any(|a| a.get("id").and_then(|v| v.as_str()) == Some(&id));
    if exists {
        return Err(ErrorShape {
            code: "conflict".to_owned(),
            message: format!("agent with id `{id}` already exists"),
            details: None,
            retryable: false,
            retry_after_ms: 0,
        });
    }

    // Build agent entry from params.
    let mut entry = serde_json::json!({ "id": id });
    if let Some(obj) = entry.as_object_mut() {
        for field in &["default", "system", "name", "channels", "avatar"] {
            if let Some(val) = params.get(*field) {
                obj.insert((*field).to_owned(), val.clone());
            }
        }
        // Handle "model": wrap plain string as { "primary": value }
        if let Some(model_val) = params.get("model") {
            if let Some(s) = model_val.as_str() {
                obj.insert(
                    "model".to_owned(),
                    serde_json::json!({ "primary": s }),
                );
            } else {
                obj.insert("model".to_owned(), model_val.clone());
            }
        }
        // Handle "toolset": place inside model object.
        if let Some(toolset_val) = params.get("toolset") {
            let ts = if let Some(arr) = toolset_val.as_array() {
                arr.first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("standard")
            } else {
                toolset_val.as_str().unwrap_or("standard")
            };
            if !obj.contains_key("model") {
                obj.insert("model".to_owned(), serde_json::json!({}));
            }
            if let Some(m) = obj.get_mut("model").and_then(|v| v.as_object_mut()) {
                m.insert("toolset".to_owned(), serde_json::json!(ts));
            }
        }
    }

    agents_list.push(entry);

    // Write back.
    let json_str =
        serde_json::to_string_pretty(&config).map_err(|e| ErrorShape::internal(e.to_string()))?;
    std::fs::write(&path, json_str).map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({
        "id": id,
        "created": true,
        "note": "restart gateway to activate"
    }))
}

pub async fn agents_update(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: id"))?
        .to_owned();

    let (path, mut config) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let agents_list = config
        .pointer_mut("/agents/list")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| ErrorShape::internal("config missing agents.list array"))?;

    let agent_entry = agents_list
        .iter_mut()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(&id))
        .ok_or_else(|| ErrorShape::not_found(format!("agent `{id}` not found")))?;

    // Update fields if provided.
    if let Some(obj) = agent_entry.as_object_mut() {
        for field in &["default", "system", "name", "channels", "avatar"] {
            if let Some(val) = params.get(*field) {
                obj.insert((*field).to_owned(), val.clone());
            }
        }

        // Handle "model" specially: if it's a plain string, wrap as { "primary": value }
        // to match the config schema where model is an object.
        if let Some(model_val) = params.get("model") {
            if let Some(s) = model_val.as_str() {
                if obj.get("model").is_some_and(|v| v.is_object()) {
                    if let Some(m) = obj.get_mut("model").and_then(|v| v.as_object_mut()) {
                        m.insert("primary".to_owned(), serde_json::json!(s));
                    }
                } else {
                    obj.insert(
                        "model".to_owned(),
                        serde_json::json!({ "primary": s }),
                    );
                }
            } else {
                obj.insert("model".to_owned(), model_val.clone());
            }
        }

        // Handle "toolset": place inside model object (model.toolset in config schema).
        // Frontend sends toolset as ["full"] or ["standard"] array -- extract first element.
        if let Some(toolset_val) = params.get("toolset") {
            let ts = if let Some(arr) = toolset_val.as_array() {
                arr.first()
                    .and_then(|v| v.as_str())
                    .unwrap_or("standard")
            } else {
                toolset_val.as_str().unwrap_or("standard")
            };
            // Ensure model object exists and set toolset inside it.
            if !obj.contains_key("model") {
                obj.insert("model".to_owned(), serde_json::json!({}));
            }
            if let Some(m) = obj.get_mut("model").and_then(|v| v.as_object_mut()) {
                m.insert("toolset".to_owned(), serde_json::json!(ts));
            }
        }
    }

    let json_str =
        serde_json::to_string_pretty(&config).map_err(|e| ErrorShape::internal(e.to_string()))?;
    std::fs::write(&path, json_str).map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({
        "id": id,
        "updated": true
    }))
}

pub async fn agents_delete(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let id = params
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: id"))?
        .to_owned();

    let (path, mut config) = crate::cmd::config_json::load_config_json()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let agents_list = config
        .pointer_mut("/agents/list")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| ErrorShape::internal("config missing agents.list array"))?;

    let original_len = agents_list.len();
    agents_list.retain(|a| a.get("id").and_then(|v| v.as_str()) != Some(&id));

    if agents_list.len() == original_len {
        return Err(ErrorShape::not_found(format!("agent `{id}` not found")));
    }

    let json_str =
        serde_json::to_string_pretty(&config).map_err(|e| ErrorShape::internal(e.to_string()))?;
    std::fs::write(&path, json_str).map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({
        "id": id,
        "deleted": true
    }))
}
