//! Root-side `HeartbeatHost` implementation (crate-split trait inversion).
//!
//! `rsclaw-heartbeat` defines the `HeartbeatHost` trait so it can reach the
//! agent registry + graceful-shutdown coordinator without depending on the
//! agent/gateway runtime crates. This adapter wraps the concrete
//! `AgentRegistry` + `ShutdownCoordinator` and keeps `AgentMessage`
//! construction + `resolve_flash_model_for` on the root side, where those
//! types live.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;

use crate::agent::registry::{AgentMessage, AgentRegistry};
use crate::gateway::shutdown::ShutdownCoordinator;

pub struct RuntimeHeartbeatHost {
    pub registry: Arc<AgentRegistry>,
    pub shutdown: Option<ShutdownCoordinator>,
    pub defaults: crate::config::schema::AgentDefaults,
}

#[async_trait::async_trait]
impl rsclaw_heartbeat::HeartbeatHost for RuntimeHeartbeatHost {
    fn agent_flash_model(&self, agent_id: &str) -> Option<String> {
        let h = self.registry.get(agent_id).ok()?;
        crate::agent::runtime::resolve_flash_model_for(&h.config, &self.defaults)
    }

    fn agent_providers(
        &self,
        agent_id: &str,
    ) -> Option<Arc<rsclaw_provider::registry::ProviderRegistry>> {
        let h = self.registry.get(agent_id).ok()?;
        Some(h.providers.clone())
    }

    fn agent_workspace(&self, agent_id: &str) -> Option<String> {
        let h = self.registry.get(agent_id).ok()?;
        h.config.workspace.clone()
    }

    async fn send_heartbeat(
        &self,
        agent_id: &str,
        session_key: &str,
        content: &str,
    ) -> anyhow::Result<()> {
        let handle = self
            .registry
            .get(agent_id)
            .map_err(|e| anyhow!("agent not found: {e:#}"))?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = AgentMessage {
            session_key: session_key.to_owned(),
            text: content.to_owned(),
            channel: "heartbeat".to_owned(),
            peer_id: "heartbeat".to_owned(),
            chat_id: String::new(),
            reply_tx,
            task_id: None,
            context_id: None,
            event_tx: None,
            cancel_token: None,
            input_request_tx: None,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
            account: None,
        };

        handle
            .tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("heartbeat send failed: agent channel closed"))?;

        // Wait for reply with timeout (5 minutes), matching the original behaviour.
        match tokio::time::timeout(Duration::from_secs(300), reply_rx).await {
            Ok(Ok(_reply)) => Ok(()),
            Ok(Err(_)) => Ok(()), // reply_tx dropped — agent finished without explicit reply
            Err(_) => Err(anyhow!("heartbeat timed out after 300s")),
        }
    }

    fn is_draining(&self) -> bool {
        self.shutdown.as_ref().map(|s| s.is_draining()).unwrap_or(false)
    }

    fn begin_work(&self) -> Option<Box<dyn std::any::Any + Send>> {
        self.shutdown
            .as_ref()
            .map(|s| Box::new(s.begin_work()) as Box<dyn std::any::Any + Send>)
    }
}
