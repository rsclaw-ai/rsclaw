//! Gateway message router (AGENTS.md §3 "Channel 绑定路由逻辑" + §20 §1).
//!
//! Routing priority (highest → lowest):
//!   1. `bindings[]` rules — explicit peer_id / group_id / path / channel
//!      matches.
//!   2. `agents[].channels` — agent declares which channels it handles.
//!   3. Default agent (`default: true`, or first defined).
//!
//! The router is read-only and thread-safe (all state is in
//! `Arc<RuntimeConfig>`).

use std::sync::Arc;

use anyhow::Result;
use tracing::debug;

use crate::{
    agent::registry::AgentRegistry,
    config::{
        runtime::RuntimeConfig,
        schema::{BindingConfig, BindingMatch},
    },
};

// ---------------------------------------------------------------------------
// Incoming message descriptor
// ---------------------------------------------------------------------------

/// Everything the router needs to know about an inbound message.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Channel name, e.g. "telegram", "discord".
    pub channel: String,
    /// Sender peer ID (user ID on the channel platform).
    pub peer_id: String,
    /// Group / chat ID (empty string for DMs).
    pub group_id: String,
    /// Webhook path (empty for non-hook messages).
    pub path: String,
    /// Account name within the channel (for multi-account channels).
    pub account_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub struct Router {
    config: Arc<RuntimeConfig>,
    registry: Arc<AgentRegistry>,
}

impl Router {
    pub fn new(config: Arc<RuntimeConfig>, registry: Arc<AgentRegistry>) -> Self {
        Self { config, registry }
    }

    /// Resolve the agent ID that should handle `msg`.
    pub fn route(&self, msg: &InboundMessage) -> Result<String> {
        // 1. Check explicit bindings (highest priority, sorted by priority desc).
        if let Some(id) = self.match_bindings(msg) {
            debug!(agent = %id, rule = "binding", "routed");
            return Ok(id);
        }

        // 2. Channel-declared agent (supports "channel:account" format).
        if let Some(id) = self.match_channel_declaration(&msg.channel, msg.account_id.as_deref()) {
            debug!(agent = %id, rule = "channel_decl", "routed");
            return Ok(id);
        }

        // 3. Default agent.
        let id = self.registry.default_agent()?.id.clone();
        debug!(agent = %id, rule = "default", "routed");
        Ok(id)
    }

    // -----------------------------------------------------------------------
    // Bindings matcher
    // -----------------------------------------------------------------------

    fn match_bindings(&self, msg: &InboundMessage) -> Option<String> {
        let mut candidates: Vec<(i32, &BindingConfig)> = self
            .config
            .agents
            .bindings
            .iter()
            .filter(|b| binding_matches(&b.match_, msg))
            .map(|b| (b.priority.unwrap_or(0), b))
            .collect();

        // Sort by priority descending; stable sort preserves config order on ties.
        candidates.sort_by(|a, b| b.0.cmp(&a.0));

        candidates.first().map(|(_, b)| b.agent_id.clone())
    }

    // -----------------------------------------------------------------------
    // Channel declaration matcher
    // -----------------------------------------------------------------------

    /// Match agents by `channels` declaration, supporting "channel:account" format.
    ///
    /// Priority: exact "channel:account" > bare "channel" > no match.
    fn match_channel_declaration(&self, channel: &str, account: Option<&str>) -> Option<String> {
        let qualified = account.map(|a| format!("{channel}:{a}"));

        let mut exact: Vec<&str> = Vec::new();
        let mut bare: Vec<&str> = Vec::new();

        for a in &self.config.agents.list {
            let Some(chs) = a.channels.as_ref() else { continue };
            if let Some(q) = &qualified {
                if chs.iter().any(|c| c == q) {
                    exact.push(&a.id);
                    continue;
                }
            }
            if chs.iter().any(|c| c == channel) {
                bare.push(&a.id);
            }
        }

        let mut matches = if !exact.is_empty() { exact } else { bare };

        match matches.len() {
            0 => None,
            1 => Some(matches[0].to_owned()),
            _ => {
                // Tie-break: lowest alphabetical ID.
                matches.sort_unstable();
                Some(matches[0].to_owned())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Binding predicate
// ---------------------------------------------------------------------------

fn binding_matches(rule: &BindingMatch, msg: &InboundMessage) -> bool {
    // All specified fields must match; unspecified fields are wildcards.
    if rule.channel.as_ref().is_some_and(|ch| ch != &msg.channel) {
        return false;
    }
    if rule.peer_id.as_ref().is_some_and(|pid| pid != &msg.peer_id) {
        return false;
    }
    if rule
        .group_id
        .as_ref()
        .is_some_and(|gid| gid != &msg.group_id)
    {
        return false;
    }
    if rule.path.as_ref().is_some_and(|path| path != &msg.path) {
        return false;
    }
    if rule.account_id.as_ref().is_some_and(|aid| {
        msg.account_id.as_ref().map_or(true, |m| m != aid)
    }) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        runtime::{
            AgentsRuntime, ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime,
            RuntimeConfig,
        },
        schema::{
            AgentEntry, BindMode, BindingConfig, BindingMatch, GatewayMode, ReloadMode,
            SessionConfig,
        },
    };

    fn make_router(agents: Vec<AgentEntry>, bindings: Vec<BindingConfig>) -> Router {
        let cfg = Arc::new(RuntimeConfig {
            gateway: GatewayRuntime {
                port: 18888,
                mode: GatewayMode::Local,
                bind: BindMode::Loopback,
                bind_address: None,
                reload: ReloadMode::Hybrid,
                auth_token: None,
                allow_tailscale: false,
                channel_health_check_minutes: 5,
                channel_stale_event_threshold_minutes: 30,
                channel_max_restarts_per_hour: 10,
                auth_token_configured: false,
                auth_token_is_plaintext: false,
                user_agent: None,
                language: None,
            },
            agents: AgentsRuntime {
                defaults: Default::default(),
                list: agents.clone(),
                bindings,
                external: vec![],
            },
            channel: ChannelRuntime {
                channels: Default::default(),
                session: SessionConfig {
                    dm_scope: None,
                    thread_bindings: None,
                    reset: None,
                    identity_links: None,
                    maintenance: None,
                },
            },
            model: ModelRuntime {
                models: None,
                auth: None,
            },
            ext: ExtRuntime {
                tools: None,
                skills: None,
                plugins: None,
            },
            ops: OpsRuntime {
                cron: None,
                hooks: None,
                sandbox: None,
                logging: None,
                secrets: None,
            },
            raw: Default::default(),
        });
        let registry = Arc::new(AgentRegistry::from_config(&cfg));
        Router::new(cfg, registry)
    }

    fn agent(id: &str, default: bool, channels: Option<Vec<&str>>) -> AgentEntry {
        AgentEntry {
            id: id.to_owned(),
            default: if default { Some(true) } else { None },
            workspace: None,
            model: None,
            flash_model: None,
            lane: None,
            lane_concurrency: None,
            group_chat: None,
            channels: channels.map(|v| v.into_iter().map(str::to_owned).collect()),
            commands: None,
            allowed_commands: None,
            name: None,
            opencode: None,
            claudecode: None,
            codex: None,
            agent_dir: None,
            system: None,
            temperature: None,
        }
    }

    fn msg(channel: &str, peer_id: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            group_id: String::new(),
            path: String::new(),
            account_id: None,
        }
    }

    #[test]
    fn falls_back_to_default() {
        let router = make_router(vec![agent("main", true, None)], vec![]);
        assert_eq!(router.route(&msg("telegram", "u1")).unwrap(), "main");
    }

    #[test]
    fn routes_by_channel_declaration() {
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("tgbot", false, Some(vec!["telegram"])),
            ],
            vec![],
        );
        assert_eq!(router.route(&msg("telegram", "u1")).unwrap(), "tgbot");
        assert_eq!(router.route(&msg("discord", "u1")).unwrap(), "main");
    }

    #[test]
    fn binding_by_peer_id_overrides_channel_decl() {
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("tgbot", false, Some(vec!["telegram"])),
                agent("vip_handler", false, None),
            ],
            vec![BindingConfig {
                kind: None,
                agent_id: "vip_handler".to_owned(),
                match_: BindingMatch {
                    channel: Some("telegram".to_owned()),
                    peer_id: Some("vip_user_123".to_owned()),
                    group_id: None,
                    path: None,
                    account_id: None,
                },
                priority: Some(10),
            }],
        );
        // VIP user → binding wins over channel declaration.
        assert_eq!(
            router
                .route(&InboundMessage {
                    channel: "telegram".to_owned(),
                    peer_id: "vip_user_123".to_owned(),
                    group_id: String::new(),
                    path: String::new(),
                    account_id: None,
                })
                .unwrap(),
            "vip_handler"
        );
        // Regular telegram user → tgbot.
        assert_eq!(
            router.route(&msg("telegram", "regular_user")).unwrap(),
            "tgbot"
        );
    }

    #[test]
    fn higher_priority_binding_wins() {
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("agent_a", false, None),
                agent("agent_b", false, None),
            ],
            vec![
                BindingConfig {
                    kind: None,
                    agent_id: "agent_a".to_owned(),
                    match_: BindingMatch {
                        channel: Some("slack".to_owned()),
                        peer_id: None,
                        group_id: None,
                        path: None,
                        account_id: None,
                    },
                    priority: Some(5),
                },
                BindingConfig {
                    kind: None,
                    agent_id: "agent_b".to_owned(),
                    match_: BindingMatch {
                        channel: Some("slack".to_owned()),
                        peer_id: None,
                        group_id: None,
                        path: None,
                        account_id: None,
                    },
                    priority: Some(10),
                },
            ],
        );
        assert_eq!(router.route(&msg("slack", "u1")).unwrap(), "agent_b");
    }

    #[test]
    fn wildcard_binding_matches_any_peer() {
        let router = make_router(
            vec![agent("main", true, None), agent("slack_agent", false, None)],
            vec![BindingConfig {
                kind: None,
                agent_id: "slack_agent".to_owned(),
                match_: BindingMatch {
                    channel: Some("slack".to_owned()),
                    peer_id: None,
                    group_id: None,
                    path: None,
                    account_id: None,
                },
                priority: None,
            }],
        );
        assert_eq!(
            router.route(&msg("slack", "anyone")).unwrap(),
            "slack_agent"
        );
    }

    // -- Multi-account routing tests --

    fn msg_with_account(channel: &str, peer_id: &str, account: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_owned(),
            peer_id: peer_id.to_owned(),
            group_id: String::new(),
            path: String::new(),
            account_id: Some(account.to_owned()),
        }
    }

    #[test]
    fn routes_by_channel_account_declaration() {
        // Two agents bound to different feishu accounts.
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("sales", false, Some(vec!["feishu:sales-bot"])),
                agent("support", false, Some(vec!["feishu:support-bot"])),
            ],
            vec![],
        );
        // Exact account match.
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "sales-bot")).unwrap(),
            "sales"
        );
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "support-bot")).unwrap(),
            "support"
        );
        // Unknown account falls back to default (no bare "feishu" declaration).
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "unknown")).unwrap(),
            "main"
        );
    }

    #[test]
    fn bare_channel_decl_catches_all_accounts() {
        // Agent with bare "feishu" catches any account.
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("fs_all", false, Some(vec!["feishu"])),
            ],
            vec![],
        );
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "any-bot")).unwrap(),
            "fs_all"
        );
        // Also matches without account_id.
        assert_eq!(
            router.route(&msg("feishu", "u1")).unwrap(),
            "fs_all"
        );
    }

    #[test]
    fn exact_account_overrides_bare_channel() {
        // Exact "feishu:vip-bot" should win over bare "feishu".
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("general", false, Some(vec!["feishu"])),
                agent("vip", false, Some(vec!["feishu:vip-bot"])),
            ],
            vec![],
        );
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "vip-bot")).unwrap(),
            "vip"
        );
        // Other accounts fall through to bare match.
        assert_eq!(
            router.route(&msg_with_account("feishu", "u1", "other")).unwrap(),
            "general"
        );
    }

    #[test]
    fn binding_with_account_id_matches() {
        let router = make_router(
            vec![
                agent("main", true, None),
                agent("dt_agent", false, None),
            ],
            vec![BindingConfig {
                kind: None,
                agent_id: "dt_agent".to_owned(),
                match_: BindingMatch {
                    channel: Some("dingtalk".to_owned()),
                    peer_id: None,
                    group_id: None,
                    path: None,
                    account_id: Some("corp-bot".to_owned()),
                },
                priority: None,
            }],
        );
        // Matches when account_id matches.
        assert_eq!(
            router.route(&msg_with_account("dingtalk", "u1", "corp-bot")).unwrap(),
            "dt_agent"
        );
        // Does NOT match a different account.
        assert_eq!(
            router.route(&msg_with_account("dingtalk", "u1", "other")).unwrap(),
            "main"
        );
    }
}
