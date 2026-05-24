use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

pub async fn memory_search(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let query = params["query"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing query"))?;
    let scope = params["scope"].as_str();
    let top_k = params["limit"].as_u64().unwrap_or(10) as usize;

    // Use the shared MemoryStore from AppState so hybrid search hits the
    // BM25 index the gateway already maintains, and so we never compete
    // for the redb lock by opening a second handle.
    let Some(mem_arc) = ctx.state.memory.clone() else {
        return Err(ErrorShape::internal("memory store not available"));
    };
    let mut mem = mem_arc.lock().await;
    let results = mem
        .search_hybrid(query, scope, top_k)
        .await
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let docs: Vec<serde_json::Value> = results
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "scope": d.scope,
                "kind": d.kind,
                "text": d.text,
                "createdAt": d.created_at,
                "accessedAt": d.accessed_at,
                "accessCount": d.access_count,
                "importance": d.importance,
            })
        })
        .collect();
    Ok(serde_json::json!({ "results": docs }))
}

pub async fn memory_store(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;
    let text = params["text"]
        .as_str()
        .ok_or_else(|| ErrorShape::bad_request("missing text"))?;
    let scope = params["scope"].as_str().unwrap_or("global").to_owned();
    let kind = params["kind"].as_str().unwrap_or("note").to_owned();
    let id = params["id"]
        .as_str()
        .map(|s| s.to_owned())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let Some(mem_arc) = ctx.state.memory.clone() else {
        return Err(ErrorShape::internal("memory store not available"));
    };

    let doc = crate::agent::memory::MemoryDoc {
        id: id.clone(),
        scope,
        kind,
        text: text.to_owned(),
        vector: vec![],
        created_at: 0,
        accessed_at: 0,
        access_count: 0,
        importance: 0.0,
        tier: Default::default(),
        abstract_text: None,
        overview_text: None,
        tags: vec![],
        pinned: false,
    };
    // `add_off_lock` embeds outside the lock then briefly re-acquires it to
    // commit — better than `add()` for the shared gateway store. The shared
    // store has its BM25 index wired so the dual-write happens automatically.
    crate::agent::memory::add_off_lock(&mem_arc, doc)
        .await
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "id": id, "stored": true }))
}

pub async fn memory_status(ctx: MethodCtx) -> MethodResult {
    let Some(mem_arc) = ctx.state.memory.clone() else {
        return Err(ErrorShape::internal("memory store not available"));
    };
    let mem = mem_arc.lock().await;
    let count = mem
        .count()
        .await
        .map_err(|e| ErrorShape::internal(e.to_string()))?;
    Ok(serde_json::json!({ "documents": count }))
}

pub async fn plugins_list(_ctx: MethodCtx) -> MethodResult {
    let config = crate::config::load().map_err(|e| ErrorShape::internal(e.to_string()))?;
    let entries = config.ext.plugins.as_ref().and_then(|p| p.entries.as_ref());
    let plugins: Vec<serde_json::Value> = match entries {
        Some(map) => map
            .iter()
            .map(|(name, entry)| {
                serde_json::json!({
                    "id": name,
                    "enabled": entry.enabled.unwrap_or(true),
                })
            })
            .collect(),
        None => vec![],
    };
    Ok(serde_json::json!({ "plugins": plugins }))
}

pub async fn hooks_list(_ctx: MethodCtx) -> MethodResult {
    let config = crate::config::load().map_err(|e| ErrorShape::internal(e.to_string()))?;
    let mappings = config
        .ops
        .hooks
        .as_ref()
        .and_then(|h| h.mappings.as_deref())
        .unwrap_or(&[]);
    let list: Vec<serde_json::Value> = mappings
        .iter()
        .map(|m| {
            serde_json::json!({
                "match": {
                    "path": m.match_.path,
                    "method": m.match_.method,
                },
                "action": format!("{:?}", m.action),
                "agentId": m.agent_id,
                "sessionKey": m.session_key,
            })
        })
        .collect();
    Ok(serde_json::json!({
        "enabled": config.ops.hooks.as_ref().map(|h| h.enabled).unwrap_or(false),
        "mappings": list,
    }))
}
