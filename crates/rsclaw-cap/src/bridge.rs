//! `cap_rs::AgentEvent` → rsclaw sinks dispatch.
//!
//! Three sink channels:
//! - `notif`: live progress notifications to the user's IM channel
//!   (used by the actor's async submission pattern).
//! - `agent_event`: real-time WS/desktop event bus (used by the
//!   originating rsclaw session subscriber to render deltas live).
//! - `reply`: per-turn accumulator the actor reads at `Done` time for
//!   the followup-inject summary.

// cap-rs doesn't re-export at the crate root; all protocol types live in cap_rs::core.
use cap_rs::core::{AgentEvent, TextChannel};
use tokio::sync::broadcast;

use super::runtime::NotifTarget;

/// Where bridge output lands. All fields are optional so the same
/// dispatch function serves tool-mode (live IM progress + WS bus +
/// reply collector) and the future P2 conversation mode.
#[allow(dead_code)]
pub struct Sinks<'a> {
    pub notif: Option<&'a NotifTarget>,
    pub agent_event: Option<&'a broadcast::Sender<rsclaw_events::AgentEvent>>,
    pub reply: Option<&'a mut String>,
    pub session_id: &'a str,
    pub agent_id: &'a str,
}

/// Pure mapping: cap-rs AgentEvent → side effects on `sinks`. Returns
/// `true` when the event is a terminal `Done` so the actor task knows
/// to resolve the pending oneshot.
///
/// Notification policy (matches old `tool_acp*` behaviour):
/// - `ToolCallStart` → push "🔧 {name}" to IM (low-volume, useful
///   progress signal).
/// - `TextChunk` Assistant → accumulate into `reply` + relay to
///   `agent_event` bus (NOT pushed to IM directly — IM chunker
///   subscribes to the bus and handles its own batching).
/// - `TextChunk` Thought + `Thought {...}` events → relay to bus on
///   a separate `channel: Thought` track so reasoning is visible to
///   subscribers (desktop UI, IM chunker) without contaminating
///   `reply_buf` (which is the final assistant text saved to history).
///   This makes codex's reasoning streamable to feishu — pre-fix,
///   thoughts were silently logged and dropped.
/// - `Done` → returns `true`. Final summary push is the actor's job
///   (it has the full `reply` text at that point).
#[allow(dead_code)]
pub fn dispatch(event: &AgentEvent, sinks: &mut Sinks<'_>) -> bool {
    match event {
        AgentEvent::TextChunk { text, channel, .. } => {
            // cap-rs TextChannel has Assistant, Thought, System.
            let (delta_channel, write_to_reply) = match channel {
                TextChannel::Assistant => (rsclaw_events::TextChannel::Assistant, true),
                TextChannel::Thought => (rsclaw_events::TextChannel::Thought, false),
                TextChannel::System => (rsclaw_events::TextChannel::System, false),
                // cap_rs::core::TextChannel is `#[non_exhaustive]` —
                // a future variant we don't recognise falls through
                // as System (debug-only, won't pollute reply text).
                _ => (rsclaw_events::TextChannel::System, false),
            };
            if write_to_reply {
                if let Some(buf) = sinks.reply.as_deref_mut() {
                    buf.push_str(text);
                }
            }
            if let Some(bus) = sinks.agent_event {
                let _ = bus.send(rsclaw_events::AgentEvent {
                    session_id: sinks.session_id.to_owned(),
                    agent_id: sinks.agent_id.to_owned(),
                    delta: text.clone(),
                    done: false,
                    files: Vec::new(),
                    images: Vec::new(),
                    tool_log: Vec::new(),
                    question: None,
                    channel: Some(delta_channel),
                });
            }
            false
        }
        AgentEvent::Thought { text, .. } => {
            // Reasoning event distinct from TextChunk-Thought. Same UX
            // intent: surface to subscribers as channel=Thought so the
            // codex thinking phase isn't 30s of dead silence.
            // Always logged at debug for noise control; the realtime
            // path is the bus send below.
            tracing::debug!(
                target: "cap",
                agent = sinks.agent_id,
                thought_len = text.len(),
                "cap thought event"
            );
            if let Some(bus) = sinks.agent_event {
                let _ = bus.send(rsclaw_events::AgentEvent {
                    session_id: sinks.session_id.to_owned(),
                    agent_id: sinks.agent_id.to_owned(),
                    delta: text.clone(),
                    done: false,
                    files: Vec::new(),
                    images: Vec::new(),
                    tool_log: Vec::new(),
                    question: None,
                    channel: Some(rsclaw_events::TextChannel::Thought),
                });
            }
            false
        }
        AgentEvent::ToolCallStart { name, .. } => {
            // Don't push per-tool-call progress to the IM channel. Each cap
            // dispatch can fire dozens of inner tool calls (Bash, Write,
            // read_file, …); pushing one IM line per call drowns the user
            // and hammers rate-limited channels (wechat ret=-2). The cap
            // completion notification (`acp_done_summary`) gives the
            // final state, which is what the user actually wants.
            tracing::debug!(target: "cap", agent = sinks.agent_id, tool = %name, "cap tool start");
            false
        }
        AgentEvent::ToolCallEnd { is_error, .. } => {
            tracing::debug!(target: "cap", agent = sinks.agent_id, is_error, "cap tool end");
            false
        }
        AgentEvent::Done { .. } => true,
        // Non-terminal events not yet projected to sinks in P1.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // cap-rs TextChannel has Assistant/Thought/System — not Final/Default.
    // All protocol types live in cap_rs::core, not the crate root.
    use cap_rs::core::{StopReason, TextChannel};
    use tokio::sync::broadcast;

    fn assistant_chunk(text: &str) -> AgentEvent {
        AgentEvent::TextChunk {
            msg_id: "m1".into(),
            text: text.into(),
            // Adjusted: plan used TextChannel::Final which doesn't exist; real variant is Assistant.
            channel: TextChannel::Assistant,
        }
    }

    #[test]
    fn text_chunk_accumulates_into_reply_and_bus() {
        let mut reply = String::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut sinks = Sinks {
            notif: None,
            agent_event: Some(&tx),
            reply: Some(&mut reply),
            session_id: "sess",
            agent_id: "claudecode",
        };
        let done = dispatch(&assistant_chunk("hello "), &mut sinks);
        assert!(!done);
        let done = dispatch(&assistant_chunk("world"), &mut sinks);
        assert!(!done);
        assert_eq!(reply, "hello world");
        // bus saw two deltas
        assert!(matches!(rx.try_recv(), Ok(ev) if ev.delta == "hello "));
        assert!(matches!(rx.try_recv(), Ok(ev) if ev.delta == "world"));
    }

    #[test]
    fn done_returns_true() {
        let mut sinks = Sinks {
            notif: None,
            agent_event: None,
            reply: None,
            session_id: "sess",
            agent_id: "claudecode",
        };
        let done = dispatch(
            &AgentEvent::Done {
                // Done requires stop_reason (no Default impl on StopReason); plan omitted it.
                stop_reason: StopReason::EndTurn,
                usage: Default::default(),
            },
            &mut sinks,
        );
        assert!(done);
    }

    #[test]
    fn thought_is_swallowed_not_relayed() {
        let mut reply = String::new();
        let mut sinks = Sinks {
            notif: None,
            agent_event: None,
            reply: Some(&mut reply),
            session_id: "sess",
            agent_id: "x",
        };
        let done = dispatch(
            &AgentEvent::Thought {
                msg_id: "t1".into(),
                text: "internal".into(),
            },
            &mut sinks,
        );
        assert!(!done);
        assert!(reply.is_empty());
    }
}
