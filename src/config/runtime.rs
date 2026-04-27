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
    AgentDefaults, AgentEntry, AuthConfig, BindMode, BindingConfig, ChannelsConfig, Config,
    CronConfig, DmScope, ExternalAgentConfig, GatewayMode, HooksConfig, LoggingConfig,
    ModelsConfig, PluginsConfig, ReloadMode, SandboxConfig, SecretOrString, SecretsConfig,
    SessionConfig, SkillsConfig, ToolsConfig,
};

// ---------------------------------------------------------------------------
// Sub-structs
// ---------------------------------------------------------------------------

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
    /// True when `gateway.auth.token` is present in config (Plain or
    /// SecretRef). Used by the validator to avoid a false "no auth token"
    /// warning when the token is a SecretRef that couldn't be resolved at
    /// startup (e.g. file/exec).
    pub auth_token_configured: bool,
    /// True when `gateway.auth.token` was specified as a plain string rather
    /// than a SecretRef.  Used by the validator to emit a security warning
    /// (agents.md §24).
    pub auth_token_is_plaintext: bool,
    pub allow_tailscale: bool,
    pub channel_health_check_minutes: u32,
    pub channel_stale_event_threshold_minutes: u32,
    pub channel_max_restarts_per_hour: u32,
    /// Global default User-Agent for LLM provider requests. Provider-level overrides this.
    pub user_agent: Option<String>,
    /// Default response language (e.g. "Chinese", "English"). Affects registry selection.
    pub language: Option<String>,
}

/// Agent list, per-agent defaults, bindings.  Registry rebuild required on
/// change.
#[derive(Debug, Clone)]
pub struct AgentsRuntime {
    pub defaults: AgentDefaults,
    pub list: Vec<AgentEntry>,
    pub bindings: Vec<BindingConfig>,
    pub external: Vec<ExternalAgentConfig>,
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
}

/// Skills, plugins, tools.  Reload triggers skill/plugin re-scan only.
#[derive(Debug, Clone)]
pub struct ExtRuntime {
    pub tools: Option<ToolsConfig>,
    pub skills: Option<SkillsConfig>,
    pub plugins: Option<PluginsConfig>,
    pub evolution: Option<crate::config::schema::EvolutionConfig>,
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
    pub raw: crate::config::schema::Config,
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
        let auth_token_configured = token_ref.is_some()
            || std::env::var("RSCLAW_AUTH_TOKEN").is_ok()
            || std::env::var("OPENCLAW_GATEWAY_TOKEN").is_ok();
        let auth_token_is_plaintext = token_ref
            .map(|t| matches!(t, SecretOrString::Plain(_)))
            .unwrap_or(false);
        // Use resolve_early() so SecretRef::Env tokens are resolved inline;
        // File/Exec refs return None here and must be resolved later via
        // SecretsManager.
        // Fallback: RSCLAW_AUTH_TOKEN or OPENCLAW_GATEWAY_TOKEN env vars.
        let auth_token = token_ref
            .and_then(|t| t.resolve_early())
            .or_else(|| std::env::var("RSCLAW_AUTH_TOKEN").ok())
            .or_else(|| std::env::var("OPENCLAW_GATEWAY_TOKEN").ok());

        Ok(RuntimeConfig {
            gateway: GatewayRuntime {
                port: gw.port.unwrap_or(18888),
                mode: gw.mode.unwrap_or(GatewayMode::Local),
                bind: gw.bind.unwrap_or(BindMode::Loopback),
                bind_address: gw.bind_address.clone(),
                reload: gw.reload.unwrap_or(ReloadMode::Hybrid),
                auth_token,
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
                external: agents_cfg.external.unwrap_or_default(),
            },
            channel: ChannelRuntime {
                channels: self.channels.unwrap_or_default(),
                session: self.session.unwrap_or_else(default_session),
            },
            model: ModelRuntime {
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

fn default_session() -> SessionConfig {
    SessionConfig {
        dm_scope: Some(DmScope::PerChannelPeer),
        thread_bindings: None,
        reset: None,
        identity_links: None,
        maintenance: None,
    }
}
