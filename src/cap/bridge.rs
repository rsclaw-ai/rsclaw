//! `cap_rs::AgentEvent` → rsclaw sinks dispatch.

// cap-rs doesn't re-export at the crate root; all protocol types live in cap_rs::core.
use cap_rs::core::{AgentEvent, TextChannel};
use tokio::sync::broadcast;

/// Where bridge output lands. All fields are optional so the same
/// dispatch function serves both tool-mode (reply collector + bus)
/// and the future P2 conversation mode (live user-channel sink).
#[allow(dead_code)]
pub(crate) struct Sinks<'a> {
    pub agent_event: Option<&'a broadcast::Sender<crate::events::AgentEvent>>,
    pub reply: Option<&'a mut String>,
    pub session_id: &'a str,
    pub agent_id: &'a str,
}

/// Pure mapping: cap-rs AgentEvent → side effects on `sinks`. Returns
/// `true` when the event is a terminal `Done` so the actor task knows
/// to resolve the pending oneshot.
#[allow(dead_code)]
pub(crate) fn dispatch(event: &AgentEvent, sinks: &mut Sinks<'_>) -> bool {
    match event {
        AgentEvent::TextChunk { text, channel, .. } => {
            // cap-rs TextChannel has: Assistant, Thought, System.
            // Plan assumed Final|Default — adjusted to relay Assistant channel
            // (the normal output channel) and skip Thought/System here.
            if matches!(channel, TextChannel::Assistant) {
                if let Some(buf) = sinks.reply.as_deref_mut() {
                    buf.push_str(text);
                }
                if let Some(bus) = sinks.agent_event {
                    let _ = bus.send(crate::events::AgentEvent {
                        session_id: sinks.session_id.to_owned(),
                        agent_id: sinks.agent_id.to_owned(),
                        delta: text.clone(),
                        done: false,
                        files: Vec::new(),
                        images: Vec::new(),
                        tool_log: Vec::new(),
                        question: None,
                    });
                }
            }
            false
        }
        AgentEvent::Thought { text, .. } => {
            tracing::info!(target: "cap", agent = sinks.agent_id, thought = %text, "cap thought");
            false
        }
        AgentEvent::ToolCallStart { name, .. } => {
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
