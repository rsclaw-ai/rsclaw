//! Push notification dispatcher.
//!
//! Subscribes to the per-task event bus and POSTs HS256-signed payloads to
//! the configured webhook endpoints, with bounded retry.

use std::sync::Arc;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::{
    event::{AgentEvent, TaskEventBus},
    store::TaskStore,
};

/// HMAC-SHA256(token, body) → base64.
pub fn sign_payload(token: &str, body: &[u8]) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(token.as_bytes()).expect("hmac");
    mac.update(body);
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

pub struct PushDispatcher {
    store: Arc<TaskStore>,
    bus: TaskEventBus,
    client: reqwest::Client,
}

impl PushDispatcher {
    pub fn new(store: Arc<TaskStore>, bus: TaskEventBus) -> Self {
        Self {
            store,
            bus,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest"),
        }
    }

    /// Watch a task's event stream. Spawn-and-forget; the spawn exits when
    /// the broadcast bus closes (final status event seen).
    pub fn watch(self: Arc<Self>, task_id: String) {
        let me = self;
        tokio::spawn(async move {
            let mut rx = me.bus.subscribe(&task_id);
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let is_final =
                            matches!(&ev, AgentEvent::Status { final_: true, .. });
                        if let Err(e) = me.dispatch(&task_id, &ev).await {
                            warn!(err = %e, "push dispatch failed");
                        }
                        if is_final {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }

    async fn dispatch(&self, task_id: &str, ev: &AgentEvent) -> anyhow::Result<()> {
        let configs = self.store.list_push_configs(task_id)?;
        if configs.is_empty() {
            return Ok(());
        }
        let body = serde_json::to_vec(&ev.to_wire_event())?;
        for cfg in configs {
            let sig = sign_payload(&cfg.token, &body);
            for attempt in 1..=3u32 {
                let resp = self
                    .client
                    .post(&cfg.url)
                    .header("Content-Type", "application/json")
                    .header("X-A2A-Signature", &sig)
                    .header("X-A2A-Task-Id", task_id)
                    .body(body.clone())
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        info!(task_id, url = %cfg.url, "push delivered");
                        break;
                    }
                    Ok(r) => warn!(task_id, url = %cfg.url, status = %r.status(), attempt, "push non-2xx"),
                    Err(e) => warn!(task_id, url = %cfg.url, attempt, err = %e, "push failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
            }
        }
        Ok(())
    }
}
