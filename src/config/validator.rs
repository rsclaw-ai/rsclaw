//! Cross-field constraint validation for RuntimeConfig.
//! Runs after loading + schema deserialization.

use anyhow::{Result, bail};
use tracing::{debug, warn};

use super::{
    runtime::RuntimeConfig,
    schema::DmScope,
};

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
    for ext in &cfg.agents.external {
        if !seen.insert(ext.id.clone()) {
            bail!("duplicate agent id (external conflicts with local): \"{}\"", ext.id);
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
