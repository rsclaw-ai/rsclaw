//! Sender trust levels.
//!
//! Channel membership (DM allowlist, pairing, group allowlist) decides who may
//! TALK to an agent. Trust decides what a sender may make the agent DO. Only
//! owners may reach high-risk capabilities (shell, file writes, cron,
//! cross-session access, local slash commands such as `/sh` and `/cat`).
//!
//! Owners are:
//! - local entry points (desktop UI, WebSocket operator, CLI, loopback HTTP,
//!   cron/heartbeat jobs created by owners);
//! - DM senders listed explicitly (not via `*`) in a channel's static
//!   `allowFrom` (top level or any account);
//! - identities listed in `gateway.owners` as `"<channel>:<peer_id>"`
//!   (these count in groups too).
//!
//! Everyone else (paired users, group members, A2A peers, webhook callers) is
//! a [`SenderTrust::User`].

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, RwLock};

/// Trust level of the sender driving a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SenderTrust {
    /// Full access to every capability the agent is configured with.
    Owner,
    /// Restricted: high-risk tools and local commands are refused.
    #[default]
    User,
}

impl SenderTrust {
    /// True for [`SenderTrust::Owner`].
    pub fn is_owner(self) -> bool {
        matches!(self, SenderTrust::Owner)
    }
}

/// Tools that non-owner senders may not call unless the agent lists them in
/// `nonOwnerTools`. Aliases are listed explicitly because dispatch matches
/// on raw names.
pub const OWNER_ONLY_TOOLS: &[&str] = &[
    // Host command execution / coding agents
    "shell",
    "execute_command",
    "exec",
    "anycli",
    "opencli",
    "cap",
    "cap_live",
    "cap_live_end",
    "cap_bind_sticky",
    "cap_unbind_sticky",
    "install_tool",
    "tool_install",
    // Host file mutation / exfiltration
    "write_file",
    "write",
    "edit_file",
    "edit",
    "web_download",
    "send_file",
    // Desktop / logged-in browser
    "computer_use",
    "web_browser",
    "browser",
    // Scheduling and cross-session / cross-agent control
    "cron",
    "session",
    "sessions_send",
    "sessions_list",
    "sessions_history",
    "agent",
    "subagents",
    "agent_create",
    "send_message",
    "message",
    // Gateway / channel administration
    "gateway",
    "pairing",
    "channel",
    "telegram_actions",
    "discord_actions",
    "slack_actions",
    "whatsapp_actions",
    "feishu_actions",
    "weixin_actions",
    "qq_actions",
    "dingtalk_actions",
    // Skill management
    "skill_install",
    "skill_remove",
];

/// True when `tool` is restricted to owners by default.
pub fn is_owner_only_tool(tool: &str) -> bool {
    OWNER_ONLY_TOOLS.contains(&tool)
}

/// Decide whether `trust` may call `tool` on an agent whose
/// `nonOwnerTools` config is `non_owner_tools`.
pub fn tool_allowed(trust: SenderTrust, tool: &str, non_owner_tools: Option<&[String]>) -> bool {
    if trust.is_owner() || !is_owner_only_tool(tool) {
        return true;
    }
    non_owner_tools.is_some_and(|list| list.iter().any(|t| t == "*" || t == tool))
}

static EXPLICIT_OWNERS: LazyLock<RwLock<HashSet<String>>> =
    LazyLock::new(|| RwLock::new(HashSet::new()));

/// Replace the `gateway.owners` set. Call at startup and on config reload.
pub fn set_explicit_owners<I, S>(owners: I)
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let set: HashSet<String> = owners
        .into_iter()
        .map(Into::into)
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    match EXPLICIT_OWNERS.write() {
        Ok(mut g) => *g = set,
        Err(poisoned) => *poisoned.into_inner() = set,
    }
}

/// True when `"<channel>:<peer_id>"` is listed in `gateway.owners`.
/// `channel` may carry an account suffix (`feishu/app2`); only the base
/// channel name is compared.
pub fn is_explicit_owner(channel: &str, peer_id: &str) -> bool {
    if peer_id.is_empty() {
        return false;
    }
    let base = channel.split('/').next().unwrap_or(channel);
    let key = format!("{base}:{peer_id}");
    match EXPLICIT_OWNERS.read() {
        Ok(g) => g.contains(&key),
        Err(poisoned) => poisoned.into_inner().contains(&key),
    }
}

static STATIC_ALLOWLISTS: LazyLock<RwLock<HashMap<String, HashSet<String>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Replace the per-channel static `allowFrom` sets (base channel name ->
/// literal peer ids, `*` excluded). Call at startup and on config reload.
pub fn set_static_allowlists(map: HashMap<String, HashSet<String>>) {
    let cleaned: HashMap<String, HashSet<String>> = map
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().filter(|p| p != "*" && !p.is_empty()).collect()))
        .collect();
    match STATIC_ALLOWLISTS.write() {
        Ok(mut g) => *g = cleaned,
        Err(poisoned) => *poisoned.into_inner() = cleaned,
    }
}

/// True when `peer_id` is literally listed in `channel`'s static `allowFrom`.
pub fn is_statically_allowed(channel: &str, peer_id: &str) -> bool {
    if peer_id.is_empty() {
        return false;
    }
    let base = channel.split('/').next().unwrap_or(channel);
    let check = |g: &HashMap<String, HashSet<String>>| {
        g.get(base).is_some_and(|set| set.contains(peer_id))
    };
    match STATIC_ALLOWLISTS.read() {
        Ok(g) => check(&g),
        Err(poisoned) => check(&poisoned.into_inner()),
    }
}

/// Channel names used by local, already-authenticated entry points. Turns
/// on these channels run as owner.
pub const LOCAL_CHANNELS: &[&str] = &[
    "ws", "desktop", "cli", "cron", "heartbeat", "api", "system", "oai", "acp",
];

/// True for [`LOCAL_CHANNELS`].
pub fn is_local_channel(channel: &str) -> bool {
    LOCAL_CHANNELS.contains(&channel)
}

/// Trust for a message arriving on a messaging channel, using the static
/// allowlists registered via [`set_static_allowlists`] and the explicit
/// owners registered via [`set_explicit_owners`].
pub fn channel_trust(channel: &str, peer_id: &str, is_group: bool) -> SenderTrust {
    if is_explicit_owner(channel, peer_id)
        || (!is_group && is_statically_allowed(channel, peer_id))
    {
        SenderTrust::Owner
    } else {
        SenderTrust::User
    }
}

/// Trust for a turn re-hydrated from the persistent task queue, where only
/// the channel / sender / group flag survived. Local channels are owners;
/// internal agent-to-agent channels (`task:`, `send:`, `a2a:` ...) and
/// webhooks are users; messaging channels are resolved via
/// [`channel_trust`]. A `:follow_up` sender suffix is ignored.
pub fn queued_trust(channel: &str, sender: &str, is_group: bool) -> SenderTrust {
    if is_local_channel(channel) {
        return SenderTrust::Owner;
    }
    if channel.contains(':') {
        return SenderTrust::User;
    }
    let sender = sender.strip_suffix(":follow_up").unwrap_or(sender);
    channel_trust(channel, sender, is_group)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_resolution_and_tool_gate() {
        set_explicit_owners(["telegram:42"]);
        let mut lists = HashMap::new();
        lists.insert(
            "telegram".to_owned(),
            ["7".to_owned(), "*".to_owned()].into_iter().collect(),
        );
        set_static_allowlists(lists);
        assert_eq!(channel_trust("telegram", "42", true), SenderTrust::Owner);
        assert_eq!(channel_trust("telegram/bot2", "42", false), SenderTrust::Owner);
        assert_eq!(channel_trust("telegram", "7", false), SenderTrust::Owner);
        assert_eq!(channel_trust("telegram", "7", true), SenderTrust::User);
        assert_eq!(channel_trust("telegram", "8", false), SenderTrust::User);
        assert_eq!(channel_trust("feishu", "42", false), SenderTrust::User);
        assert_eq!(queued_trust("telegram", "7:follow_up", false), SenderTrust::Owner);
        assert_eq!(queued_trust("ws", "anyone", false), SenderTrust::Owner);
        assert_eq!(queued_trust("a2a:main", "7", false), SenderTrust::User);
        set_explicit_owners(Vec::<String>::new());
        set_static_allowlists(HashMap::new());

        assert!(tool_allowed(SenderTrust::Owner, "shell", None));
        assert!(!tool_allowed(SenderTrust::User, "shell", None));
        assert!(tool_allowed(SenderTrust::User, "web_search", None));
        let allow = vec!["shell".to_owned()];
        assert!(tool_allowed(SenderTrust::User, "shell", Some(&allow)));
        assert!(!tool_allowed(SenderTrust::User, "cron", Some(&allow)));
        let all = vec!["*".to_owned()];
        assert!(tool_allowed(SenderTrust::User, "cron", Some(&all)));
    }
}
