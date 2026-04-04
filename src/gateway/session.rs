//! Session key generation and dmScope isolation (AGENTS.md §17).
//!
//! Session keys are deterministic strings that control context isolation.
//! The key format is determined by `session.dmScope` in the config.
//!
//! Key formats:
//!
//! | dmScope                   | DM key                                          |
//! |---------------------------|-------------------------------------------------|
//! | main                      | `agent:<agentId>:main`                          |
//! | per-peer                  | `agent:<agentId>:direct:<peerId>`               |
//! | per-channel-peer (default)| `agent:<agentId>:<channel>:direct:<peerId>`     |
//! | per-account-channel-peer  | `agent:<agentId>:<channel>:<accountId>:direct:<peerId>` |
//!
//! Group message keys:
//!   `agent:<agentId>:<channel>:group:<groupId>`
//!
//! Telegram topic:
//!   `agent:<agentId>:telegram:group:<groupId>:topic:<threadId>`
//!
//! Cron job:
//!   `cron:<jobId>`
//!
//! Webhook:
//!   `hook:<uuid>` (or `hooks.defaultSessionKey` when set)

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::schema::DmScope;

// ---------------------------------------------------------------------------
// MessageKind
// ---------------------------------------------------------------------------

/// Describes the kind of inbound message for session-key generation.
#[derive(Debug, Clone)]
pub enum MessageKind {
    /// Direct message to the agent.
    DirectMessage { account_id: Option<String> },
    /// Group / channel message.
    GroupMessage {
        group_id: String,
        thread_id: Option<String>, // Telegram topics
    },
    /// Webhook-triggered message.
    Webhook { custom_key: Option<String> },
    /// Cron job.
    Cron {
        job_id: String,
        mode: CronSessionMode,
    },
}

#[derive(Debug, Clone)]
pub enum CronSessionMode {
    /// Fresh isolated session each run.
    Isolated,
    /// Reuse a named persistent session.
    Persistent(String),
}

// ---------------------------------------------------------------------------
// SessionKeyParams
// ---------------------------------------------------------------------------

/// All parameters needed to derive a session key.
#[derive(Debug, Clone)]
pub struct SessionKeyParams {
    pub agent_id: String,
    pub channel: String,
    pub peer_id: String,
    pub kind: MessageKind,
    pub dm_scope: DmScope,
}

// ---------------------------------------------------------------------------
// Session key derivation
// ---------------------------------------------------------------------------

/// Compute the session key for a given message.
pub fn derive_session_key(params: &SessionKeyParams) -> String {
    match &params.kind {
        MessageKind::DirectMessage { account_id } => derive_dm_key(params, account_id.as_deref()),
        MessageKind::GroupMessage {
            group_id,
            thread_id,
        } => derive_group_key(
            &params.agent_id,
            &params.channel,
            group_id,
            thread_id.as_deref(),
        ),
        MessageKind::Webhook { custom_key } => custom_key
            .clone()
            .unwrap_or_else(|| format!("hook:{}", Uuid::new_v4())),
        MessageKind::Cron { job_id, mode } => match mode {
            CronSessionMode::Isolated => format!("cron:{job_id}"),
            CronSessionMode::Persistent(key) => format!("session:{key}"),
        },
    }
}

fn derive_dm_key(params: &SessionKeyParams, account_id: Option<&str>) -> String {
    let a = &params.agent_id;
    let c = &params.channel;
    let p = &params.peer_id;

    match params.dm_scope {
        DmScope::Main => {
            format!("agent:{a}:main")
        }
        DmScope::PerPeer => {
            format!("agent:{a}:direct:{p}")
        }
        DmScope::PerChannelPeer => {
            format!("agent:{a}:{c}:direct:{p}")
        }
        DmScope::PerAccountChannelPeer => {
            let acc = account_id.unwrap_or("default");
            format!("agent:{a}:{c}:{acc}:direct:{p}")
        }
    }
}

fn derive_group_key(
    agent_id: &str,
    channel: &str,
    group_id: &str,
    thread_id: Option<&str>,
) -> String {
    match thread_id {
        Some(tid) => {
            format!("agent:{agent_id}:{channel}:group:{group_id}:topic:{tid}")
        }
        None => {
            format!("agent:{agent_id}:{channel}:group:{group_id}")
        }
    }
}

// ---------------------------------------------------------------------------
// Identity links resolution (AGENTS.md §17)
// ---------------------------------------------------------------------------

/// Resolve `session.identityLinks` to merge cross-channel identities.
///
/// If a peer on `channel` is listed under any identity in `identity_links`,
/// return the canonical identity key for that person.
///
/// `identity_links` format:
/// ```json5
/// { "alice": ["telegram:123", "discord:456"] }
/// ```
pub fn resolve_identity(
    channel: &str,
    peer_id: &str,
    identity_links: &HashMap<String, Vec<String>>,
) -> Option<String> {
    let needle = format!("{channel}:{peer_id}");
    for (identity, peers) in identity_links {
        if peers.iter().any(|p| p == &needle) {
            return Some(identity.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn params(scope: DmScope, channel: &str, peer: &str) -> SessionKeyParams {
        SessionKeyParams {
            agent_id: "main".to_owned(),
            channel: channel.to_owned(),
            peer_id: peer.to_owned(),
            kind: MessageKind::DirectMessage { account_id: None },
            dm_scope: scope,
        }
    }

    #[test]
    fn dm_scope_main() {
        let key = derive_session_key(&params(DmScope::Main, "telegram", "u1"));
        assert_eq!(key, "agent:main:main");
    }

    #[test]
    fn dm_scope_per_peer() {
        let key = derive_session_key(&params(DmScope::PerPeer, "telegram", "u1"));
        assert_eq!(key, "agent:main:direct:u1");
    }

    #[test]
    fn dm_scope_per_channel_peer() {
        let key = derive_session_key(&params(DmScope::PerChannelPeer, "telegram", "u1"));
        assert_eq!(key, "agent:main:telegram:direct:u1");
    }

    #[test]
    fn dm_scope_per_account_channel_peer() {
        let mut p = params(DmScope::PerAccountChannelPeer, "telegram", "u1");
        p.kind = MessageKind::DirectMessage {
            account_id: Some("acc42".to_owned()),
        };
        let key = derive_session_key(&p);
        assert_eq!(key, "agent:main:telegram:acc42:direct:u1");
    }

    #[test]
    fn group_message_key() {
        let p = SessionKeyParams {
            agent_id: "main".to_owned(),
            channel: "telegram".to_owned(),
            peer_id: "u1".to_owned(),
            kind: MessageKind::GroupMessage {
                group_id: "g100".to_owned(),
                thread_id: None,
            },
            dm_scope: DmScope::PerChannelPeer,
        };
        assert_eq!(derive_session_key(&p), "agent:main:telegram:group:g100");
    }

    #[test]
    fn telegram_topic_key() {
        let p = SessionKeyParams {
            agent_id: "main".to_owned(),
            channel: "telegram".to_owned(),
            peer_id: String::new(),
            kind: MessageKind::GroupMessage {
                group_id: "g100".to_owned(),
                thread_id: Some("t42".to_owned()),
            },
            dm_scope: DmScope::PerChannelPeer,
        };
        assert_eq!(
            derive_session_key(&p),
            "agent:main:telegram:group:g100:topic:t42"
        );
    }

    #[test]
    fn cron_isolated_key() {
        let p = SessionKeyParams {
            agent_id: "main".to_owned(),
            channel: String::new(),
            peer_id: String::new(),
            kind: MessageKind::Cron {
                job_id: "morning-briefing".to_owned(),
                mode: CronSessionMode::Isolated,
            },
            dm_scope: DmScope::Main,
        };
        assert_eq!(derive_session_key(&p), "cron:morning-briefing");
    }

    #[test]
    fn identity_link_resolution() {
        let mut links = HashMap::new();
        links.insert(
            "alice".to_owned(),
            vec!["telegram:12345".to_owned(), "discord:67890".to_owned()],
        );
        assert_eq!(
            resolve_identity("telegram", "12345", &links),
            Some("alice".to_owned())
        );
        assert_eq!(
            resolve_identity("discord", "67890", &links),
            Some("alice".to_owned())
        );
        assert_eq!(resolve_identity("slack", "99999", &links), None);
    }
}
