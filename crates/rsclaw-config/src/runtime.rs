//! RuntimeConfig — the unified config consumed by all modules after loading.
//!
//! `Config` (schema layer, lots of Option<T>) is converted into this
//! representation via `IntoRuntime`, which also applies defaults and validates
//! cross-field constraints.
//!
//! Sub-structs are grouped by hot-reload domain so each can be independently
//! swapped via `Arc<RwLock<T>>` without touching the rest:
//!
//!   GatewayRuntime  — network / auth / channel-health knobs
//!   AgentsRuntime   — agent list, per-agent defaults, bindings
//!   ChannelRuntime  — channel drivers + session routing
//!   ModelRuntime    — provider registry + auth
//!   ExtRuntime      — skills, plugins, tools
//!   OpsRuntime      — cron, hooks, sandbox, logging, secrets

use anyhow::Result;

use super::schema::{
    A2aPeerConfig, A2aRelayMode, A2aRelayStrategy, AgentDefaults, AgentEntry, AuthConfig, BindMode,
    BindingConfig, ChannelsConfig, Config, CronConfig, DmScope, GatewayMode, HooksConfig,
    LoggingConfig, ModelsConfig, PluginsConfig, ReloadMode, RetryConfig, SandboxConfig,
    SecretOrString, SecretsConfig, SessionConfig, SkillsConfig, ToolsConfig,
};

// ---------------------------------------------------------------------------
// Sub-structs
// ---------------------------------------------------------------------------

/// A resolved, accepted A2A inbound credential: the `secret` that authenticates
/// as principal `id`, carrying optional `scopes` for future per-method
/// authorization (A2A §7.5). Anonymous (legacy / env) credentials get a
/// synthetic id like `legacy:bearer:0`.
#[derive(Debug, Clone)]
pub struct A2aPrincipal {
    pub id: String,
    pub secret: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aRelayModeRuntime {
    Disabled,
    Hub,
    Spoke,
}

impl Default for A2aRelayModeRuntime {
    fn default() -> Self {
        Self::Disabled
    }
}

impl From<A2aRelayMode> for A2aRelayModeRuntime {
    fn from(value: A2aRelayMode) -> Self {
        match value {
            A2aRelayMode::Disabled => Self::Disabled,
            A2aRelayMode::Hub => Self::Hub,
            A2aRelayMode::Spoke => Self::Spoke,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum A2aRelayStrategyRuntime {
    PrimaryStandby,
    MultiHome,
}

impl Default for A2aRelayStrategyRuntime {
    fn default() -> Self {
        Self::PrimaryStandby
    }
}

impl From<A2aRelayStrategy> for A2aRelayStrategyRuntime {
    fn from(value: A2aRelayStrategy) -> Self {
        match value {
            A2aRelayStrategy::PrimaryStandby => Self::PrimaryStandby,
            A2aRelayStrategy::MultiHome => Self::MultiHome,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct A2aRelayNodeRuntime {
    pub node_id: String,
    /// Bearer token. Empty string means "token auth disabled" — the node
    /// MUST authenticate via Ed25519 challenge-response (`public_key` set).
    pub token: String,
    /// Base64 Ed25519 public key (raw 32 bytes). When set, the hub
    /// requires a successful challenge-response signature from this node.
    pub public_key: Option<String>,
    pub roles: Vec<String>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct A2aRelayRuntime {
    pub mode: A2aRelayModeRuntime,
    pub relay_id: String,
    pub public_url: Option<String>,
    pub node_id: Option<String>,
    pub hub_urls: Vec<String>,
    pub strategy: A2aRelayStrategyRuntime,
    pub token: Option<String>,
    /// Spoke-side Ed25519 private key (raw base64, 32 bytes). Resolved at
    /// startup from `relay.privateKey` or read from `relay.privateKeyFile`.
    /// When present, spoke uses keypair handshake.
    pub private_key: Option<String>,
    /// Hub-side revocation list — node_ids whose connections are refused.
    pub revoked_nodes: Vec<String>,
    pub nodes: Vec<A2aRelayNodeRuntime>,
    /// WebRTC/ICE direct-transport configuration (ADR 0002). None if disabled.
    pub peer: Option<A2aPeerRelayRuntime>,
}

/// Resolved WebRTC/ICE direct-transport configuration (ADR 0002).
#[derive(Debug, Clone)]
pub struct A2aPeerRelayRuntime {
    pub enabled: bool,
    pub stun_urls: Vec<String>,
    pub turn_urls: Vec<String>,
    pub turn_username: Option<String>,
    pub turn_credential: Option<String>,
    pub listen_port: u16,
}

/// Network / auth / channel-health knobs.  Swappable without restart.
#[derive(Debug, Clone)]
pub struct GatewayRuntime {
    pub port: u16,
    pub mode: GatewayMode,
    pub bind: BindMode,
    /// Custom IP address to bind to (when bind mode is Custom or an IP string).
    pub bind_address: Option<String>,
    pub reload: ReloadMode,
    pub auth_token: Option<String>,
    /// Accepted A2A inbound credentials for `/api/v1/a2a`, each carrying the
    /// principal `id` it authenticates as. Resolved from `gateway.a2a.clients`
    /// plus the deprecated `authTokens`/`apiKeys` (as anonymous principals) and
    /// the env lists `RSCLAW_A2A_BEARER_TOKENS` / `RSCLAW_A2A_API_KEYS`. A
    /// secret matches on either the Bearer or X-API-Key header. Empty Vec means
    /// no credential source was configured and enables dev pass-through;
    /// explicitly configured empty or unresolved credential sources are errors.
    pub a2a_principals: Vec<A2aPrincipal>,
    /// Private rsclaw A2A relay overlay configuration. Standard `/api/v1/a2a`
    /// auth remains in `a2a_principals`; relay credentials are separate.
    pub a2a_relay: A2aRelayRuntime,
    /// Max body size in bytes for `/api/v1/a2a`. Resolved from
    /// `gateway.a2a.maxBodyMb` × 1 MiB. Default 100 MiB. Wired as
    /// `DefaultBodyLimit::max(...)` on the route — axum's stock 2 MiB
    /// is too small for realistic file attachments.
    pub a2a_max_body_bytes: u64,
    /// True when `gateway.auth.token` or a supported gateway-token environment
    /// variable is configured. Config conversion fails when the selected token
    /// cannot be resolved, so this never represents an unresolved credential.
    pub auth_token_configured: bool,
    /// True when `gateway.auth.token` was specified as a plain string rather
    /// than a SecretRef.  Used by the validator to emit a security warning
    /// (agents.md §24).
    pub auth_token_is_plaintext: bool,
    pub allow_tailscale: bool,
    pub channel_health_check_minutes: u32,
    pub channel_stale_event_threshold_minutes: u32,
    pub channel_max_restarts_per_hour: u32,
    /// Global default User-Agent for LLM provider requests. Provider-level
    /// overrides this.
    pub user_agent: Option<String>,
    /// Default response language (e.g. "Chinese", "English"). Affects registry
    /// selection.
    pub language: Option<String>,
}

/// Agent list, per-agent defaults, bindings.  Registry rebuild required on
/// change.
#[derive(Debug, Clone)]
pub struct AgentsRuntime {
    pub defaults: AgentDefaults,
    pub list: Vec<AgentEntry>,
    pub bindings: Vec<BindingConfig>,
    pub a2a: Vec<A2aPeerConfig>,
}

impl AgentsRuntime {
    /// Is the agent `id` flagged `daemon: true` (long-lived monitor loop whose
    /// turn-bounding guards and cron turn-timeout are disabled)?
    pub fn is_daemon_agent(&self, id: &str) -> bool {
        self.list.iter().any(|a| a.daemon && a.id == id)
    }

    /// IDs of all agents flagged `daemon: true`.
    pub fn daemon_agent_ids(&self) -> Vec<String> {
        self.list
            .iter()
            .filter(|a| a.daemon)
            .map(|a| a.id.clone())
            .collect()
    }
}

/// Channel drivers + session routing.  Swappable per-channel.
#[derive(Debug, Clone)]
pub struct ChannelRuntime {
    pub channels: ChannelsConfig,
    pub session: SessionConfig,
}

/// LLM provider registry + auth config.  ProviderRegistry rebuild is cheap.
#[derive(Debug, Clone)]
pub struct ModelRuntime {
    pub models: Option<ModelsConfig>,
    pub auth: Option<AuthConfig>,
    /// Resolved provider-level retry configuration (from `models.retry`).
    /// Stored as the schema type; callers convert to the provider crate's
    /// `RetryConfig` at the point of use.
    pub retry: Option<RetryConfig>,
}

/// Skills, plugins, tools.  Reload triggers skill/plugin re-scan only.
#[derive(Debug, Clone)]
pub struct ExtRuntime {
    pub tools: Option<ToolsConfig>,
    pub skills: Option<SkillsConfig>,
    pub plugins: Option<PluginsConfig>,
    pub evolution: Option<crate::schema::EvolutionConfig>,
}

/// Operational: cron, hooks, sandbox, logging, secrets.  Rarely change.
#[derive(Debug, Clone)]
pub struct OpsRuntime {
    pub cron: Option<CronConfig>,
    pub hooks: Option<HooksConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub logging: Option<LoggingConfig>,
    pub secrets: Option<SecretsConfig>,
}

// ---------------------------------------------------------------------------
// RuntimeConfig
// ---------------------------------------------------------------------------

/// Top-level runtime config — composed of domain sub-structs.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub gateway: GatewayRuntime,
    pub agents: AgentsRuntime,
    pub channel: ChannelRuntime,
    pub model: ModelRuntime,
    pub ext: ExtRuntime,
    pub ops: OpsRuntime,
    /// Original parsed config — retained for sections not yet mapped to runtime
    /// types.
    pub raw: crate::schema::Config,
}

impl RuntimeConfig {
    /// Resolve the default agent (the one with `default: true`, or the first).
    pub fn default_agent(&self) -> Option<&AgentEntry> {
        self.agents
            .list
            .iter()
            .find(|a| a.default == Some(true))
            .or_else(|| self.agents.list.first())
    }

    /// Resolve a specific agent by ID.
    pub fn agent_by_id(&self, id: &str) -> Option<&AgentEntry> {
        self.agents.list.iter().find(|a| a.id == id)
    }
}

// ---------------------------------------------------------------------------
// Conversion from Config
// ---------------------------------------------------------------------------

pub trait IntoRuntime {
    fn into_runtime(self) -> Result<RuntimeConfig>;
}

impl IntoRuntime for Config {
    fn into_runtime(self) -> Result<RuntimeConfig> {
        let raw = self.clone();
        let gw = self.gateway.unwrap_or_default();
        let agents_cfg = self.agents.unwrap_or_default();

        // Resolve auth token before consuming `gw`.
        let token_ref = gw.auth.as_ref().and_then(|a| a.token.as_ref());
        let secrets_cfg = self.secrets.as_ref();
        // Config has strict priority over environment credentials. Do not let a
        // stale lower-priority environment variable invalidate or replace an
        // explicitly configured token.
        let (env_auth_token, legacy_env_auth_token) = if token_ref.is_none() {
            (
                configured_env_secret("RSCLAW_AUTH_TOKEN")?,
                configured_env_secret("OPENCLAW_GATEWAY_TOKEN")?,
            )
        } else {
            (None, None)
        };
        let auth_token_configured =
            token_ref.is_some() || env_auth_token.is_some() || legacy_env_auth_token.is_some();
        let auth_token_is_plaintext = token_ref
            .map(|t| matches!(t, SecretOrString::Plain(_)))
            .unwrap_or(false);
        // An explicitly configured token must resolve. Falling through to an
        // environment token after a broken config reference would silently
        // change the intended credential and can expose the gateway.
        let auth_token = match token_ref {
            Some(token) => Some(resolve_required_secret(
                token,
                secrets_cfg,
                "gateway.auth.token",
            )?),
            None => env_auth_token.or(legacy_env_auth_token),
        };

        // A2A inbound auth — resolve config-listed tokens/keys, then merge
        // env-set lists for back-compat with the original env-only design.
        // Empty in both => middleware passes through (dev mode).
        let resolve_list =
            |field: &str, list: Option<&Vec<SecretOrString>>| -> Result<Vec<String>> {
                let Some(values) = list else {
                    return Ok(Vec::new());
                };
                if values.is_empty() {
                    anyhow::bail!("{field} is configured but empty");
                }
                values
                    .iter()
                    .enumerate()
                    .map(|(index, secret)| {
                        resolve_required_secret(secret, secrets_cfg, &format!("{field}[{index}]"))
                    })
                    .collect()
            };
        let env_bearer_tokens = configured_env_secret_list("RSCLAW_A2A_BEARER_TOKENS")?;
        let env_api_keys = configured_env_secret_list("RSCLAW_A2A_API_KEYS")?;
        // Unified A2A credential pool. Named `clients` resolve to their own id;
        // the deprecated authTokens/apiKeys and env lists become anonymous
        // principals. A secret is accepted on either header at request time —
        // transport is the caller's choice, not a config axis.
        let mut a2a_principals: Vec<A2aPrincipal> = Vec::new();
        if let Some(clients) = gw.a2a.as_ref().and_then(|a| a.clients.as_ref()) {
            if clients.is_empty() {
                anyhow::bail!("gateway.a2a.clients is configured but empty");
            }
            for (index, client) in clients.iter().enumerate() {
                let secret = resolve_required_secret(
                    &client.secret,
                    secrets_cfg,
                    &format!("gateway.a2a.clients[{index}].secret"),
                )?;
                a2a_principals.push(A2aPrincipal {
                    id: client.id.clone(),
                    secret,
                    scopes: client.scopes.clone().unwrap_or_default(),
                });
            }
        }
        let anon = |secret: String, kind: &str, n: usize| A2aPrincipal {
            id: format!("legacy:{kind}:{n}"),
            secret,
            // Legacy token/API-key lists predate scopes and historically grant
            // full A2A access. Preserve that behavior explicitly rather than
            // making an authenticated legacy client unusable.
            scopes: vec!["*".to_owned()],
        };
        for (n, s) in resolve_list(
            "gateway.a2a.authTokens",
            gw.a2a.as_ref().and_then(|a| a.auth_tokens.as_ref()),
        )?
        .into_iter()
        .chain(env_bearer_tokens)
        .enumerate()
        {
            a2a_principals.push(anon(s, "bearer", n));
        }
        for (n, s) in resolve_list(
            "gateway.a2a.apiKeys",
            gw.a2a.as_ref().and_then(|a| a.api_keys.as_ref()),
        )?
        .into_iter()
        .chain(env_api_keys)
        .enumerate()
        {
            a2a_principals.push(anon(s, "apikey", n));
        }
        let a2a_max_body_bytes: u64 =
            gw.a2a.as_ref().and_then(|a| a.max_body_mb).unwrap_or(100) as u64 * 1024 * 1024;
        let a2a_relay = gw
            .a2a
            .as_ref()
            .and_then(|a| a.relay.as_ref())
            .map(|relay| -> Result<A2aRelayRuntime> {
                let mode = relay
                    .mode
                    .clone()
                    .map(A2aRelayModeRuntime::from)
                    .unwrap_or_default();
                let relay_id = relay
                    .relay_id
                    .clone()
                    .or_else(|| relay.node_id.clone())
                    .unwrap_or_else(|| "main".to_owned());
                let mut hub_urls = Vec::new();
                if let Some(url) = relay.hub_url.clone() {
                    hub_urls.push(url);
                }
                if let Some(urls) = relay.relays.clone() {
                    hub_urls.extend(urls);
                }
                let nodes = relay
                    .nodes
                    .as_ref()
                    .map(|nodes| {
                        nodes
                            .iter()
                            .enumerate()
                            .map(|(index, node)| {
                                let token = match node.token.as_ref() {
                                    Some(token) => resolve_required_secret(
                                        token,
                                        secrets_cfg,
                                        &format!("gateway.a2a.relay.nodes[{index}].token"),
                                    )?,
                                    None => String::new(),
                                };
                                let public_key = node.public_key.clone();
                                if token.is_empty() && public_key.is_none() {
                                    anyhow::bail!(
                                        "gateway.a2a.relay.nodes[{index}] requires token or publicKey"
                                    );
                                }
                                Ok(A2aRelayNodeRuntime {
                                    node_id: node.node_id.clone(),
                                    token,
                                    public_key,
                                    roles: node.roles.clone().unwrap_or_default(),
                                    scopes: node.scopes.clone().unwrap_or_default(),
                                })
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .transpose()?
                    .unwrap_or_default();
                // Spoke private key: prefer inline `privateKey`, else require
                // `privateKeyFile` to be readable and non-empty. Configured
                // relay credentials must never silently degrade to no auth.
                let private_key = match relay.private_key.as_ref() {
                    Some(key) => Some(resolve_required_secret(
                        key,
                        secrets_cfg,
                        "gateway.a2a.relay.privateKey",
                    )?),
                    None => match relay.private_key_file.as_ref() {
                        Some(path) => {
                            let content = std::fs::read_to_string(path).map_err(|error| {
                                anyhow::anyhow!(
                                    "gateway.a2a.relay.privateKeyFile '{path}' cannot be read: {error}"
                                )
                            })?;
                            let content = content.trim();
                            if content.is_empty() {
                                anyhow::bail!(
                                    "gateway.a2a.relay.privateKeyFile '{path}' is empty"
                                );
                            }
                            Some(content.to_owned())
                        }
                        None => None,
                    },
                };
                let token = relay
                    .token
                    .as_ref()
                    .map(|token| {
                        resolve_required_secret(token, secrets_cfg, "gateway.a2a.relay.token")
                    })
                    .transpose()?;
                let peer = relay.peer.as_ref().map(|p| A2aPeerRelayRuntime {
                    enabled: p.enabled,
                    stun_urls: p.stun_urls.clone().unwrap_or_default(),
                    turn_urls: p.turn_urls.clone().unwrap_or_default(),
                    turn_username: p.turn_username.clone(),
                    turn_credential: p
                        .turn_credential
                        .as_ref()
                        .and_then(|credential| credential.resolve_full(secrets_cfg)),
                    listen_port: p.listen_port.unwrap_or(0),
                });
                Ok(A2aRelayRuntime {
                    mode,
                    relay_id,
                    public_url: relay.public_url.clone(),
                    node_id: relay.node_id.clone(),
                    hub_urls,
                    strategy: relay
                        .strategy
                        .clone()
                        .map(A2aRelayStrategyRuntime::from)
                        .unwrap_or_default(),
                    token,
                    private_key,
                    revoked_nodes: relay.revoked_nodes.clone().unwrap_or_default(),
                    nodes,
                    peer,
                })
            })
            .transpose()?
            .unwrap_or_default();

        Ok(RuntimeConfig {
            gateway: GatewayRuntime {
                port: gw.port.unwrap_or(18888),
                mode: gw.mode.unwrap_or(GatewayMode::Local),
                bind: gw.bind.unwrap_or(BindMode::Loopback),
                bind_address: gw.bind_address.clone(),
                reload: gw.reload.unwrap_or(ReloadMode::Hybrid),
                auth_token,
                a2a_principals,
                a2a_relay,
                a2a_max_body_bytes,
                auth_token_configured,
                auth_token_is_plaintext,
                allow_tailscale: gw
                    .auth
                    .as_ref()
                    .and_then(|a| a.allow_tailscale)
                    .unwrap_or(false),
                channel_health_check_minutes: gw.channel_health_check_minutes.unwrap_or(5),
                channel_stale_event_threshold_minutes: gw
                    .channel_stale_event_threshold_minutes
                    .unwrap_or(30),
                channel_max_restarts_per_hour: gw.channel_max_restarts_per_hour.unwrap_or(10),
                user_agent: gw.user_agent.clone(),
                language: gw.language.clone(),
            },
            agents: AgentsRuntime {
                defaults: agents_cfg.defaults.unwrap_or_default(),
                list: agents_cfg.list.unwrap_or_default(),
                bindings: self.bindings.unwrap_or_default(),
                a2a: agents_cfg.a2a.unwrap_or_default(),
            },
            channel: ChannelRuntime {
                channels: self.channels.unwrap_or_default(),
                session: self.session.unwrap_or_else(default_session),
            },
            model: ModelRuntime {
                retry: self.models.as_ref().and_then(|m| m.retry.clone()),
                models: self.models,
                auth: self.auth,
            },
            ext: ExtRuntime {
                tools: self.tools,
                skills: self.skills,
                plugins: self.plugins,
                evolution: self.evolution,
            },
            ops: OpsRuntime {
                cron: self.cron,
                hooks: self.hooks,
                sandbox: self.sandbox,
                logging: self.logging,
                secrets: self.secrets,
            },
            raw,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_required_secret(
    secret: &SecretOrString,
    secrets: Option<&SecretsConfig>,
    field: &str,
) -> Result<String> {
    let value = secret
        .resolve_full(secrets)
        .ok_or_else(|| anyhow::anyhow!("{field} is configured but could not be resolved"))?;
    if value.trim().is_empty() {
        anyhow::bail!("{field} resolved to an empty value");
    }
    Ok(value)
}

fn configured_env_secret(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.trim().is_empty() => {
            anyhow::bail!("{name} is set but empty");
        }
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("{name} is not valid Unicode");
        }
    }
}

fn configured_env_secret_list(name: &str) -> Result<Vec<String>> {
    let Some(value) = configured_env_secret(name)? else {
        return Ok(Vec::new());
    };
    let values: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    if values.is_empty() {
        anyhow::bail!("{name} contains no credentials");
    }
    Ok(values)
}

fn default_session() -> SessionConfig {
    SessionConfig {
        dm_scope: Some(DmScope::PerChannelPeer),
        thread_bindings: None,
        reset: None,
        identity_links: None,
        maintenance: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{
        A2aClient, A2aRelayConfig, A2aRelayNodeConfig, GatewayA2a, GatewayAuth, GatewayConfig,
        SecretRef, SecretSource,
    };

    fn unresolved_secret() -> SecretOrString {
        SecretOrString::Ref(SecretRef {
            source: SecretSource::File,
            provider: None,
            id: "missing-secret".to_owned(),
        })
    }

    #[test]
    fn configured_gateway_auth_must_resolve() {
        let config = Config {
            gateway: Some(GatewayConfig {
                auth: Some(GatewayAuth {
                    mode: None,
                    token: Some(unresolved_secret()),
                    password: None,
                    allow_tailscale: None,
                    allow_local: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("unresolved gateway auth must fail closed");
        assert!(error.to_string().contains("gateway.auth.token"));
    }

    #[test]
    fn configured_a2a_client_secret_must_resolve() {
        let config = Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    clients: Some(vec![A2aClient {
                        id: "partner".to_owned(),
                        secret: unresolved_secret(),
                        scopes: None,
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("unresolved A2A client secret must fail closed");
        assert!(error.to_string().contains("gateway.a2a.clients[0].secret"));
    }

    #[test]
    fn configured_relay_credentials_must_resolve() {
        let config = Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    relay: Some(A2aRelayConfig {
                        token: Some(unresolved_secret()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("unresolved relay token must fail closed");
        assert!(error.to_string().contains("gateway.a2a.relay.token"));
    }

    #[test]
    fn configured_relay_node_credentials_must_resolve() {
        let config = Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    relay: Some(A2aRelayConfig {
                        nodes: Some(vec![A2aRelayNodeConfig {
                            node_id: "node-a".to_owned(),
                            token: Some(unresolved_secret()),
                            public_key: None,
                            roles: None,
                            scopes: None,
                        }]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("unresolved relay node token must fail closed");
        assert!(
            error
                .to_string()
                .contains("gateway.a2a.relay.nodes[0].token")
        );
    }

    #[test]
    fn configured_relay_private_key_file_must_be_readable() {
        let config = Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    relay: Some(A2aRelayConfig {
                        private_key_file: Some("/missing/rsclaw-relay-private-key".to_owned()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("unreadable relay private key file must fail closed");
        assert!(
            error
                .to_string()
                .contains("gateway.a2a.relay.privateKeyFile")
        );
    }

    #[test]
    fn explicitly_empty_a2a_credential_pool_fails_closed() {
        let config = Config {
            gateway: Some(GatewayConfig {
                a2a: Some(GatewayA2a {
                    clients: Some(Vec::new()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = config
            .into_runtime()
            .expect_err("explicitly empty A2A credentials must not become pass-through");
        assert!(
            error
                .to_string()
                .contains("gateway.a2a.clients is configured but empty")
        );
    }
}
