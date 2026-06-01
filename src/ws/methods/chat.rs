use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    agent::AgentMessage,
    events::AgentEvent,
    gateway::preparse::{PreparseOrigin, try_preparse_locally},
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

    // Slash-command preparse bypass — channels (telegram/discord/etc.) and
    // cron already short-circuit `/abort /clear /new /status /ls /cat /ss
    // /loop /watch /task /model /plugin …` here so they never enter the
    // agent's mpsc. The WS path skipped this layer, so desktop / web users
    // saw their slash commands stuck behind whatever long turn the agent
    // was streaming — `/abort` typed mid-turn fired against an
    // already-finished session, `/status` hung.
    //
    // We run the same preparse pipeline on WS-origin text. On match, the
    // reply is published directly to event_bus as two AgentEvents (delta
    // then done) that the relay spawn below picks up and forwards to the
    // client as standard `chat` frames — identical wire format to a real
    // agent reply, so the frontend renders it the same way.
    if let Some(reply) =
        try_preparse_locally(&text, &agent, "ws", "ws-client", PreparseOrigin::User).await
    {
        // Synthesize delta + done. We publish before returning so the relay
        // task (spawned below) is already subscribed when these arrive.
        let _ = ctx.state.event_bus.send(AgentEvent {
            session_id: session_key.clone(),
            agent_id: agent.id.clone(),
            delta: reply.text.clone(),
            done: false,
            files: vec![],
            images: vec![],
            tool_log: vec![],
            question: None,
            channel: None,
        });
        let _ = ctx.state.event_bus.send(AgentEvent {
            session_id: session_key.clone(),
            agent_id: agent.id.clone(),
            delta: String::new(),
            done: true,
            files: vec![],
            images: reply.images,
            tool_log: vec![],
            question: None,
            channel: None,
        });

        // Same relay spawn shape as the agent-dispatch path — the events
        // we just published are visible to it via the `rx` subscription
        // taken above.
        let inflight_guard = ctx.state.shutdown.begin_work();
        let shutdown_for_relay = ctx.state.shutdown.clone();
        tokio::spawn(async move {
            let _inflight_guard = inflight_guard;
            use futures::StreamExt;
            let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
            loop {
                tokio::select! {
                    () = shutdown_for_relay.notified() => break,
                    next = stream.next() => {
                        let Some(Ok(event)) = next else { break };
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
                                "files": event.files,
                                "images": event.images,
                                "toolLog": event.tool_log,
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
                }
            }
        });

        return Ok(serde_json::json!({
            "runId": run_id,
            "sessionKey": session_key,
            "status": "preparse",
        }));
    }

    // Extract `[file:/abs/path]` markers — desktop / WS clients embed
    // image and file references via this syntax. Without this hop the
    // markers travel as raw text and the LLM never sees the image; only
    // the HTTP send_message handler in server/mod.rs ran extract_file_refs
    // before, so the WS / desktop path was silently dropping every image.
    let (text, file_images, file_files) = crate::agent::registry::extract_file_refs(&text);

    // Dispatch message to agent.
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = AgentMessage {
        session_key: session_key.clone(),
        text,
        channel: "ws".to_owned(),
        peer_id: "ws-client".to_owned(),
        chat_id: String::new(),
        reply_tx,
        task_id: None,
        context_id: None,
        event_tx: None,
        cancel_token: None,
        input_request_tx: None,
        extra_tools: vec![],
        images: file_images,
        files: file_files,
        account: None,
    };

    tracing::info!(
        agent_id = %agent.id,
        session_key = %session_key,
        run_id = %run_id,
        tx_capacity = agent.tx.capacity(),
        tx_max_capacity = agent.tx.max_capacity(),
        "chat.send: dispatching to agent runtime"
    );
    if let Err(e) = agent.tx.send(msg).await {
        tracing::error!(
            agent_id = %agent.id,
            session_key = %session_key,
            error = %e,
            "chat.send: agent.tx.send failed"
        );
        return Err(ErrorShape::internal(e.to_string()));
    }
    tracing::info!(
        agent_id = %agent.id,
        session_key = %session_key,
        run_id = %run_id,
        "chat.send: dispatched ok, waiting for runtime"
    );

    // Track this chat in the gateway's inflight count so a graceful restart
    // waits for the relay task to finish (event.done or stream end). The
    // guard is moved into the spawned task and drops on task exit.
    let inflight_guard = ctx.state.shutdown.begin_work();
    let shutdown_for_relay = ctx.state.shutdown.clone();

    // Spawn relay task: emit OpenClaw-format "chat" events back to the WS
    // client that initiated the request.  The payload uses the `event:chat`
    // wire format expected by the WebUI Chat component.
    // NOTE(H-18): This relay runs alongside the auto-relay in handshake.rs.
    // Both are needed — see comment there for rationale.
    tokio::spawn(async move {
        let _inflight_guard = inflight_guard;
        use futures::StreamExt;
        let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
        loop {
            tokio::select! {
                () = shutdown_for_relay.notified() => break,
                next = stream.next() => {
                    let Some(Ok(event)) = next else { break };
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
                            "files": event.files,
                            "images": event.images,
                            "toolLog": event.tool_log,
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

    // Filter out compaction summary messages — they are internal-only
    // and should not be displayed to the user.
    let filtered: Vec<_> = all_messages
        .into_iter()
        .filter(|v| !is_compaction_message(v))
        .map(crate::provider::redact_rsclaw_hidden_value)
        .collect();

    let messages: Vec<_> = if filtered.len() > limit {
        filtered[filtered.len() - limit..].to_vec()
    } else {
        filtered
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

    // Two-layer abort for the session on all agents:
    //   1. abort_flag — cooperative, polled at stream/tool boundaries.
    //   2. cancel_token — hard cancel, drops a wedged turn immediately so the
    //      single-threaded queue isn't blocked behind a stalled await.
    for agent in ctx.state.agents.all() {
        if let Ok(mut flags) = agent.abort_flags.write() {
            let flag = flags
                .entry(sk.to_string())
                .or_insert_with(|| Arc::new(AtomicBool::new(false)));
            flag.store(true, Ordering::SeqCst);
        }
        if let Ok(tokens) = agent.cancel_tokens.read() {
            if let Some(token) = tokens.get(sk) {
                token.cancel();
            }
        }
    }

    Ok(serde_json::json!({
        "aborted": true,
        "sessionKey": sk
    }))
}

/// `chat.permission_response` — desktop UI replies to a
/// `PermissionRequest` event surfaced by the computer_use VLM driver.
///
/// Params:
///   `requestId` (string) — id minted by the driver
///   `decision`  (string) — one of "allow_once" / "allow_session" /
///                          "allow_always" / "deny"
///
/// Returns `{ resolved: true }` on success, or `{ resolved: false }`
/// when the id is unknown (request already timed out or was cancelled).
pub async fn chat_permission_response(ctx: MethodCtx) -> MethodResult {
    let params = ctx
        .req
        .params
        .as_ref()
        .ok_or_else(|| ErrorShape::bad_request("missing params"))?;

    let request_id = params
        .get("requestId")
        .or_else(|| params.get("request_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: requestId"))?
        .to_owned();

    let decision_str = params
        .get("decision")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ErrorShape::bad_request("missing required param: decision"))?;

    use crate::computer::permission::{PermissionDecision, PermissionStore as _};
    let decision = match decision_str {
        "allow_once" => PermissionDecision::AllowOnce,
        "allow_session" => PermissionDecision::AllowSession,
        "allow_always" => PermissionDecision::AllowAlways,
        "deny" => PermissionDecision::Deny,
        other => {
            return Err(ErrorShape::bad_request(format!(
                "invalid decision `{other}` (expected: allow_once, allow_session, allow_always, deny)"
            )));
        }
    };

    // Persist the decision so subsequent runs in the same session /
    // future sessions see it. The driver's polling loop also reads
    // from the same store, so this synchronises both wake-up paths.
    let agent_id = params
        .get("agentId")
        .and_then(|v| v.as_str())
        .unwrap_or("default");
    let app = params.get("app").and_then(|v| v.as_str()).unwrap_or("");
    if let Err(e) = ctx
        .state
        .computer_permission
        .record(agent_id, app, decision)
        .await
    {
        tracing::warn!(error = %e, "computer_permission.record failed");
    }

    let resolved = ctx
        .state
        .computer_permission
        .resolve_pending_request(&request_id, decision)
        .await;

    Ok(serde_json::json!({
        "resolved": resolved,
        "requestId": request_id,
    }))
}

// is_compaction_message moved to
// crate::agent::compaction::is_compaction_message
use crate::agent::compaction::is_compaction_message;
