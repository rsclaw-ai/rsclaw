//! CapAgentManager — owns four respawnable driver slots, one per
//! `AgentKind`. Each slot fronts an actor task that owns a
//! `Box<cap_rs::driver::Driver>`.
//!
//! Each `tool_cap` call returns IMMEDIATELY with `status: submitted` once
//! the prompt is queued. The actual coding-agent work runs in the actor
//! task; progress is reported live to the user's IM channel via
//! `notif_tx`, and the final summary is reinjected into the originating
//! agent's inbox via `inbox_tx` on a `:cap-followup` sub-session. This
//! mirrors the old `tool_acp*` behaviour (LLM ack-fast + background
//! delivery) — see `src/agent/tools_acp.rs` in commit 9deb237 for the
//! original pattern.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use cap_rs::core::{AgentEvent, ClientFrame, Content, RiskLevel};
use cap_rs::driver::Driver;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};

use super::{bridge, permission};
use crate::channel::OutboundMessage;
use crate::i18n;

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

    /// Display name used in i18n strings (e.g. "OpenCode", "Claude Code").
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::Claudecode => "Claude Code",
            Self::Openclaude => "OpenClaude",
            Self::Opencode => "OpenCode",
            Self::Codex => "Codex",
        }
    }
}

/// IM channel routing for live progress + completion notifications.
/// `lang` is a static i18n key (e.g. "en", "zh") — see `crate::i18n::resolve_lang`.
#[derive(Clone)]
pub(crate) struct NotifTarget {
    pub tx: broadcast::Sender<OutboundMessage>,
    pub target_id: String,
    pub is_group: bool,
    pub channel: String,
    pub lang: &'static str,
}

/// Where to reinject the completion as a follow-up agent message so the
/// LLM can act on the result (e.g. send_file). The follow-up runs on a
/// `:cap-followup` sub-session to avoid re-activating the user-visible
/// session.
#[derive(Clone)]
pub(crate) struct InboxTarget {
    pub agent_tx: mpsc::Sender<crate::agent::registry::AgentMessage>,
    pub session_key: String,
    pub channel: String,
    pub peer_id: String,
    pub chat_id: String,
}

/// Returned to the LLM immediately after the prompt is queued.
pub(crate) struct Submitted {
    pub session_id: String,
}

pub(crate) enum ToolCapRequest {
    Prompt {
        task: String,
        notif: Option<NotifTarget>,
        inbox: Option<InboxTarget>,
        /// Resolved as soon as the initial notification is sent — BEFORE
        /// the driver actually runs the prompt. The LLM gets back
        /// `Submitted { session_id }` and moves on.
        reply: oneshot::Sender<Result<Submitted>>,
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

    /// Queue a prompt on the agent's driver. Returns as soon as the
    /// initial notification fires; the actual driver run + completion
    /// happens asynchronously and is delivered via `notif` (IM live
    /// progress) and `inbox` (agent inbox reinjection on Done).
    pub(crate) async fn dispatch_async(
        &self,
        kind: AgentKind,
        task: String,
        cwd: std::path::PathBuf,
        notif: Option<NotifTarget>,
        inbox: Option<InboxTarget>,
    ) -> Result<Submitted> {
        let tx = self.ensure_actor(kind, cwd).await?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(ToolCapRequest::Prompt {
            task,
            notif,
            inbox,
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
    use cap_rs::driver::stream_json::ClaudeCodeDriver;

    let driver: Box<dyn Driver> = match kind {
        AgentKind::Claudecode => Box::new(
            ClaudeCodeDriver::builder(cwd)
                .dangerously_skip_permissions(true)
                .spawn()
                .await
                .map_err(|e| anyhow!("cap claudecode spawn: {e}"))?,
        ),
        AgentKind::Openclaude => Box::new(
            ClaudeCodeDriver::builder(cwd)
                .bin("openclaude")
                .dangerously_skip_permissions(true)
                .spawn()
                .await
                .map_err(|e| anyhow!("cap openclaude spawn: {e}"))?,
        ),
        AgentKind::Opencode => Box::new(
            ClaudeCodeDriver::opencode_builder(cwd)
                .spawn()
                .await
                .map_err(|e| anyhow!("cap opencode spawn: {e}"))?,
        ),
        AgentKind::Codex => {
            // Transitional path until cap-rs ships a stream-json driver
            // for codex; swap to ClaudeCodeDriver::codex_builder when
            // available. Box<dyn Driver> is the same shape.
            use cap_rs::driver::codex_mcp::CodexMcpDriver;
            Box::new(
                CodexMcpDriver::builder(cwd)
                    .spawn()
                    .await
                    .map_err(|e| anyhow!("cap codex spawn: {e}"))?,
            )
        }
    };
    Ok(driver)
}

/// Send a notification to the IM channel, if a target is configured.
/// Logs at warn! on send error so a transient channel issue doesn't kill
/// the whole turn.
fn push_notif(target: &NotifTarget, text: String) {
    let msg = OutboundMessage {
        target_id: target.target_id.clone(),
        is_group: target.is_group,
        text,
        reply_to: None,
        images: Vec::new(),
        files: Vec::new(),
        channel: Some(target.channel.clone()),
        account: None,
    };
    if let Err(e) = target.tx.send(msg) {
        tracing::warn!(target: "cap", err = %e, "cap notif send failed");
    }
}

async fn actor_loop(
    kind: AgentKind,
    mut driver: Box<dyn Driver>,
    mut rx: mpsc::Receiver<ToolCapRequest>,
    bus: broadcast::Sender<crate::events::AgentEvent>,
    slot: Slot,
) {
    let agent_id = kind.as_str();
    let display = kind.display_name();
    while let Some(req) = rx.recv().await {
        match req {
            ToolCapRequest::Prompt {
                task,
                notif,
                inbox,
                reply,
            } => {
                let session_id = format!("cap-{agent_id}-{}", uuid::Uuid::new_v4());

                // 1. Initial "submitted" notification to the user IM
                //    channel. Mirrors old tools_acp's first-message.
                if let Some(n) = &notif {
                    push_notif(
                        n,
                        i18n::t_fmt("acp_submitted", n.lang, &[("name", display)]),
                    );
                }

                // 2. Tell the LLM the prompt is queued. From here on the
                //    LLM is free; the result is delivered async.
                let _ = reply.send(Ok(Submitted {
                    session_id: session_id.clone(),
                }));

                // 3. Send the prompt frame to the driver.
                if let Err(e) = driver
                    .send(ClientFrame::Prompt {
                        content: vec![Content::text(task)],
                    })
                    .await
                {
                    let err_text = format!("cap send: {e}");
                    if let Some(n) = &notif {
                        push_notif(
                            n,
                            i18n::t_fmt(
                                "acp_error",
                                n.lang,
                                &[("name", display), ("error", &err_text)],
                            ),
                        );
                    }
                    // Driver send failure: actor probably dead; respawn
                    // by exiting the loop.
                    break;
                }

                // 4. Run the turn. `run_turn` streams ToolCallStart
                //    progress to `notif` and accumulates the final text
                //    into `reply_buf`.
                let mut reply_buf = String::new();
                let outcome = run_turn(
                    driver.as_mut(),
                    &bus,
                    &session_id,
                    agent_id,
                    notif.as_ref(),
                    &mut reply_buf,
                )
                .await;

                // 5. Completion / error notification + inbox reinjection.
                match &outcome {
                    Ok(()) => {
                        if let Some(n) = &notif {
                            let body = if reply_buf.is_empty() {
                                i18n::t_fmt(
                                    "acp_done_empty",
                                    n.lang,
                                    &[("status", "✅"), ("name", display)],
                                )
                            } else {
                                i18n::t_fmt(
                                    "acp_done_summary",
                                    n.lang,
                                    &[
                                        ("status", "✅"),
                                        ("name", display),
                                        ("count", "0"),
                                        ("summary", reply_buf.as_str()),
                                    ],
                                )
                            };
                            push_notif(n, body);
                        }
                        if let Some(ib) = &inbox {
                            inject_followup(ib, display, &reply_buf);
                        }
                    }
                    Err(e) => {
                        if let Some(n) = &notif {
                            push_notif(
                                n,
                                i18n::t_fmt(
                                    "acp_error",
                                    n.lang,
                                    &[("name", display), ("error", &e.to_string())],
                                ),
                            );
                        }
                    }
                }

                if outcome.is_err() {
                    // Driver died mid-turn → respawn.
                    break;
                }
            }
        }
    }
    let _ = driver.shutdown().await;
    let mut g = slot.write().await;
    *g = None;
}

/// Reinject the agent run summary back into the originating agent's
/// inbox as a follow-up message so the LLM can act on it (e.g. call
/// `send_file`, post a summary, schedule follow-ups). The follow-up
/// runs on a `:cap-followup` sub-session so the live user-visible
/// session does not get re-activated.
fn inject_followup(inbox: &InboxTarget, display: &str, summary: &str) {
    let followup_session = format!("{}:cap-followup", inbox.session_key);
    let text = if summary.is_empty() {
        format!("[{display} completed] Task finished.")
    } else {
        format!("[{display} completed] {summary}")
    };
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let msg = crate::agent::registry::AgentMessage {
        session_key: followup_session,
        text,
        channel: inbox.channel.clone(),
        peer_id: inbox.peer_id.clone(),
        chat_id: inbox.chat_id.clone(),
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
    let agent_tx = inbox.agent_tx.clone();
    // mpsc::Sender::send is async; the actor task is not blocked on a
    // tokio runtime that requires a different reactor, so a fresh spawn
    // is the safest way to forward without awaiting in the middle of
    // notification dispatch.
    tokio::spawn(async move {
        if let Err(e) = agent_tx.send(msg).await {
            tracing::warn!(target: "cap", err = %e, "cap followup inject failed");
        }
    });
}

async fn run_turn(
    driver: &mut dyn Driver,
    bus: &broadcast::Sender<crate::events::AgentEvent>,
    session_id: &str,
    agent_id: &str,
    notif: Option<&NotifTarget>,
    reply_buf: &mut String,
) -> Result<()> {
    loop {
        let Some(event) = driver.next_event().await else {
            return Err(anyhow!("cap driver exited mid-turn"));
        };
        if let AgentEvent::PermissionRequest {
            req_id,
            tool,
            risk_level,
            ..
        } = &event
        {
            let resp = permission::auto_approve(req_id, tool, *risk_level);
            if let Err(e) = driver.send(resp).await {
                return Err(anyhow!("cap permission send: {e}"));
            }
            continue;
        }
        if let AgentEvent::AskUser { ask_id, .. } = &event {
            let resp = ClientFrame::AskUserAnswer {
                ask_id: ask_id.clone(),
                value: serde_json::json!("cancelled"),
            };
            if let Err(e) = driver.send(resp).await {
                return Err(anyhow!("cap ask_user cancel: {e}"));
            }
            continue;
        }
        let mut sinks = bridge::Sinks {
            notif,
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
            Self {
                events: events.into(),
            }
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
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
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
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ok");
    }

    #[tokio::test]
    async fn run_turn_cancels_ask_user() {
        use cap_rs::core::AskKind;
        let mut driver = FakeDriver::new(vec![
            AgentEvent::AskUser {
                ask_id: "q1".into(),
                prompt: "Continue?".into(),
                ask_kind: AskKind::YesNo,
                options: vec![],
                timeout_seconds: None,
            },
            text("ok"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "ok");
    }

    #[tokio::test]
    async fn run_turn_surfaces_mid_turn_exit() {
        let mut driver = FakeDriver::new(vec![text("partial")]);
        let (bus, _rx) = broadcast::channel(8);
        let mut reply = String::new();
        let err = run_turn(&mut driver, &bus, "sess", "claudecode", None, &mut reply)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exited mid-turn"));
    }

    #[tokio::test]
    async fn run_turn_pushes_tool_call_progress_to_notif() {
        let mut driver = FakeDriver::new(vec![
            AgentEvent::ToolCallStart {
                call_id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "/etc/hosts"}),
            },
            text("done reading"),
            done(),
        ]);
        let (bus, _rx) = broadcast::channel(8);
        let (notif_tx, mut notif_rx) = broadcast::channel(8);
        let notif = NotifTarget {
            tx: notif_tx,
            target_id: "user@feishu".into(),
            is_group: false,
            channel: "feishu".into(),
            lang: "en",
        };
        let mut reply = String::new();
        run_turn(
            &mut driver,
            &bus,
            "sess",
            "claudecode",
            Some(&notif),
            &mut reply,
        )
        .await
        .unwrap();
        // Bridge should have pushed at least one OutboundMessage for the
        // ToolCallStart.
        let m = notif_rx.try_recv().expect("expected tool-call notif");
        assert!(
            m.text.contains("read_file"),
            "got notif: {:?}",
            m.text
        );
    }
}
