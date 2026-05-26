//! CapAgentManager — owns four respawnable driver slots, one per
//! `AgentKind`. Each slot fronts an actor task that owns a
//! `Box<dyn cap_rs::driver::Driver>`.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use cap_rs::core::{AgentEvent, ClientFrame, Content, RiskLevel};
use cap_rs::driver::Driver;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};

use super::{bridge, permission};

/// Which coding agent a `tool_cap` call dispatches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AgentKind {
    Claudecode,
    Openclaude,
    Opencode,
    Codex,
}

impl AgentKind {
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "claudecode" => Some(Self::Claudecode),
            "openclaude" => Some(Self::Openclaude),
            "opencode" => Some(Self::Opencode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claudecode => "claudecode",
            Self::Openclaude => "openclaude",
            Self::Opencode => "opencode",
            Self::Codex => "codex",
        }
    }
}

pub(crate) struct Reply {
    pub text: String,
}

pub(crate) enum ToolCapRequest {
    Prompt {
        task: String,
        reply: oneshot::Sender<Result<Reply>>,
    },
}

type Slot = Arc<RwLock<Option<mpsc::Sender<ToolCapRequest>>>>;

pub(crate) struct CapAgentManager {
    claudecode: Slot,
    openclaude: Slot,
    opencode: Slot,
    codex: Slot,
    bus: broadcast::Sender<crate::events::AgentEvent>,
}

impl CapAgentManager {
    pub(crate) fn new(bus: broadcast::Sender<crate::events::AgentEvent>) -> Self {
        Self {
            claudecode: Arc::new(RwLock::new(None)),
            openclaude: Arc::new(RwLock::new(None)),
            opencode: Arc::new(RwLock::new(None)),
            codex: Arc::new(RwLock::new(None)),
            bus,
        }
    }

    fn slot(&self, kind: AgentKind) -> Slot {
        match kind {
            AgentKind::Claudecode => Arc::clone(&self.claudecode),
            AgentKind::Openclaude => Arc::clone(&self.openclaude),
            AgentKind::Opencode => Arc::clone(&self.opencode),
            AgentKind::Codex => Arc::clone(&self.codex),
        }
    }

    /// Submit a prompt, await its `Done`, return collected reply text.
    pub(crate) async fn dispatch(
        &self,
        kind: AgentKind,
        task: String,
        cwd: std::path::PathBuf,
    ) -> Result<Reply> {
        let tx = self.ensure_actor(kind, cwd).await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ToolCapRequest::Prompt {
            task,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow!("cap actor for {} closed", kind.as_str()))?;
        reply_rx.await.map_err(|_| anyhow!("cap actor dropped reply"))?
    }

    async fn ensure_actor(
        &self,
        kind: AgentKind,
        cwd: std::path::PathBuf,
    ) -> Result<mpsc::Sender<ToolCapRequest>> {
        let slot = self.slot(kind);
        {
            let g = slot.read().await;
            if let Some(tx) = g.as_ref() {
                return Ok(tx.clone());
            }
        }
        let mut g = slot.write().await;
        if let Some(tx) = g.as_ref() {
            return Ok(tx.clone());
        }
        let driver = spawn_driver(kind, &cwd).await?;
        let (tx, rx) = mpsc::channel::<ToolCapRequest>(8);
        let bus = self.bus.clone();
        let slot_for_actor = Arc::clone(&slot);
        tokio::spawn(actor_loop(kind, driver, rx, bus, slot_for_actor));
        *g = Some(tx.clone());
        Ok(tx)
    }
}

async fn spawn_driver(kind: AgentKind, cwd: &std::path::Path) -> Result<Box<dyn Driver>> {
    // Real driver construction goes in Task 6. Placeholder here so the
    // module is wirable end-to-end in tests via fn-injection.
    let _ = (kind, cwd);
    Err(anyhow!("spawn_driver: real drivers wired in Task 6"))
}

async fn actor_loop(
    kind: AgentKind,
    mut driver: Box<dyn Driver>,
    mut rx: mpsc::Receiver<ToolCapRequest>,
    bus: broadcast::Sender<crate::events::AgentEvent>,
    slot: Slot,
) {
    let agent_id = kind.as_str();
    while let Some(req) = rx.recv().await {
        match req {
            ToolCapRequest::Prompt { task, reply } => {
                let session_id = format!("cap-{agent_id}-{}", uuid::Uuid::new_v4());
                if let Err(e) = driver
                    .send(ClientFrame::Prompt {
                        content: vec![Content::text(task)],
                    })
                    .await
                {
                    let _ = reply.send(Err(anyhow!("cap send: {e}")));
                    continue;
                }
                let mut reply_buf = String::new();
                let outcome = run_turn(
                    driver.as_mut(),
                    &bus,
                    &session_id,
                    agent_id,
                    &mut reply_buf,
                )
                .await;
                let _ = reply.send(outcome.map(|()| Reply { text: reply_buf }));
            }
        }
    }
    let _ = driver.shutdown().await;
    let mut g = slot.write().await;
    *g = None;
}

async fn run_turn(
    driver: &mut dyn Driver,
    bus: &broadcast::Sender<crate::events::AgentEvent>,
    session_id: &str,
    agent_id: &str,
    reply_buf: &mut String,
) -> Result<()> {
    loop {
        let Some(event) = driver.next_event().await else {
            return Err(anyhow!("cap driver exited mid-turn"));
        };
        if let AgentEvent::PermissionRequest { req_id, tool, risk_level, .. } = &event {
            let resp = permission::auto_approve(req_id, tool, *risk_level);
            if let Err(e) = driver.send(resp).await {
                return Err(anyhow!("cap permission send: {e}"));
            }
            continue;
        }
        let mut sinks = bridge::Sinks {
            agent_event: Some(bus),
            reply: Some(reply_buf),
            session_id,
            agent_id,
        };
        let done = bridge::dispatch(&event, &mut sinks);
        if done {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use cap_rs::core::{StopReason, TextChannel, Usage};
    use cap_rs::driver::DriverError;
    use std::collections::VecDeque;

    struct FakeDriver {
        events: VecDeque<AgentEvent>,
    }

    impl FakeDriver {
        fn new(events: Vec<AgentEvent>) -> Self {
            Self { events: events.into() }
        }
    }

    #[async_trait]
    impl Driver for FakeDriver {
        async fn send(&mut self, _frame: ClientFrame) -> Result<(), DriverError> {
            Ok(())
        }
        async fn next_event(&mut self) -> Option<AgentEvent> {
            self.events.pop_front()
        }
        async fn shutdown(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    fn done() -> AgentEvent {
        AgentEvent::Done {
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn text(t: &str) -> AgentEvent {
        AgentEvent::TextChunk {
            msg_id: "m".into(),
            text: t.into(),
            channel: TextChannel::Assistant,
        }
    }

    #[tokio::test]
    async fn run_turn_collects_text_until_done() {
        let mut driver = FakeDriver::new(vec![text("Hello "), text("world"), done()]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "Hello world");
    }

    #[tokio::test]
    async fn run_turn_auto_approves_permission() {
        let mut driver = FakeDriver::new(vec![
            AgentEvent::PermissionRequest {
                req_id: "p1".into(),
                tool: "shell".into(),
                intent: serde_json::json!({}),
                scope: cap_rs::core::PermissionScope::Execute,
                risk_level: RiskLevel::Low,
            },
            text("ok"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ok");
    }

    #[tokio::test]
    async fn run_turn_surfaces_mid_turn_exit() {
        let mut driver = FakeDriver::new(vec![text("partial")]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        let err = run_turn(&mut driver, &bus, "sess", "claudecode", &mut reply)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited mid-turn"));
    }
}
