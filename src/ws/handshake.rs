//! WebSocket handshake — server-initiated challenge/connect/hello-ok flow,
//! device token persistence, and the main receive loop.

use std::{collections::HashMap, sync::Arc, time::SystemTime};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::FromRequest;
use axum::response::IntoResponse;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

use super::{
    conn::{ConnHandle, ConnId},
    dispatch::{self, MethodCtx},
    types::{
        AuthCredentials, ConnectParams, DeviceRecord, ErrorShape, EventFrame, FeaturesInfo,
        HelloAuth, HelloOkPayload, InboundFrame, PROTOCOL_VERSION, PolicyInfo, ResFrame,
        ServerInfo,
    },
};
use crate::server::AppState;

// ---------------------------------------------------------------------------
// DeviceStore — persists device tokens to a JSON file on disk.
// ---------------------------------------------------------------------------

pub struct DeviceStore {
    tokens: RwLock<HashMap<String, DeviceRecord>>,
    path: std::path::PathBuf,
}

impl DeviceStore {
    pub fn new(path: std::path::PathBuf) -> Self {
        let mut map = HashMap::new();
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(loaded) = serde_json::from_str::<HashMap<String, DeviceRecord>>(&raw)
        {
            map = loaded;
        }
        Self {
            tokens: RwLock::new(map),
            path,
        }
    }

    pub async fn is_valid_device_token(&self, token: &str) -> bool {
        self.tokens
            .read()
            .await
            .values()
            .any(|r| r.device_token == token)
    }

    pub async fn issue_token(&self, device_id: Option<String>) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let record = DeviceRecord {
            device_token: token.clone(),
            device_id: device_id.clone(),
            created_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let key = device_id.unwrap_or_else(|| token.clone());
        self.tokens.write().await.insert(key, record);
        self.persist().await;
        token
    }

    async fn persist(&self) {
        let guard = self.tokens.read().await;
        if let Ok(json) = serde_json::to_string_pretty(&*guard) {
            let _ = std::fs::write(&self.path, json);
            // SECURITY: restrict file permissions to owner-only
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ws_handler — Axum upgrade entry point
// ---------------------------------------------------------------------------

pub async fn ws_handler(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// Combined handler for root "/": WS upgrade if requested, otherwise info page.
pub async fn root_or_ws_handler(
    headers: axum::http::HeaderMap,
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
) -> axum::response::Response {
    // Check if this is a WebSocket upgrade request
    let is_ws = headers
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_ws {
        let ws = match axum::extract::ws::WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws,
            Err(e) => return e.into_response(),
        };
        ws.on_upgrade(move |socket| handle_socket(socket, state))
            .into_response()
    } else {
        root_handler().await.into_response()
    }
}

/// Fallback for plain HTTP GET on root — browsers get an info page instead of
/// the WebSocket upgrade error.
pub async fn root_handler() -> impl IntoResponse {
    axum::response::Html(format!(
        "<html><body>\
        <h2>rsclaw gateway v{}</h2>\
        <p>WebSocket endpoint. Connect with a compatible client.</p>\
        <p><a href=\"/api/v1/health\">Health</a></p>\
        </body></html>",
        env!("RSCLAW_BUILD_VERSION"),
    ))
}

// ---------------------------------------------------------------------------
// handle_socket — full connection lifecycle
// ---------------------------------------------------------------------------

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut write_half, mut read_half) = socket.split();

    // Outbound channel: handlers and broadcast tasks send serialized JSON here;
    // a dedicated writer task forwards them to the WebSocket.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<String>(256);

    // Writer task.
    let write_task = tokio::spawn(async move {
        while let Some(text) = outbound_rx.recv().await {
            if futures::SinkExt::send(&mut write_half, Message::Text(text.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 1. Send connect.challenge event.
    let nonce = uuid::Uuid::new_v4().to_string();
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let challenge = EventFrame::new("connect.challenge", json!({ "nonce": nonce, "ts": ts }), 0);
    if send_frame(&outbound_tx, &challenge).await.is_err() {
        return;
    }

    // 2. Wait for the first text frame — must be a req with method="connect".
    let connect_params: ConnectParams;
    let req_id: String;
    loop {
        match read_half.next().await {
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<InboundFrame>(&text.to_string()) {
                    Ok(InboundFrame::Req(req)) if req.method == "connect" => {
                        req_id = req.id.clone();
                        connect_params = req
                            .params
                            .as_ref()
                            .and_then(|v| serde_json::from_value::<ConnectParams>(v.clone()).ok())
                            .unwrap_or_default();
                        break;
                    }
                    Ok(InboundFrame::Req(req)) => {
                        let err = ResFrame::err(
                            req.id,
                            ErrorShape::bad_request("expected method=connect as first request"),
                        );
                        let _ = send_serialized(&outbound_tx, &err).await;
                        drop(outbound_tx);
                        let _ = write_task.await;
                        return;
                    }
                    Err(e) => {
                        let err = ResFrame::err("0", ErrorShape::bad_request(e.to_string()));
                        let _ = send_serialized(&outbound_tx, &err).await;
                        drop(outbound_tx);
                        let _ = write_task.await;
                        return;
                    }
                }
            }
            Some(Ok(Message::Ping(_))) => continue,
            Some(Ok(Message::Close(_))) | None => {
                drop(outbound_tx);
                let _ = write_task.await;
                return;
            }
            _ => continue,
        }
    }

    // 3. Validate protocol version.
    let client_min = connect_params.min_protocol.unwrap_or(PROTOCOL_VERSION);
    let client_max = connect_params.max_protocol.unwrap_or(PROTOCOL_VERSION);
    if client_max < PROTOCOL_VERSION || client_min > PROTOCOL_VERSION {
        let err = ResFrame::err(
            &req_id,
            ErrorShape {
                code: "protocol_mismatch".to_owned(),
                message: format!(
                    "server requires protocol {PROTOCOL_VERSION}, client offered {client_min}-{client_max}"
                ),
                details: Some(json!({
                    "serverMin": PROTOCOL_VERSION,
                    "serverMax": PROTOCOL_VERSION,
                })),
                retryable: false,
                retry_after_ms: 0,
            },
        );
        let _ = send_serialized(&outbound_tx, &err).await;
        drop(outbound_tx);
        let _ = write_task.await;
        return;
    }

    // 4. Validate auth.
    let expected_token = state.live.gateway.read().await.auth_token.clone();
    if let Some(expected) = expected_token {
        let auth: &AuthCredentials = connect_params.auth.as_ref().unwrap_or(&AuthCredentials {
            token: None,
            device_token: None,
            password: None,
        });

        let mut authed = false;

        // Check device token first.
        if let Some(ref dt) = auth.device_token {
            authed = state.devices.is_valid_device_token(dt).await;
        }

        // Fall back to bearer token.
        if !authed && let Some(ref t) = auth.token {
            authed = t == &expected;
        }

        if !authed {
            warn!("ws: auth failed");
            let err = ResFrame::err(&req_id, ErrorShape::unauthorized("auth_failed"));
            let _ = send_serialized(&outbound_tx, &err).await;
            drop(outbound_tx);
            let _ = write_task.await;
            return;
        }
    }

    // 5. Issue device token and send hello-ok.
    let device_token = state
        .devices
        .issue_token(connect_params.device_id.clone())
        .await;
    info!(
        "ws: issued device token for device_id={:?}",
        connect_params.device_id
    );

    let agent_count = state.agents.len();
    let hello = HelloOkPayload {
        kind: "hello-ok",
        protocol: PROTOCOL_VERSION,
        server: ServerInfo {
            name: "rsclaw".to_owned(),
            version: env!("RSCLAW_BUILD_VERSION").to_owned(),
            agent_count,
        },
        features: FeaturesInfo {
            streaming: true,
            multi_agent: agent_count > 1,
            memory: false,
        },
        auth: HelloAuth { device_token },
        policy: PolicyInfo {
            max_message_length: 100_000,
            rate_limit_rpm: 120,
            tick_interval_ms: 15_000,
        },
    };

    let hello_value = serde_json::to_value(&hello).unwrap_or_default();
    let res = ResFrame::ok(&req_id, hello_value);
    if send_serialized(&outbound_tx, &res).await.is_err() {
        drop(outbound_tx);
        let _ = write_task.await;
        return;
    }

    // 6. Send presence event with all agents.
    let agents_list: Vec<serde_json::Value> = state
        .agents
        .all()
        .iter()
        .map(|h| {
            json!({
                "agentId": h.id,
                "status": "online"
            })
        })
        .collect();
    let presence = EventFrame::new("presence", json!({ "agents": agents_list }), 0);
    let _ = send_frame(&outbound_tx, &presence).await;

    // 7. Register this connection in the ConnRegistry.
    let conn_id: ConnId = uuid::Uuid::new_v4().to_string();
    let conn = Arc::new(RwLock::new(ConnHandle::new(
        conn_id.clone(),
        outbound_tx.clone(),
    )));
    state.ws_conns.register(Arc::clone(&conn)).await;
    info!("ws: connection {conn_id} registered");

    // 8. Auto-relay: forward ALL AgentEvents to this WS connection. OpenClaw WebUI
    //    sends messages via HTTP and receives events via WS.
    {
        let rx = state.event_bus.subscribe();
        let relay_tx = outbound_tx.clone();
        let relay_conn = Arc::clone(&conn);
        let relay_id = conn_id.clone();
        tokio::spawn(async move {
            use futures::StreamExt as _;
            info!(conn = %relay_id, "ws auto-relay started");
            let mut stream = tokio_stream::wrappers::BroadcastStream::new(rx);
            while let Some(result) = stream.next().await {
                match result {
                    Ok(event) => {
                        debug!(
                            conn = %relay_id,
                            session = %event.session_id,
                            done = event.done,
                            delta_len = event.delta.len(),
                            "ws auto-relay event"
                        );
                        let seq = relay_conn.write().await.next_seq();
                        let payload = if event.done {
                            serde_json::json!({
                                "runId": format!("auto-{}", event.session_id),
                                "sessionKey": event.session_id,
                                "type": "done",
                                "role": "assistant",
                            })
                        } else {
                            serde_json::json!({
                                "runId": format!("auto-{}", event.session_id),
                                "sessionKey": event.session_id,
                                "type": "text_delta",
                                "delta": event.delta,
                                "role": "assistant",
                            })
                        };
                        let frame = EventFrame::new("chat", payload, seq);
                        let json = serde_json::to_string(&frame).unwrap_or_default();
                        if relay_tx.send(json).await.is_err() {
                            info!(conn = %relay_id, "ws auto-relay: outbound closed");
                            break;
                        }
                    }
                    Err(e) => {
                        warn!(conn = %relay_id, error = %e, "ws auto-relay: recv error");
                    }
                }
            }
            info!(conn = %relay_id, "ws auto-relay exited");
        });
    }

    // 9. Main dispatch loop.
    let mut rate_limiter = super::rate_limit::RateLimiter::default_write_limiter();
    loop {
        match read_half.next().await {
            Some(Ok(Message::Text(text))) => {
                let raw = text.to_string();
                debug!(len = raw.len(), "ws recv");
                match serde_json::from_str::<InboundFrame>(&raw) {
                    Ok(InboundFrame::Req(req)) => {
                        debug!(method = %req.method, id = %req.id, "ws dispatch");
                        let id = req.id.clone();

                        // Rate-limit write operations.
                        if super::rate_limit::RateLimiter::is_write_method(&req.method)
                            && !rate_limiter.check()
                        {
                            warn!(method = %req.method, "ws: rate limited");
                            let err = ResFrame::err(
                                id,
                                ErrorShape {
                                    code: "rate_limited".to_owned(),
                                    message: "too many write requests; try again later".to_owned(),
                                    details: None,
                                    retryable: true,
                                    retry_after_ms: 2000,
                                },
                            );
                            let _ = send_serialized(&outbound_tx, &err).await;
                            continue;
                        }

                        let ctx = MethodCtx {
                            req,
                            state: state.clone(),
                            conn: Arc::clone(&conn),
                        };
                        let result = dispatch::dispatch(ctx).await;
                        let frame = match result {
                            Ok(p) => ResFrame::ok(id, p),
                            Err(e) => ResFrame::err(id, e),
                        };
                        let _ = send_serialized(&outbound_tx, &frame).await;
                    }
                    Err(e) => {
                        warn!("ws parse error: {e} — raw: {}", &raw[..raw.len().min(200)]);
                        let err = ResFrame::err("0", ErrorShape::bad_request(e.to_string()));
                        let _ = send_serialized(&outbound_tx, &err).await;
                    }
                }
            }
            Some(Ok(Message::Ping(_))) => {
                // Handled at the tungstenite layer; no action needed.
            }
            Some(Ok(Message::Close(_))) | None => break,
            _ => {}
        }
    }

    // 9. Cleanup.
    state.ws_conns.unregister(&conn_id).await;
    info!("ws: connection {conn_id} disconnected");
    drop(outbound_tx);
    let _ = write_task.await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn send_frame(
    tx: &mpsc::Sender<String>,
    frame: &EventFrame,
) -> Result<(), mpsc::error::SendError<String>> {
    let text = serde_json::to_string(frame).unwrap_or_default();
    tx.send(text).await
}

async fn send_serialized<T: serde::Serialize>(
    tx: &mpsc::Sender<String>,
    value: &T,
) -> Result<(), mpsc::error::SendError<String>> {
    let text = serde_json::to_string(value).unwrap_or_default();
    tx.send(text).await
}
