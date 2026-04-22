use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::{
    sync::{RwLock, mpsc},
    time::{Duration, timeout},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::acp::types::SessionId;

pub const GATEWAY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct GatewayClient {
    inner: Arc<RwLock<GatewayClientInner>>,
}

#[allow(dead_code)]
struct GatewayClientInner {
    write_tx: mpsc::Sender<String>,
    read_rx: mpsc::Receiver<String>,
    url: String,
    token: Option<String>,
    pending_requests: HashMap<String, mpsc::Sender<Result<serde_json::Value>>>,
    next_req_id: u64,
    initialized: bool,
    session_id: Option<SessionId>,
    agent_id: Option<String>,
}

impl GatewayClient {
    pub async fn connect(url: &str, token: Option<&str>) -> Result<Self> {
        let (ws, _) = connect_async(url)
            .await
            .context("Failed to connect to gateway")?;

        let (mut write_half, read_half) = ws.split();

        let (write_tx, write_rx) = mpsc::channel::<String>(256);
        let (read_tx, read_rx) = mpsc::channel::<String>(256);

        tokio::spawn(async move {
            let mut rx = write_rx;
            while let Some(msg) = rx.recv().await {
                if write_half.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            let mut read_half = read_half;
            let tx = read_tx;
            while let Some(Ok(Message::Text(text))) = read_half.next().await {
                if tx.send(text.to_string()).await.is_err() {
                    break;
                }
            }
        });

        let inner = Arc::new(RwLock::new(GatewayClientInner {
            write_tx,
            read_rx,
            url: url.to_string(),
            token: token.map(|s| s.to_string()),
            pending_requests: HashMap::new(),
            next_req_id: 1,
            initialized: false,
            session_id: None,
            agent_id: None,
        }));

        // Start receive_loop in background
        let inner_clone = inner.clone();
        tokio::spawn(async move {
            loop {
                let msg = {
                    let mut inner = inner_clone.write().await;
                    inner.read_rx.recv().await
                };

                match msg {
                    Some(text) => {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(id) = parsed.get("id").and_then(|v| v.as_str()) {
                                if let Some(tx) = {
                                    let mut inner = inner_clone.write().await;
                                    inner.pending_requests.remove(id)
                                } {
                                    let result = parsed
                                        .get("result")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null);
                                    let error = parsed.get("error");
                                    let value = if let Some(err) = error {
                                        Err(anyhow::anyhow!("{}", err))
                                    } else {
                                        Ok(result)
                                    };
                                    tx.send(value).await.ok();
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
        });

        Ok(Self { inner })
    }

    pub async fn authenticate(&self, _password: Option<&str>) -> Result<()> {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true,
            },
            "clientInfo": {
                "name": "rsclaw",
                "version": option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev"),
            },
        });

        self.call("initialize", params).await?;
        {
            let mut inner = self.inner.write().await;
            inner.initialized = true;
        }
        Ok(())
    }

    pub async fn spawn_agent(
        &self,
        cwd: &str,
        model: Option<&str>,
        agent_label: Option<&str>,
    ) -> Result<AgentSessionInfo> {
        let params = json!({
            "cwd": cwd,
            "model": model,
            "agentLabel": agent_label,
        });

        let response: serde_json::Value = self.call("agent.spawn", params).await?;

        let info = AgentSessionInfo {
            agent_id: response["agentId"].as_str().unwrap_or("").to_string(),
            session_id: response["sessionId"].as_str().unwrap_or("").to_string(),
        };

        {
            let mut inner = self.inner.write().await;
            inner.agent_id = Some(info.agent_id.clone());
            inner.session_id = Some(info.session_id.clone());
        }

        Ok(info)
    }

    pub async fn send_prompt(
        &self,
        prompt: &str,
        _model: Option<&str>,
    ) -> Result<serde_json::Value> {
        let session_id = {
            let inner = self.inner.read().await;
            inner.session_id.clone().context("No agent session")?
        };

        let params = json!({
            "sessionId": session_id,
            "prompt": prompt,
        });

        let response: serde_json::Value = self.call("agent.prompt", params).await?;
        Ok(response)
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>> {
        let response: serde_json::Value = self.call("agent.list", json!({})).await?;
        let agents = response["agents"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|a| AgentInfo {
                        id: a["id"].as_str().unwrap_or("").to_string(),
                        label: a["label"].as_str().map(|s| s.to_string()),
                        status: a["status"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(agents)
    }

    pub async fn kill_agent(&self, agent_id: &str) -> Result<()> {
        let params = json!({ "agentId": agent_id });
        self.call("agent.kill", params).await?;
        Ok(())
    }

    pub async fn is_connected(&self) -> bool {
        self.inner.read().await.initialized
    }

    pub async fn session_id(&self) -> Option<SessionId> {
        self.inner.read().await.session_id.clone()
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let (tx, mut rx) = mpsc::channel(1);

        let req_id = {
            let mut inner = self.inner.write().await;
            let id = format!("{}-{}", method, inner.next_req_id);
            inner.next_req_id += 1;
            inner.pending_requests.insert(id.clone(), tx);
            id
        };

        let request = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params,
        });

        let text = serde_json::to_string(&request)?;
        eprintln!("DEBUG: Sending: {}", text);

        {
            let inner = self.inner.read().await;
            inner.write_tx.send(text).await?;
        }

        match timeout(GATEWAY_TIMEOUT, rx.recv()).await {
            Ok(Some(result)) => result,
            Ok(None) => Err(anyhow::anyhow!("Request channel closed")),
            Err(_) => Err(anyhow::anyhow!("Gateway request timed out")),
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
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(id) = parsed.get("id").and_then(|v| v.as_str()) {
                            if let Some(tx) = {
                                let mut inner = self.inner.write().await;
                                inner.pending_requests.remove(id)
                            } {
                                let result = parsed
                                    .get("result")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null);
                                let error = parsed.get("error");
                                let value = if let Some(err) = error {
                                    Err(anyhow::anyhow!("{}", err))
                                } else {
                                    Ok(result)
                                };
                                tx.send(value).await.ok();
                            }
                        }
                    }
                }
                None => break,
            }
        }
    }

    pub async fn close(self) -> Result<()> {
        let _ = self.inner.read().await;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AgentSessionInfo {
    pub agent_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub label: Option<String>,
    pub status: String,
}
