use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::{
    sync::{RwLock, mpsc},
    time::{Duration, timeout},
};

use crate::acp::gateway::{
    EventFrame, Features, GatewayFrame, HelloOk, Policy, RequestFrame, RequestFrameType,
    ServerInfo, Snapshot, client_mode,
};

pub const GATEWAY_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone)]
pub struct GatewayClient {
    inner: Arc<RwLock<GwClientInner>>,
}

struct GwClientInner {
    write_tx: mpsc::Sender<String>,
    read_rx: mpsc::Receiver<String>,
    pending_requests: HashMap<String, mpsc::Sender<Result<serde_json::Value>>>,
    next_req_id: u64,
    connected: bool,
    server_info: Option<ServerInfo>,
    features: Option<Features>,
    policy: Option<Policy>,
    snapshot: Option<Snapshot>,
    session_id: Option<String>,
    agent_id: Option<String>,
}

impl GatewayClient {
    pub async fn connect(
        url: &str,
        client_id: &str,
        client_version: &str,
        token: Option<&str>,
        device_auth: Option<DeviceAuthParams>,
    ) -> Result<Self> {
        eprintln!("[GWClient] Connecting to {}", url);
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("Failed to connect to gateway")?;

        eprintln!("[GWClient] WebSocket connected!");

        let (mut write_half, mut read_half) = ws.split();

        let (write_tx, write_rx) = mpsc::channel::<String>(256);
        let (read_tx, read_rx) = mpsc::channel::<String>(256);
        let read_tx_for_loop = read_tx.clone();

        tokio::spawn(async move {
            let mut rx = write_rx;
            while let Some(msg) = rx.recv().await {
                if write_half
                    .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = read_half.next().await {
                if let Ok(tokio_tungstenite::tungstenite::Message::Text(text)) = msg {
                    if read_tx_for_loop.send(text.to_string()).await.is_err() {
                        break;
                    }
                }
            }
        });

        let inner = Arc::new(RwLock::new(GwClientInner {
            write_tx,
            read_rx,
            pending_requests: HashMap::new(),
            next_req_id: 1,
            connected: false,
            server_info: None,
            features: None,
            policy: None,
            snapshot: None,
            session_id: None,
            agent_id: None,
        }));

        let client = Self { inner };

        let nonce = client.wait_challenge().await?;
        client
            .send_connect(client_id, client_version, token, device_auth, &nonce)
            .await?;

        Ok(client)
    }

    async fn wait_challenge(&self) -> Result<String> {
        eprintln!("[GWClient] Waiting for challenge...");

        let result = timeout(Duration::from_secs(10), async {
            loop {
                let msg = {
                    let mut inner = self.inner.write().await;
                    inner.read_rx.recv().await
                };

                match msg {
                    Some(text) => {
                        eprintln!("[GWClient] ← {}", text);

                        if let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) {
                            if frame.get("type").and_then(|v| v.as_str()) == Some("event") {
                                if let Some(event) = frame.get("event").and_then(|v| v.as_str()) {
                                    if event == "connect.challenge" {
                                        if let Some(params) = frame.get("payload") {
                                            if let Some(nonce) =
                                                params.get("nonce").and_then(|v| v.as_str())
                                            {
                                                return Ok(nonce.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => return Err(anyhow::anyhow!("Connection closed")),
                }
            }
        })
        .await;

        match result {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("Challenge timeout")),
        }
    }

    async fn send_connect(
        &self,
        client_id: &str,
        client_version: &str,
        token: Option<&str>,
        device_auth: Option<DeviceAuthParams>,
        _nonce: &str,
    ) -> Result<()> {
        let mut params = serde_json::json!({
            "minProtocol": DEFAULT_PROTOCOL_VERSION,
            "maxProtocol": DEFAULT_PROTOCOL_VERSION,
            "client": {
                "id": client_id,
                "version": client_version,
                "platform": std::env::consts::OS,
                "mode": client_mode::CLI,
            },
        });

        if let Some(token) = token {
            params["auth"] = serde_json::json!({ "token": token });
        }

        if let Some(auth) = device_auth {
            params["device"] = serde_json::json!({
                "id": auth.device_id,
                "publicKey": auth.public_key,
                "signature": auth.signature,
                "signedAt": auth.signed_at,
                "nonce": auth.nonce,
            });
        }

        let response = self.call("connect", Some(params)).await?;

        let hello: HelloOk =
            serde_json::from_value(response).context("Failed to parse hello response")?;

        let server_version = hello.server.version.clone();
        let server_conn_id = hello.server.conn_id.clone();

        {
            let mut inner = self.inner.write().await;
            inner.connected = true;
            inner.server_info = Some(hello.server);
            inner.features = Some(hello.features);
            inner.policy = Some(hello.policy);
            inner.snapshot = Some(hello.snapshot);
        }

        eprintln!(
            "[GWClient] Connected! Server: {} v{}",
            server_version, server_conn_id
        );

        Ok(())
    }

    pub async fn spawn_agent(
        &self,
        cwd: &str,
        model: Option<&str>,
        agent_label: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut params = serde_json::json!({ "cwd": cwd });

        if let Some(m) = model {
            params["model"] = serde_json::json!(m);
        }
        if let Some(label) = agent_label {
            params["agentLabel"] = serde_json::json!(label);
        }

        let response = self.call("agent.spawn", Some(params)).await?;

        if let Some(agent_id) = response.get("agentId").and_then(|v| v.as_str()) {
            let mut inner = self.inner.write().await;
            inner.agent_id = Some(agent_id.to_string());
            inner.session_id = response
                .get("sessionId")
                .and_then(|v| v.as_str().map(String::from));
        }

        Ok(response)
    }

    pub async fn send_prompt(
        &self,
        prompt: &str,
        _model: Option<&str>,
    ) -> Result<serde_json::Value> {
        let session_id = {
            let inner = self.inner.read().await;
            inner.session_id.clone().context("No active session")?
        };

        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": prompt,
        });

        let response = self.call("agent.prompt", Some(params)).await?;
        Ok(response)
    }

    pub async fn session_send(&self, session_id: &str, prompt: &str) -> Result<serde_json::Value> {
        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": prompt,
        });

        let response = self.call("session.send", Some(params)).await?;
        Ok(response)
    }

    pub async fn session_subscribe(&self, session_id: &str) -> Result<()> {
        let params = serde_json::json!({ "sessionId": session_id });
        self.call("session.subscribe", Some(params)).await?;
        Ok(())
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let response = self.call("agent.list", None).await?;

        let agents = response["agents"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|a| AgentInfo {
                        id: a["id"].as_str().unwrap_or("").to_string(),
                        label: a["label"].as_str().map(String::from),
                        status: a["status"].as_str().unwrap_or("").to_string(),
                        model: a["model"].as_str().map(String::from),
                        session_id: a["sessionId"].as_str().map(String::from),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(agents)
    }

    pub async fn kill_agent(&self, agent_id: &str) -> Result<()> {
        let params = serde_json::json!({ "agentId": agent_id });
        self.call("agent.kill", Some(params)).await?;
        Ok(())
    }

    pub async fn is_connected(&self) -> bool {
        self.inner.read().await.connected
    }

    pub async fn server_info(&self) -> Option<ServerInfo> {
        self.inner.read().await.server_info.clone()
    }

    pub async fn snapshot(&self) -> Option<Snapshot> {
        self.inner.read().await.snapshot.clone()
    }

    pub async fn session_id(&self) -> Option<String> {
        self.inner.read().await.session_id.clone()
    }

    pub async fn agent_id(&self) -> Option<String> {
        self.inner.read().await.agent_id.clone()
    }

    async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let (tx, mut rx) = mpsc::channel(1);

        let req_id = {
            let mut inner = self.inner.write().await;
            let id = format!("{}-{}", method, inner.next_req_id);
            inner.next_req_id += 1;
            inner.pending_requests.insert(id.clone(), tx);
            id
        };

        let frame = RequestFrame {
            frame_type: RequestFrameType::Req,
            id: req_id.clone(),
            method: method.to_string(),
            params,
        };

        let json = serde_json::to_string(&frame)?;
        eprintln!("[GWClient] → {}", json);

        {
            let inner = self.inner.read().await;
            inner.write_tx.send(json).await?;
        }

        match timeout(GATEWAY_TIMEOUT, rx.recv()).await {
            Ok(Some(result)) => result,
            Ok(None) => Err(anyhow::anyhow!("Channel closed")),
            Err(_) => Err(anyhow::anyhow!("Request timed out: {}", method)),
        }
    }

    pub async fn receive_loop(&self) {
        loop {
            let msg = {
                let mut inner = self.inner.write().await;
                inner.read_rx.recv().await
            };

            match msg {
                Some(text) => {
                    eprintln!("[GWClient] ← {}", text);

                    if let Ok(frame) = serde_json::from_str::<GatewayFrame>(&text) {
                        match frame {
                            GatewayFrame::Res(response) => {
                                if let Some(tx) = {
                                    let mut inner = self.inner.write().await;
                                    inner.pending_requests.remove(&response.id)
                                } {
                                    let value = if response.ok {
                                        Ok(response.payload.unwrap_or(serde_json::Value::Null))
                                    } else {
                                        Err(anyhow::anyhow!(
                                            "Gateway error: {}",
                                            response.error.map(|e| e.message).unwrap_or_default()
                                        ))
                                    };
                                    let _ = tx.send(value).await;
                                }
                            }
                            GatewayFrame::Event(event) => {
                                self.handle_event(event).await;
                            }
                            _ => {}
                        }
                    }
                }
                None => break,
            }
        }
    }

    async fn handle_event(&self, event: EventFrame) {
        eprintln!("[GWClient] Event: {}", event.event);
    }

    pub async fn close(self) -> Result<()> {
        let _inner = self.inner.read().await;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DeviceAuthParams {
    pub device_id: String,
    pub public_key: String,
    pub signature: String,
    pub signed_at: u64,
    pub nonce: String,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub label: Option<String>,
    pub status: String,
    pub model: Option<String>,
    pub session_id: Option<String>,
}
