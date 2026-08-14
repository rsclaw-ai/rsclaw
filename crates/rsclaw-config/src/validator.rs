//! Cross-field constraint validation for RuntimeConfig.
//! Runs after loading + schema deserialization.

use anyhow::{Result, bail};
use tracing::{debug, warn};

use super::{runtime::RuntimeConfig, schema::DmScope};

fn is_blank(value: Option<&str>) -> bool {
    value.is_none_or(|value| value.trim().is_empty())
}

fn is_valid_node_id(value: &str) -> bool {
    !value.trim().is_empty() && !value.contains(['\r', '\n'])
}

fn is_valid_hub_url(value: &str) -> bool {
    !value.chars().any(char::is_whitespace)
        && url::Url::parse(value)
            .is_ok_and(|url| matches!(url.scheme(), "ws" | "wss") && url.host_str().is_some())
}

fn is_valid_ice_url(value: &str, allowed_schemes: &[&str]) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    let Some((_, endpoint)) = value.split_once(':') else {
        return false;
    };
    if !allowed_schemes.contains(&url.scheme()) || endpoint.is_empty() || endpoint.starts_with("//")
    {
        return false;
    }
    let Ok(endpoint_url) = url::Url::parse(&format!("http://{endpoint}")) else {
        return false;
    };
    endpoint_url.host_str().is_some()
        && endpoint_url.path() == "/"
        && endpoint_url.username().is_empty()
        && endpoint_url.password().is_none()
        && endpoint_url.fragment().is_none()
}

/// Validate the fully-loaded RuntimeConfig.
/// Returns `Err` for hard errors (will prevent startup).
/// Emits `warn!` for soft issues that are still allowed.
pub fn validate(cfg: &RuntimeConfig) -> Result<()> {
    validate_gateway(cfg)?;
    validate_agents(cfg)?;
    validate_session(cfg)?;
    validate_hooks(cfg)?;
    Ok(())
}

fn validate_gateway(cfg: &RuntimeConfig) -> Result<()> {
    if !cfg.gateway.auth_token_configured {
        warn!(
            "gateway.auth.token is not set — the gateway accepts all connections without authentication. \
             Set gateway.auth.token in your config to require a bearer token."
        );
    }
    if cfg.gateway.port < 1024 && cfg.gateway.port != 80 && cfg.gateway.port != 443 {
        warn!(
            port = cfg.gateway.port,
            "gateway port < 1024 may require elevated privileges"
        );
    }
    if cfg
        .gateway
        .a2a_relay
        .nodes
        .iter()
        .any(|node| !is_valid_node_id(&node.node_id))
        || cfg.agents.a2a.iter().any(|peer| {
            peer.node_id
                .as_deref()
                .is_some_and(|node_id| !is_valid_node_id(node_id))
        })
    {
        bail!("gateway.a2a.relay requires single-line configured peer node IDs");
    }
    if let Some(peer) = cfg
        .gateway
        .a2a_relay
        .peer
        .as_ref()
        .filter(|peer| peer.enabled)
    {
        if cfg.gateway.a2a_relay.mode != super::runtime::A2aRelayModeRuntime::Spoke {
            bail!("gateway.a2a.relay.peer.enabled requires relay mode \"spoke\"");
        }
        if cfg
            .gateway
            .a2a_relay
            .node_id
            .as_deref()
            .is_none_or(|node_id| !is_valid_node_id(node_id))
        {
            bail!("gateway.a2a.relay.peer.enabled requires a non-empty single-line nodeId");
        }
        if cfg.gateway.a2a_relay.hub_urls.is_empty() {
            bail!("gateway.a2a.relay.peer.enabled requires hubUrl or relays");
        }
        if cfg
            .gateway
            .a2a_relay
            .hub_urls
            .iter()
            .any(|url| !is_valid_hub_url(url))
        {
            bail!(
                "gateway.a2a.relay.peer.enabled requires valid ws:// or wss:// hubUrl and relays entries"
            );
        }
        if is_blank(cfg.gateway.a2a_relay.token.as_deref())
            && is_blank(cfg.gateway.a2a_relay.private_key.as_deref())
        {
            bail!("gateway.a2a.relay.peer.enabled requires relay token or privateKey");
        }
        if peer
            .stun_urls
            .iter()
            .any(|url| !is_valid_ice_url(url, &["stun", "stuns"]))
        {
            bail!("gateway.a2a.relay.peer.stunUrls entries must use valid stun: or stuns: URLs");
        }
        if peer
            .turn_urls
            .iter()
            .any(|url| !is_valid_ice_url(url, &["turn", "turns"]))
        {
            bail!("gateway.a2a.relay.peer.turnUrls entries must use valid turn: or turns: URLs");
        }
        if !peer.turn_urls.is_empty()
            && (is_blank(peer.turn_username.as_deref())
                || is_blank(peer.turn_credential.as_deref()))
        {
            bail!("gateway.a2a.relay.peer.turnUrls requires turnUsername and turnCredential");
        }
    }
    Ok(())
}

fn validate_agents(cfg: &RuntimeConfig) -> Result<()> {
    if cfg.agents.list.is_empty() {
        debug!("agents.list empty; default agent will be auto-synthesized");
    }
    let defaults: Vec<_> = cfg
        .agents
        .list
        .iter()
        .filter(|a| a.default == Some(true))
        .collect();
    if defaults.len() > 1 {
        bail!(
            "multiple agents marked as default: {}. Only one agent may have `default: true`.",
            defaults
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let mut seen = std::collections::HashSet::new();
    for agent in &cfg.agents.list {
        if !seen.insert(agent.id.clone()) {
            bail!("duplicate agent id: \"{}\"", agent.id);
        }
    }
    for ext in &cfg.agents.a2a {
        if !seen.insert(ext.id.clone()) {
            bail!(
                "duplicate agent id (external conflicts with local): \"{}\"",
                ext.id
            );
        }
    }
    Ok(())
}

fn validate_session(cfg: &RuntimeConfig) -> Result<()> {
    if let Some(DmScope::Main) = cfg.channel.session.dm_scope {
        warn!(
            "session.dmScope = \"main\" means all DMs share one context. \
             Consider \"per-channel-peer\" for multi-user setups."
        );
    }
    Ok(())
}

fn validate_hooks(cfg: &RuntimeConfig) -> Result<()> {
    if let Some(hooks) = &cfg.ops.hooks
        && hooks.enabled
        && hooks.token.is_none()
    {
        warn!(
            "hooks.enabled = true but no hooks.token is set. \
             Any caller can trigger webhooks."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        runtime::IntoRuntime,
        schema::{
            A2aPeerRelayConfig, A2aRelayConfig, A2aRelayMode, Config, GatewayA2a, GatewayConfig,
            SecretOrString,
        },
    };

    fn peer_config(peer: A2aPeerRelayConfig, mode: A2aRelayMode) -> RuntimeConfig {
        Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    relay: Some(A2aRelayConfig {
                        mode: Some(mode),
                        node_id: Some("node-a".to_owned()),
                        hub_url: Some("wss://hub.example.test/api/v1/a2a/relay/ws".to_owned()),
                        token: Some(SecretOrString::Plain("relay-token".to_owned())),
                        peer: Some(peer),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
        .into_runtime()
        .expect("test config should resolve")
    }

    fn enabled_peer() -> A2aPeerRelayConfig {
        A2aPeerRelayConfig {
            enabled: true,
            stun_urls: None,
            turn_urls: None,
            turn_username: None,
            turn_credential: None,
            listen_port: None,
        }
    }

    #[test]
    fn peer_transport_requires_spoke_mode() {
        let cfg = peer_config(enabled_peer(), A2aRelayMode::Hub);
        let error = validate(&cfg).expect_err("hub mode must reject peer transport");
        assert!(error.to_string().contains("requires relay mode \"spoke\""));
    }

    #[test]
    fn peer_transport_requires_relay_authentication() {
        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.token = None;
        let error = validate(&cfg).expect_err("peer relay without authentication must fail");
        assert!(
            error
                .to_string()
                .contains("requires relay token or privateKey")
        );
    }

    #[test]
    fn peer_transport_rejects_whitespace_only_prerequisites() {
        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.node_id = Some(" \t".to_owned());
        let error = validate(&cfg).expect_err("blank node ID must fail");
        assert!(error.to_string().contains("non-empty single-line nodeId"));

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.hub_urls = vec!["  ".to_owned()];
        let error = validate(&cfg).expect_err("blank relay URL must fail");
        assert!(
            error
                .to_string()
                .contains("requires valid ws:// or wss:// hubUrl and relays entries")
        );

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.token = Some("  ".to_owned());
        cfg.gateway.a2a_relay.private_key = Some("\n".to_owned());
        let error = validate(&cfg).expect_err("blank relay authentication must fail");
        assert!(
            error
                .to_string()
                .contains("requires relay token or privateKey")
        );

        let mut peer = enabled_peer();
        peer.turn_urls = Some(vec!["turn:turn.example.test:3478".to_owned()]);
        peer.turn_username = Some(" \t".to_owned());
        peer.turn_credential = Some(SecretOrString::Plain("\n".to_owned()));
        let cfg = peer_config(peer, A2aRelayMode::Spoke);
        let error = validate(&cfg).expect_err("blank TURN credentials must fail");
        assert!(
            error
                .to_string()
                .contains("requires turnUsername and turnCredential")
        );
    }

    #[test]
    fn peer_transport_rejects_multiline_signaling_node_ids() {
        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.node_id = Some("node-a\nnode-b".to_owned());
        let error = validate(&cfg).expect_err("multiline local node ID must fail");
        assert!(error.to_string().contains("single-line nodeId"));

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway
            .a2a_relay
            .nodes
            .push(crate::runtime::A2aRelayNodeRuntime {
                node_id: "node-b\rnode-c".to_owned(),
                token: "token".to_owned(),
                ..Default::default()
            });
        let error = validate(&cfg).expect_err("multiline hub node ID must fail");
        assert!(
            error
                .to_string()
                .contains("single-line configured peer node IDs")
        );

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.agents.a2a.push(crate::schema::A2aPeerConfig {
            id: "peer-b".to_owned(),
            url: "https://peer-b.example.test".to_owned(),
            auth_token: None,
            remote_agent_id: None,
            description: None,
            mode: Some("peer".to_owned()),
            node_id: Some("node-b\nnode-c".to_owned()),
            public_key: None,
            scopes: None,
        });
        let error = validate(&cfg).expect_err("multiline configured peer node ID must fail");
        assert!(
            error
                .to_string()
                .contains("single-line configured peer node IDs")
        );
    }

    #[test]
    fn peer_transport_rejects_blank_endpoint_entries() {
        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.hub_urls.push(" \t".to_owned());
        let error = validate(&cfg).expect_err("mixed blank relay URL must fail");
        assert!(
            error
                .to_string()
                .contains("requires valid ws:// or wss:// hubUrl and relays entries")
        );

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway
            .a2a_relay
            .peer
            .as_mut()
            .expect("peer config")
            .stun_urls = vec!["stun:stun.example.test:3478".to_owned(), " ".to_owned()];
        let error = validate(&cfg).expect_err("blank STUN URL must fail");
        assert!(
            error
                .to_string()
                .contains("stunUrls entries must use valid stun: or stuns: URLs")
        );

        let mut peer = enabled_peer();
        peer.turn_urls = Some(vec![
            "turn:turn.example.test:3478".to_owned(),
            "\n".to_owned(),
        ]);
        peer.turn_username = Some("node-a".to_owned());
        peer.turn_credential = Some(SecretOrString::Plain("credential".to_owned()));
        let cfg = peer_config(peer, A2aRelayMode::Spoke);
        let error = validate(&cfg).expect_err("blank TURN URL must fail");
        assert!(
            error
                .to_string()
                .contains("turnUrls entries must use valid turn: or turns: URLs")
        );
    }

    #[test]
    fn peer_transport_rejects_invalid_endpoint_schemes() {
        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway.a2a_relay.hub_urls = vec!["https://hub.example.test/relay".to_owned()];
        let error = validate(&cfg).expect_err("HTTP relay URL must fail");
        assert!(
            error
                .to_string()
                .contains("requires valid ws:// or wss:// hubUrl and relays entries")
        );

        let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
        cfg.gateway
            .a2a_relay
            .peer
            .as_mut()
            .expect("peer config")
            .stun_urls = vec!["https://stun.example.test".to_owned()];
        let error = validate(&cfg).expect_err("HTTP STUN URL must fail");
        assert!(
            error
                .to_string()
                .contains("stunUrls entries must use valid stun: or stuns: URLs")
        );

        let mut peer = enabled_peer();
        peer.turn_urls = Some(vec!["stun:turn.example.test:3478".to_owned()]);
        peer.turn_username = Some("node-a".to_owned());
        peer.turn_credential = Some(SecretOrString::Plain("credential".to_owned()));
        let cfg = peer_config(peer, A2aRelayMode::Spoke);
        let error = validate(&cfg).expect_err("STUN scheme in TURN list must fail");
        assert!(
            error
                .to_string()
                .contains("turnUrls entries must use valid turn: or turns: URLs")
        );

        for malformed in [
            "stun:/",
            "stun:?transport=udp",
            "stun://stun.example.test:3478",
            "stun:user@stun.example.test:3478",
            "stun:stun.example.test:3478/path",
        ] {
            let mut cfg = peer_config(enabled_peer(), A2aRelayMode::Spoke);
            cfg.gateway
                .a2a_relay
                .peer
                .as_mut()
                .expect("peer config")
                .stun_urls = vec![malformed.to_owned()];
            validate(&cfg).expect_err("malformed STUN endpoint must fail");
        }
        for malformed in [
            "turn:/",
            "turn:?transport=udp",
            "turn://turn.example.test:3478",
            "turn:user@turn.example.test:3478",
            "turn:turn.example.test:3478/path",
        ] {
            let mut peer = enabled_peer();
            peer.turn_urls = Some(vec![malformed.to_owned()]);
            peer.turn_username = Some("node-a".to_owned());
            peer.turn_credential = Some(SecretOrString::Plain("credential".to_owned()));
            let cfg = peer_config(peer, A2aRelayMode::Spoke);
            validate(&cfg).expect_err("malformed TURN endpoint must fail");
        }
    }

    #[test]
    fn turn_urls_require_resolved_credentials() {
        let mut peer = enabled_peer();
        peer.turn_urls = Some(vec!["turn:turn.example.test:3478".to_owned()]);
        peer.turn_username = Some("node-a".to_owned());
        let cfg = peer_config(peer, A2aRelayMode::Spoke);
        let error = validate(&cfg).expect_err("TURN without credential must fail");
        assert!(
            error
                .to_string()
                .contains("requires turnUsername and turnCredential")
        );
    }

    #[test]
    fn valid_peer_transport_config_is_accepted() {
        let mut peer = enabled_peer();
        peer.stun_urls = Some(vec![
            "stun:stun.example.test:3478".to_owned(),
            "stuns:stun.example.test:5349".to_owned(),
        ]);
        peer.turn_urls = Some(vec![
            "turn:turn.example.test:3478".to_owned(),
            "turns:turn.example.test:5349".to_owned(),
        ]);
        peer.turn_username = Some("node-a".to_owned());
        peer.turn_credential = Some(SecretOrString::Plain("credential".to_owned()));
        let cfg = peer_config(peer, A2aRelayMode::Spoke);
        validate(&cfg).expect("complete peer transport config should validate");
    }
}
