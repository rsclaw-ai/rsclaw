//! A2A HTTP client — sends tasks to remote agents.

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{JsonRpcRequest, JsonRpcResponse};

pub struct A2aClient {
    client: Client,
}

impl Default for A2aClient {
    fn default() -> Self {
        Self::new()
    }
}

impl A2aClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Send a `tasks/send` request to `base_url` targeting `agent_id`.
    ///
    /// `base_url` is the remote gateway base (e.g. "http://host:18888").
    /// The A2A endpoint is `{base_url}/api/v1/a2a`.
    pub async fn send_task(
        &self,
        base_url: &str,
        agent_id: &str,
        text: &str,
        session_key: &str,
        auth_token: Option<&str>,
    ) -> Result<String> {
        let task_id = Uuid::new_v4().to_string();
        let rpc = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: json!(task_id),
            method: "tasks/send".to_owned(),
            params: json!({
                "id": task_id,
                "sessionId": session_key,
                "message": {
                    "role": "user",
                    "parts": [{ "type": "text", "text": text }]
                },
                "metadata": if agent_id.is_empty() { json!({}) } else { json!({ "agentId": agent_id }) }
            }),
        };

        let url = format!("{}/api/v1/a2a", base_url.trim_end_matches('/'));

        let mut req = self.client.post(&url).json(&rpc);
        if let Some(token) = auth_token {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.context("A2A HTTP request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("A2A remote {status}: {body}"));
        }

        let rpc_resp: JsonRpcResponse = resp.json().await.context("parse A2A response")?;

        if let Some(err) = rpc_resp.error {
            return Err(anyhow!("A2A RPC error {}: {}", err.code, err.message));
        }

        let result = rpc_resp
            .result
            .ok_or_else(|| anyhow!("A2A: empty result"))?;
        extract_reply_text(&result)
    }
}

/// Extract the first text part from a completed task result.
fn extract_reply_text(result: &Value) -> Result<String> {
    // Try artifacts[0].parts[0].text
    if let Some(artifacts) = result["artifacts"].as_array() {
        for art in artifacts {
            if let Some(parts) = art["parts"].as_array() {
                for part in parts {
                    if part["type"] == "text"
                        && let Some(t) = part["text"].as_str()
                    {
                        return Ok(t.to_owned());
                    }
                }
            }
        }
    }
    // Fallback: status.message parts
    if let Some(parts) = result["status"]["message"]["parts"].as_array() {
        for part in parts {
            if part["type"] == "text"
                && let Some(t) = part["text"].as_str()
            {
                return Ok(t.to_owned());
            }
        }
    }
    Err(anyhow!("A2A: no text part found in result"))
}
