use crate::{
    agent::AgentMessage,
    ws::{
        dispatch::{MethodCtx, MethodResult},
        types::{ErrorShape, EventFrame},
    },
};

/// `chat.send` — the primary method the OpenClaw WebUI uses to send messages.
///
/// Returns `{ runId, sessionKey, status: "started" }` immediately.
/// Streaming events are pushed as `event: "chat"` frames with
/// `type: "text_delta" | "done"`.
pub async fn chat_send(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let text = params
        .get("message")
        .or_else(|| params.get("text"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: message"))?
        .to_owned();

    let session_key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| format!("ws:{}", uuid::Uuid::new_v4()));

    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let run_id = uuid::Uuid::new_v4().to_string();

    let agent = if agent_id == "default" {
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

    // Subscribe to event_bus BEFORE dispatch.
    let rx = ctx.state.event_bus.subscribe();
    let event_tx = ctx.conn.read().await.event_tx.clone();
    let conn = ctx.conn.clone();
    let sk = session_key.clone();
    let rid = run_id.clone();

    // Dispatch message to agent.
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

    // Spawn relay task: emit OpenClaw-format "chat" events back to the WS
    // client that initiated the request.  The payload uses the `event:chat`
    // wire format expected by the WebUI Chat component.
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        while let Some(Ok(event)) = stream.next().await {
            if event.session_id != sk {
                continue;
            }

            let conn_seq = conn.write().await.next_seq();

            let payload = if event.done {
                serde_json::json!({
                    "runId": rid,
                    "sessionKey": sk,
                    "type": "done",
                    "role": "assistant",
                })
            } else {
                serde_json::json!({
                    "runId": rid,
                    "sessionKey": sk,
                    "type": "text_delta",
                    "delta": event.delta,
                    "role": "assistant",
                })
            };
            let frame = EventFrame::new("chat", payload, conn_seq);
            let json = serde_json::to_string(&frame).unwrap_or_default();
            if event_tx.send(json).await.is_err() {
                break;
            }
            if event.done {
                break;
            }
        }
    });

    Ok(serde_json::json!({
        "runId": run_id,
        "sessionKey": session_key,
        "status": "started"
    }))
}

/// `chat.inject` — append a synthetic message to session history.
pub async fn chat_inject(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let session_key = params
        .get("sessionKey")
        .or_else(|| params.get("key"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: sessionKey"))?;

    let role = params
        .get("role")
        .and_then(|v| v.as_str())
        .unwrap_or("assistant");

    let content = params
        .get("content")
        .or_else(|| params.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let msg = serde_json::json!({
        "role": role,
        "content": content,
    });

    ctx.state
        .store
        .db
        .append_message(session_key, &msg)
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    Ok(serde_json::json!({ "ok": true }))
}

pub async fn chat_history(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let sk = params
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: sessionKey"))?;

    let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(200) as usize;

    let all_messages = ctx
        .state
        .store
        .db
        .load_messages(sk)
        .map_err(|e| ErrorShape::internal(e.to_string()))?;

    let messages: Vec<_> = if all_messages.len() > limit {
        all_messages[all_messages.len() - limit..].to_vec()
    } else {
        all_messages
    };

    Ok(serde_json::json!({
        "sessionKey": sk,
        "messages": messages
    }))
}

pub async fn chat_abort(ctx: MethodCtx) -> MethodResult {
    let params = ctx.req.params.as_ref();

    let sk = params
        .and_then(|p| p.get("sessionKey"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    Ok(serde_json::json!({
        "aborted": true,
        "sessionKey": sk
    }))
}
