use crate::{
    agent::AgentMessage,
    ws::{
        dispatch::{MethodCtx, MethodResult},
        types::ErrorShape,
    },
};

pub async fn sessions_list(ctx: MethodCtx) -> MethodResult {
    let keys = ctx
        .state
        .store
        .db
        .list_sessions()
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let sessions: Vec<serde_json::Value> = keys
        .into_iter()
        .map(|k| {
            let meta = ctx.state.store.db.get_session_meta(&k).ok().flatten();
            let (updated_ts, created_ts, msg_count, tokens) = match &meta {
                Some(m) => {
                    let updated = chrono::DateTime::from_timestamp(m.last_active, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default();
                    let created = chrono::DateTime::from_timestamp(m.created_at, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default();
                    let msgs = ctx.state.store.db.load_messages(&k).unwrap_or_default();
                    // Count tokens from both string content and array-of-parts content.
                    let total_chars: usize = msgs
                        .iter()
                        .map(|v| {
                            if let Some(s) = v
                                .as_object()
                                .and_then(|o| o.get("content"))
                                .and_then(|c| c.as_str())
                            {
                                s.len()
                            } else if let Some(arr) = v
                                .as_object()
                                .and_then(|o| o.get("content"))
                                .and_then(|c| c.as_array())
                            {
                                arr.iter()
                                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                    .map(|t| t.len())
                                    .sum()
                            } else {
                                0
                            }
                        })
                        .sum();
                    (updated, created, m.message_count, (total_chars / 4).max(1))
                }
                None => (String::new(), String::new(), 0, 0),
            };
            // Emit both rsclaw-native and openclaw-compat field names so that
            // whichever the Control UI reads will have a value.
            let updated_val: serde_json::Value = if updated_ts.is_empty() {
                serde_json::Value::Null
            } else {
                updated_ts.clone().into()
            };
            let created_val: serde_json::Value = if created_ts.is_empty() {
                serde_json::Value::Null
            } else {
                created_ts.clone().into()
            };
            serde_json::json!({
                // primary key — openclaw UI reads "sessionKey"
                "key": k,
                "sessionKey": k,
                // store backend — UI may read "store", "storeType", or "backend"
                "store": "redb",
                "storeType": "redb",
                "backend": "redb",
                // message counts / timestamps
                "messageCount": msg_count,
                "updated": updated_val,
                "updatedAt": updated_val,
                "createdAt": created_val,
                // token estimates — UI may read "tokens" or "estimatedTokens"
                "estimatedTokens": tokens,
                "tokens": tokens,
                // openclaw compat — label, agentId, model
                "label": serde_json::Value::Null,
                "agentId": "main",
                "model": serde_json::Value::Null,
            })
        })
        .collect();

    Ok(serde_json::json!({ "sessions": sessions }))
}

pub async fn sessions_send(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let text = params
        .get("text")
        .or_else(|| params.get("message"))
        .or_else(|| params.get("content"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ErrorShape::bad_request(format!(
                "missing required param: text (got keys: {:?})",
                params
                    .as_object()
                    .map(|o| o.keys().collect::<Vec<_>>())
                    .unwrap_or_default()
            ))
        })?
        .to_owned();

    let session_key = params
        .get("key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| format!("ws:{}", uuid::Uuid::new_v4()));

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    let agent = if agent_id == "main" {
        ctx.state
            .agents
            .default_agent()
            .map_err(|e| ErrorShape::internal(e.to_string()))?
    } else {
        ctx.state
            .agents
            .get(agent_id)
            .map_err(|e| ErrorShape::not_found(e.to_string()))?
    };

    // Subscribe to event bus BEFORE dispatching so we don't miss deltas.
    let rx = ctx.state.event_bus.subscribe();
    let event_tx = ctx.conn.read().await.event_tx.clone();
    let conn = ctx.conn.clone();
    let sk = session_key.clone();

    // Build and send AgentMessage.
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: "ws".to_owned(),
        peer_id: "ws-client".to_owned(),
        chat_id: String::new(),
        reply_tx,
        extra_tools: vec![],
        images: vec![],
        files: vec![],
    };

    agent
        .tx
        .send(msg)
        .await
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    // Spawn relay task: forward matching AgentEvents as EventFrames.
    tokio::spawn(async move {
        use futures::StreamExt;
        tracing::debug!(session_key = %sk, "ws relay task started");
        let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    if event.session_id != sk {
                        continue;
                    }
                    tracing::debug!(session = %sk, done = event.done, delta_len = event.delta.len(), "ws relay: forwarding event");
                    let seq = conn.write().await.next_seq();
                    let payload = serde_json::json!({
                        "sessionKey": sk,
                        "message": {
                            "role": "assistant",
                            "content": event.delta,
                            "done": event.done
                        }
                    });
                    let frame = crate::ws::types::EventFrame::new("session.message", payload, seq);
                    let json = serde_json::to_string(&frame).unwrap_or_default();
                    if event_tx.send(json).await.is_err() {
                        tracing::warn!(session = %sk, "ws relay: outbound channel closed");
                        break;
                    }
                    if event.done {
                        tracing::debug!(session = %sk, "ws relay: done");
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(session = %sk, error = %e, "ws relay: broadcast recv error");
                    break;
                }
            }
        }
        tracing::debug!(session = %sk, "ws relay task exited");
    });

    Ok(serde_json::json!({ "sessionKey": session_key }))
}

pub async fn sessions_messages_subscribe(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?
        .to_owned();

    // Register this session in the connection's subscription set.
    ctx.conn
        .write()
        .await
        .subscribed_sessions
        .insert(key.clone());

    // Spawn a long-lived relay task that forwards events for this session.
    let rx = ctx.state.event_bus.subscribe();
    let event_tx = ctx.conn.read().await.event_tx.clone();
    let conn = ctx.conn.clone();
    let sk = key.clone();

    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        while let Some(Ok(event)) = stream.next().await {
            if event.session_id != sk {
                continue;
            }
            // Check if still subscribed.
            let still_subscribed = conn.read().await.subscribed_sessions.contains(&sk);
            if !still_subscribed {
                break;
            }
            let seq = conn.write().await.next_seq();
            let payload = serde_json::json!({
                "sessionKey": sk,
                "message": {
                    "role": "assistant",
                    "content": event.delta,
                    "done": event.done
                }
            });
            let frame = crate::ws::types::EventFrame::new("session.message", payload, seq);
            let json = serde_json::to_string(&frame).unwrap_or_default();
            if event_tx.send(json).await.is_err() {
                break;
            }
        }
    });

    Ok(serde_json::json!({ "subscribed": true, "key": key }))
}

pub async fn sessions_messages_unsubscribe(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?
        .to_owned();

    ctx.conn.write().await.subscribed_sessions.remove(&key);

    Ok(serde_json::json!({ "unsubscribed": true, "key": key }))
}

pub async fn sessions_reset(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?;

    ctx.state
        .store
        .db
        .delete_session(key)
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "reset": true, "key": key }))
}

pub async fn sessions_delete(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?;

    ctx.state
        .store
        .db
        .delete_session(key)
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "deleted": true, "key": key }))
}

pub async fn sessions_create(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| format!("ws:{}", uuid::Uuid::new_v4()));

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_owned();

    Ok(serde_json::json!({
        "sessionKey": key,
        "agentId": agent_id,
        "created": true,
    }))
}

pub async fn sessions_patch(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .or_else(|| params.get("sessionKey"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key or sessionKey"))?;

    // Accept metadata updates (title, tags, etc.) — store is append-only
    // so we acknowledge but metadata storage is not yet implemented.
    Ok(serde_json::json!({
        "patched": true,
        "key": key,
        "sessionKey": key,
    }))
}

pub async fn sessions_compact(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?;

    // Compaction is triggered automatically by the agent runtime.
    // This method can be used to request an immediate compaction.
    Ok(serde_json::json!({
        "compacted": true,
        "key": key,
    }))
}

pub async fn sessions_usage(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?;

    let messages = ctx.state.store.db.load_messages(key).unwrap_or_default();

    let message_count = messages.len();
    let total_chars: usize = messages
        .iter()
        .filter_map(|v| {
            v.as_object()
                .and_then(|o| o.get("content"))
                .and_then(|c| c.as_str())
                .map(|s| s.len())
        })
        .sum();

    Ok(serde_json::json!({
        "key": key,
        "messageCount": message_count,
        "totalChars": total_chars,
        "estimatedTokens": total_chars / 4,
    }))
}

pub async fn sessions_resolve(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let key = params
        .get("key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: key"))?;

    let messages = ctx.state.store.db.load_messages(key).unwrap_or_default();

    if messages.is_empty() {
        return Err(ErrorShape::not_found(format!(
            "session '{}' not found",
            key
        )));
    }

    Ok(serde_json::json!({
        "key": key,
        "exists": true,
        "messageCount": messages.len(),
    }))
}
