use crate::ws::{
    dispatch::{MethodCtx, MethodResult},
    types::ErrorShape,
};

fn load_search_cfg() -> Option<crate::config::schema::MemorySearchConfig> {
    crate::config::load()
        .ok()
        .and_then(|c| c.raw.memory_search.clone())
}

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

    let base = crate::config::loader::base_dir();
    let data_dir = base.join("var/data");
    let model_dir = {
        let zh = base.join("models/bge-small-zh");
        let en = base.join("models/bge-small-en");
        if zh.join("config.json").exists() { zh } else { en }
    };
    let tier = crate::sys::detect_memory_tier();
    let search_cfg = load_search_cfg();
    let mut mem = crate::agent::memory::MemoryStore::open(
        &data_dir,
        Some(&model_dir),
        tier,
        search_cfg.as_ref(),
    )
    .await
    .map_err(|e| ErrorShape::internal(e.to_string()))?;
    let results = mem
        .search(query, scope, top_k)
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

    let base = crate::config::loader::base_dir();
    let data_dir = base.join("var/data");
    let model_dir = {
        let zh = base.join("models/bge-small-zh");
        let en = base.join("models/bge-small-en");
        if zh.join("config.json").exists() { zh } else { en }
    };
    let tier = crate::sys::detect_memory_tier();
    let search_cfg = load_search_cfg();
    let mut mem = crate::agent::memory::MemoryStore::open(
        &data_dir,
        Some(&model_dir),
        tier,
        search_cfg.as_ref(),
    )
    .await
    .map_err(|e| ErrorShape::internal(e.to_string()))?;

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
    };
    mem.add(doc)
        .await
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "id": id, "stored": true }))
}

pub async fn memory_status(_ctx: MethodCtx) -> MethodResult {
    let base = crate::config::loader::base_dir();
    let data_dir = base.join("var/data");
    let model_dir = {
        let zh = base.join("models/bge-small-zh");
        let en = base.join("models/bge-small-en");
        if zh.join("config.json").exists() { zh } else { en }
    };
    let tier = crate::sys::detect_memory_tier();
    let search_cfg = load_search_cfg();
    let mem = crate::agent::memory::MemoryStore::open(
        &data_dir,
        Some(&model_dir),
        tier,
        search_cfg.as_ref(),
    )
    .await
    .map_err(|e| ErrorShape::internal(e.to_string()))?;
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
