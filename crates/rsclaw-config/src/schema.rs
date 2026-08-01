//! rsclaw JSON5 config schema — full field coverage.
//! Unknown fields on most structs are silently tolerated (retained in the raw
//! config and written back on save). This is a deliberate trade-off: the only
//! struct that rejects unknown fields is `KbOcrConfig` (because unrecognized
//! OCR params are likely misconfiguration). Future hardening may extend
//! `deny_unknown_fields` to more structs, but this must be done carefully to
//! avoid breaking existing deployments that have extra keys from prior
//! versions or manual edits.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Top-level
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    // --- Core ---
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    pub meta: Option<MetaConfig>,
    pub gateway: Option<GatewayConfig>,
    pub update: Option<UpdateConfig>,
    pub env: Option<EnvConfig>,
    pub agents: Option<AgentsConfig>,
    pub models: Option<ModelsConfig>,
    pub auth: Option<AuthConfig>,
    pub channels: Option<ChannelsConfig>,
    pub session: Option<SessionConfig>,
    pub bindings: Option<Vec<BindingConfig>>,
    pub cron: Option<CronConfig>,
    pub tools: Option<ToolsConfig>,
    pub sandbox: Option<SandboxConfig>,
    pub logging: Option<LoggingConfig>,
    pub skills: Option<SkillsConfig>,
    pub plugins: Option<PluginsConfig>,
    pub evolution: Option<EvolutionConfig>,
    pub hooks: Option<HooksConfig>,
    pub secrets: Option<SecretsConfig>,
    pub memory_search: Option<MemorySearchConfig>,
    pub memory: Option<MemoryTopConfig>,
    pub kb: Option<KbConfig>,
    pub mcp: Option<McpConfig>,
    /// Per-registry overrides keyed by registry name (`iwencai`, `clawhub`,
    /// `skillhub`, ...). Lets users put `apiKey`/`baseUrl` for paid skill
    /// registries into rsclaw.json5 instead of shell rc — at startup any
    /// resolved key is exported into the gateway process env so spawned
    /// skill subprocesses (python CLIs etc.) inherit it transparently.
    pub skill_registries: Option<std::collections::HashMap<String, SkillRegistryEntry>>,

    // --- OpenClaw-compatible sections ---
    pub wizard: Option<Value>,
    pub messages: Option<MessagesConfig>,
    pub commands: Option<CommandsConfig>,
    pub diagnostics: Option<Value>,
    pub cli: Option<CliConfig>,
    pub browser: Option<BrowserConfig>,
    pub ui: Option<UiConfig>,
    pub acp: Option<Value>,
    pub node_host: Option<Value>,
    pub broadcast: Option<Value>,
    pub audio: Option<Value>,
    pub media: Option<Value>,
    pub approvals: Option<ApprovalsConfig>,
    pub discovery: Option<Value>,
    pub canvas_host: Option<CanvasHostConfig>,
    pub talk: Option<TalkConfig>,
    pub web: Option<WebConfig>,
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaConfig {
    pub version: Option<String>,
    pub name: Option<String>,
    /// OpenClaw uses lastTouchedVersion/lastTouchedAt
    pub last_touched_version: Option<String>,
    pub last_touched_at: Option<String>,
    pub timestamp: Option<u64>,
}

// ---------------------------------------------------------------------------
// gateway
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    pub port: Option<u16>,
    pub mode: Option<GatewayMode>,
    pub bind: Option<BindMode>,
    /// Custom bind address (IP string like "192.168.0.169"). Used when bind is
    /// an IP address string instead of a named mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,
    pub auth: Option<GatewayAuth>,
    /// A2A endpoint (`/api/v1/a2a`) authentication. When any field here is
    /// populated, the A2A middleware accepts these credentials.
    /// `gateway.auth.token` is also accepted as a Bearer when set
    /// (single-token convenience). Falls back to env vars
    /// `RSCLAW_A2A_BEARER_TOKENS` / `RSCLAW_A2A_API_KEYS` for back-compat —
    /// env-set lists are merged into the accepted set. When everything is
    /// empty, the middleware passes through (dev mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<GatewayA2a>,
    pub control_ui: Option<ControlUiConfig>,
    pub reload: Option<ReloadMode>,
    pub push: Option<PushConfig>,
    pub channel_health_check_minutes: Option<u32>,
    pub channel_stale_event_threshold_minutes: Option<u32>,
    pub channel_max_restarts_per_hour: Option<u32>,
    pub remote: Option<Value>,
    pub tailscale: Option<Value>,
    pub hot_reload: Option<Value>,
    /// Default response language (e.g. "Chinese", "English", "Japanese").
    pub language: Option<String>,
    /// Global default User-Agent for all LLM provider HTTP requests.
    /// Provider-level user_agent overrides this. Default: "Mozilla/5.0
    /// (compatible; rsclaw/1.0)".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Global HTTP/SOCKS5 proxy URL (e.g. "http://127.0.0.1:7890", "socks5://proxy:1080").
    /// Env var RSCLAW_PROXY overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Proxy allow list: only matching domains use the proxy.
    /// Supports wildcards: "*.openai.com,api.anthropic.com,github.com"
    /// "*" means all domains use proxy. Empty/unset = all (default).
    /// Env var RSCLAW_PROXY_ALLOW overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_allow: Option<String>,
    /// Proxy deny list: matching domains bypass the proxy (added to NO_PROXY).
    /// "localhost,127.0.0.1,192.168.*,10.*"
    /// Env var RSCLAW_PROXY_DENY overrides this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_deny: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GatewayMode {
    Local,
    Cloud,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BindMode {
    Auto,
    Lan,
    Loopback,
    All,
    Custom,
    Tailnet,
}

impl BindMode {
    /// Parse a bind string that might be an enum variant or an IP address.
    pub fn from_config_str(s: &str) -> (Self, Option<String>) {
        match s.to_lowercase().as_str() {
            "auto" => (Self::Auto, None),
            "lan" => (Self::Lan, None),
            "loopback" => (Self::Loopback, None),
            "all" => (Self::All, None),
            "custom" => (Self::Custom, None),
            "tailnet" => (Self::Tailnet, None),
            // Treat as IP address
            _ => (Self::Custom, Some(s.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayAuth {
    pub mode: Option<String>,
    pub token: Option<SecretOrString>,
    pub password: Option<SecretOrString>,
    pub allow_tailscale: Option<bool>,
    pub allow_local: Option<bool>,
}

/// A2A inbound configuration in the gateway block. JSON5 key
/// `gateway.a2a`. Auth lists support plain strings, `${VAR}` expansion,
/// and `{ source: "env", id: "..." }` refs.
/// One named A2A client credential. The `secret` is accepted on EITHER the
/// `Authorization: Bearer` or `X-API-Key` header — transport is the caller's
/// choice, not a server config axis. Once the secret matches, the request is
/// attributed to `id` (the principal). `scopes` is reserved for per-method
/// authorization (A2A spec §7.5) and is not enforced yet.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aClient {
    /// Logical identity this credential authenticates as (e.g. "partner-acme").
    pub id: String,
    /// Accepted secret: plain string, `${ENV}`, or a secret ref.
    pub secret: SecretOrString,
    /// Reserved for per-method authorization (A2A §7.5); unenforced today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayA2a {
    /// Named A2A client credentials for `/api/v1/a2a`. Each secret is accepted
    /// on either the Bearer or X-API-Key header and resolves to its `id` as the
    /// request principal. Prefer this over `authTokens`/`apiKeys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<Vec<A2aClient>>,
    /// DEPRECATED: prefer `clients`. Accepted Bearer tokens for `/api/v1/a2a`;
    /// each becomes an anonymous principal. Kept for config compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_tokens: Option<Vec<SecretOrString>>,
    /// DEPRECATED: prefer `clients`. Accepted `X-API-Key` values; each becomes
    /// an anonymous principal. Kept for config compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_keys: Option<Vec<SecretOrString>>,
    /// Max body size in megabytes for `/api/v1/a2a` requests. Default
    /// 100 MB. axum's built-in default is 2 MB which immediately
    /// 413's any non-trivial file attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_mb: Option<u32>,
    /// Private rsclaw relay overlay. Standard A2A remains HTTP/SSE; this
    /// config controls the rsclaw-only WS hub/spoke transport used for
    /// outbound-only private nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<A2aRelayConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum A2aRelayMode {
    Disabled,
    Hub,
    Spoke,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum A2aRelayStrategy {
    PrimaryStandby,
    MultiHome,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct A2aRelayConfig {
    pub mode: Option<A2aRelayMode>,
    pub relay_id: Option<String>,
    pub public_url: Option<String>,
    pub node_id: Option<String>,
    pub hub_url: Option<String>,
    pub relays: Option<Vec<String>>,
    pub strategy: Option<A2aRelayStrategy>,
    pub token: Option<SecretOrString>,
    /// Spoke-side Ed25519 private key (base64 raw 32-byte). If set, spoke
    /// uses keypair handshake instead of (or in addition to) the bearer
    /// token. Prefer `private_key_file` for production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<SecretOrString>,
    /// Path to a file containing the base64 Ed25519 private key. Read at
    /// startup; never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_file: Option<String>,
    /// List of revoked node_ids (hub-side). Connections from these nodes
    /// are refused regardless of token/keypair. Hot-reloadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_nodes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Vec<A2aRelayNodeConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aRelayNodeConfig {
    pub node_id: String,
    /// Bearer token. Optional when `public_key` is set; required otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretOrString>,
    /// Base64 Ed25519 public key (raw 32 bytes). When set, this node MUST
    /// pass challenge-response auth even if it also presents a token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlUiConfig {
    pub enabled: Option<bool>,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReloadMode {
    Hot,
    Hybrid,
    Restart,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushConfig {
    pub apns: Option<ApnsConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApnsConfig {
    pub relay_url: Option<String>,
    pub token: Option<SecretOrString>,
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfig {
    pub channel: Option<String>,
    pub auto: Option<bool>,
    pub check: Option<bool>,
    pub notify: Option<bool>,
}

// ---------------------------------------------------------------------------
// env
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvConfig(pub HashMap<String, String>);

// ---------------------------------------------------------------------------
// agents
// ---------------------------------------------------------------------------

/// A remote A2A peer this gateway outbound-calls via the `agent_<id>` tool.
///
/// Renamed from `ExternalAgentConfig` once the A2A v1.0 wire shape landed;
/// the old name was protocol-agnostic ("external" could mean shell, HTTP,
/// anything), the new name pins down that the transport is A2A. Hard switch
/// — no back-compat alias since the prior implementation was incomplete and
/// not deployed anywhere.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2aPeerConfig {
    /// Logical agent ID (used as `agent_<id>` tool name).
    pub id: String,
    /// Base URL of the remote rsclaw/OpenClaw gateway, e.g. "http://host:18888".
    pub url: String,
    /// Optional bearer token presented to the remote gateway. Plain string,
    /// `${ENV}`, or a secret ref — so partner credentials need not sit in
    /// plaintext config (parity with `gateway.a2a` secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretOrString>,
    /// Remote agent ID to target. If omitted, uses the remote gateway's default
    /// agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_agent_id: Option<String>,
    /// Human-written capability blurb shown to the LLM as the tool's
    /// description. Without this, the auto-generated description is just
    /// "Send a task to remote agent 'X'" — opaque, and 9B-class models
    /// won't route to the peer because they can't tell what it does. Put
    /// trigger keywords here: 生图/生视频/数字人/对口型/TTS/抖音 ...
    /// Falls back to "Send a task to remote agent '<id>' at <url>" when
    /// unset (back-compat with older configs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentsConfig {
    pub defaults: Option<AgentDefaults>,
    pub list: Option<Vec<AgentEntry>>,
    /// Remote A2A peers. JSON5 key: `a2a`.
    pub a2a: Option<Vec<A2aPeerConfig>>,
}

impl AgentsConfig {
    /// IDs of agents flagged `daemon: true` — long-lived monitor loops whose
    /// turn-bounding guards and cron turn-timeout are disabled.
    pub fn daemon_agent_ids(&self) -> Vec<String> {
        self.list
            .as_ref()
            .map(|l| {
                l.iter()
                    .filter(|a| a.daemon)
                    .map(|a| a.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Is the agent `id` flagged `daemon: true`?
    pub fn is_daemon_agent(&self, id: &str) -> bool {
        self.list
            .as_ref()
            .is_some_and(|l| l.iter().any(|a| a.daemon && a.id == id))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefaults {
    pub workspace: Option<String>,
    pub model: Option<ModelConfig>,
    /// Cheap/fast model used for internal low-stakes sub-tasks: query
    /// planning, intent classification, summarization hints, etc. Falls back
    /// to `model` when unset. Keep it pointed at something ~50x cheaper than
    /// the main model (gpt-4o-mini / qwen-turbo / doubao-lite / local 1.5B).
    pub flash_model: Option<ModelConfig>,
    pub models: Option<HashMap<String, ModelAlias>>,
    pub max_concurrent: Option<u32>,
    pub context_pruning: Option<ContextPruningConfig>,
    pub compaction: Option<CompactionConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub sandbox: Option<AgentSandboxConfig>,
    pub image_max_dimension_px: Option<u32>,
    pub group_chat: Option<GroupChatConfig>,
    pub skip_bootstrap: Option<bool>,
    pub block_streaming_default: Option<bool>,
    pub timeout_seconds: Option<u32>,
    pub prompt_mode: Option<PromptMode>,
    pub memory: Option<MemoryConfig>,
    pub subagents: Option<Value>,
    pub bootstrap: Option<Value>,
    pub image: Option<Value>,
    pub pdf: Option<Value>,
    pub image_gen: Option<Value>,
    pub repo_root: Option<String>,
    pub context_tokens: Option<u32>,
    /// KV cache mode: 0 = off, 1 = full/append-only (default), 2 =
    /// delta/incremental.
    pub kv_cache_mode: Option<u8>,
    /// Maximum token budget for `/btw` side-channel quick queries
    /// (handled by `handle_side_query`). Default: 5000.
    pub btw_tokens: Option<u32>,
    /// Maximum tokens a SINGLE turn may add to the session input
    /// (user message + tool_results), before the turn is sent upstream.
    /// Oversized tool_results are compressed down until the turn's new
    /// input fits this budget. This caps per-turn growth so one giant
    /// tool output (a huge web page, a multi-image batch) can't blow the
    /// kvCacheMode=2 session past the worker's `--rsclaw-max-session-ctx`
    /// in a single step. It is NOT the total context budget — the total
    /// is bounded by the worker's max-session-ctx; this only bounds the
    /// per-turn delta. Default: 5000.
    pub max_per_turn_input_tokens: Option<u32>,
    /// Max tokens a SINGLE `read_artifact` call may return in one page.
    /// `read_artifact` is the explicit "give me this content" path, so it
    /// gets a much wider budget than the generic per-turn floor
    /// (`max_per_turn_input_tokens`) — most SKILL.md / web pages then come
    /// back whole in one read and never need paging. Still BOUNDED: a
    /// deliberate read can't blow the session context in one shot. Content
    /// beyond this size is served via the server-side read cursor (each
    /// `read_artifact` returns the next chunk ≤ this budget). The per-turn
    /// aggregate guard is raised to at least this value so the page actually
    /// reaches the model instead of being re-trimmed. Default: 16000.
    pub max_artifact_read_tokens: Option<u32>,
    pub timezone: Option<String>,
    pub timestamp: Option<Value>,
    pub thinking: Option<ThinkingConfig>,
    pub strip_think_tags: Option<bool>,
    pub frequency_penalty: Option<f32>,
    /// Sampling temperature: None = "auto" (use built-in heuristic — 0.7 for
    /// chat, 0.6 with tools, None for thinking models). Some(t) = explicit
    /// override, where 0.0 is fully deterministic and 1.0 is most random.
    pub temperature: Option<f32>,
    pub streaming: Option<StreamingMode>,
    pub timeout: Option<Value>,
    pub tools: Option<Value>,
    pub subagent: Option<Value>,
    pub cli: Option<Value>,
    pub media: Option<Value>,
    pub embedded: Option<Value>,
    pub archive: Option<Value>,
    /// Stagnation budget per turn (progress-aware). Simple tasks: 50, complex
    /// (browser/shell/cap): 100. Budget depletes on stagnation/errors, not on
    /// productive iterations. Set to 0 to use built-in defaults.
    pub max_iterations: Option<u32>,
    /// Send intermediate text to user during multi-step tool calls. Default:
    /// true.
    pub intermediate_output: Option<bool>,
    /// OpenCode ACP configuration for default agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opencode: Option<OpenCodeConfig>,
    /// Claude Code ACP configuration for default agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claudecode: Option<ClaudeCodeConfig>,
    /// Codex MCP configuration for default agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Capability blurb shown to OTHER agents as this agent's `agent_<id>`
    /// tool description (what it's for + trigger keywords). Without it the
    /// tool reads just "Send a task to agent '<id>'" — opaque, so a routing
    /// agent can't tell WHEN to delegate here and falls back to doing the
    /// work itself. Mirrors `A2aPeerConfig.description` for local agents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelConfig>,
    /// Per-agent flash model override. Falls back to defaults.flash_model,
    /// then to this agent's `model`, then to defaults.model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flash_model: Option<ModelConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane_concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_chat: Option<GroupChatConfig>,
    /// rsclaw extension: which channels this agent handles (None = all)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
    /// Custom slash commands for this agent (in addition to built-in ones)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<AgentCommand>>,
    /// Allowed pre-parsed commands: "*" = all (default for main agent),
    /// pipe-separated list = specific (e.g. "/help|/search|/version"),
    /// empty/null = none (default for non-main agents).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_commands: Option<String>,
    /// rsclaw extension: use OpenCode ACP client instead of LLM
    /// When set, this agent spawns opencode acp subprocess and routes all
    /// prompts through it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opencode: Option<OpenCodeConfig>,
    /// rsclaw extension: use Claude Code ACP client instead of LLM
    /// When set, this agent spawns claude-agent-acp subprocess and routes all
    /// prompts through it. Uses the official Claude Agent SDK via ACP
    /// protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claudecode: Option<ClaudeCodeConfig>,
    /// rsclaw extension: use Codex MCP client instead of LLM
    /// When set, this agent spawns codex mcp-server subprocess and routes all
    /// prompts through it. Uses OpenAI Codex CLI via MCP protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexConfig>,
    /// OpenClaw-specific
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Per-agent sampling temperature override. None = inherit from
    /// `agents.defaults.temperature` (which itself may be None = "auto").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// rsclaw extension: run this agent as a long-lived DAEMON loop (a realtime
    /// monitor that polls forever). Disables the turn-bounding guards (hard
    /// iteration ceiling, stagnation budget, same-call/same-name repeat breaks)
    /// and the cron turn-timeout — the loop only ends on error or external
    /// stop, with the agent's own cron as a restart backstop. Default
    /// false. Replaces the old top-level `agents.defaults.daemonAgentIds`
    /// list.
    #[serde(default)]
    pub daemon: bool,
}

/// OpenCode ACP configuration for an agent.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeConfig {
    /// Path to opencode binary (default: "opencode")
    pub command: Option<String>,
    /// Arguments passed to opencode acp (default: ["acp"])
    pub args: Option<Vec<String>>,
    /// Workspace directory for opencode (default: ".")
    pub cwd: Option<String>,
    /// Default model ID (e.g., "opencode/big-pickle", "alibaba/qwen3.5-plus")
    pub model: Option<String>,
    /// Timeout for initialization/session creation in seconds (default: 600).
    /// OpenCode/ClaudeCode/Codex can take long to initialize as they load MCP
    /// servers.
    pub init_timeout_seconds: Option<u64>,
}

/// Claude Code ACP configuration for an agent.
/// Uses claude-agent-acp (https://github.com/agentclientprotocol/claude-agent-acp)
/// which wraps the Claude Agent SDK with ACP protocol.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeConfig {
    /// Path to claude-agent-acp binary (default: "claude-agent-acp")
    pub command: Option<String>,
    /// Arguments passed to claude-agent-acp (default: [])
    pub args: Option<Vec<String>>,
    /// Workspace directory for claude code (default: agent's workspace)
    pub cwd: Option<String>,
    /// Claude model ID (e.g., "claude-sonnet-4-6", "claude-opus-4-6")
    pub model: Option<String>,
    /// Timeout for initialization/session creation in seconds (default: 600).
    /// OpenCode/ClaudeCode/Codex can take long to initialize as they load MCP
    /// servers.
    pub init_timeout_seconds: Option<u64>,
}

/// Codex MCP configuration for an agent.
/// Uses Codex CLI's MCP server mode (codex mcp-server).
/// Codex provides tools: `codex` (start session) and `codex-reply` (continue
/// session).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexConfig {
    /// Path to codex binary (default: "codex")
    pub command: Option<String>,
    /// Arguments passed to codex (default: ["mcp-server"])
    pub args: Option<Vec<String>>,
    /// Workspace directory for codex (default: agent's workspace)
    pub cwd: Option<String>,
    /// OpenAI model ID (optional, uses Codex default)
    pub model: Option<String>,
    /// Timeout for initialization/session creation in seconds (default: 600).
    /// OpenCode/ClaudeCode/Codex can take long to initialize as they load MCP
    /// servers.
    pub init_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCommand {
    /// Command name without slash (e.g. "deploy")
    pub name: String,
    /// Description shown in /help
    pub description: String,
    /// What to execute: tool name or shell command
    pub action: String,
    /// Optional: static arguments
    pub args: Option<Value>,
}

/// A model field that accepts either a single id ("doubao/seed-2.0-pro") or
/// an ordered list (["rsclaw/agent-v1", "doubao/seed-2.0-pro", …]). The first
/// element is the preferred model; subsequent entries are tried in order on
/// transient failure (rate limit, 5xx, timeout). On permanent failure (auth,
/// balance, model-not-found) the entry is marked Disabled and skipped — see
/// `provider::health` for the state machine. Empty chains are treated as
/// "unset".
///
/// Serde is untagged with `String` first so a bare string ("doubao/x") wins
/// over the array variant. JSON5 round-trip preserves the original form —
/// users writing `primary: "doubao/x"` see `primary: "doubao/x"` after save.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum StringOrVec {
    Single(String),
    Multi(Vec<String>),
}

impl StringOrVec {
    /// First non-empty model id in the chain, or `None` if empty.
    pub fn head(&self) -> Option<&str> {
        match self {
            Self::Single(s) => {
                let t = s.trim();
                if t.is_empty() { None } else { Some(t) }
            }
            Self::Multi(v) => v.iter().map(|s| s.trim()).find(|s| !s.is_empty()),
        }
    }

    /// Full chain as borrowed slice; trimmed; empties filtered.
    pub fn chain(&self) -> Vec<&str> {
        match self {
            Self::Single(s) => {
                let t = s.trim();
                if t.is_empty() { vec![] } else { vec![t] }
            }
            Self::Multi(v) => v
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Owned chain — handy for failover construction.
    pub fn to_chain(&self) -> Vec<String> {
        self.chain().into_iter().map(String::from).collect()
    }

    /// Construct from a single id — convenience for default-builder code.
    pub fn single(s: impl Into<String>) -> Self {
        Self::Single(s.into())
    }
}

impl From<&str> for StringOrVec {
    fn from(s: &str) -> Self {
        Self::Single(s.to_owned())
    }
}

impl From<String> for StringOrVec {
    fn from(s: String) -> Self {
        Self::Single(s)
    }
}

impl From<Vec<String>> for StringOrVec {
    fn from(v: Vec<String>) -> Self {
        Self::Multi(v)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// Preferred chat / agent model. May be a single id or an ordered chain
    /// (see [`StringOrVec`]); chain order is "try in order, demote failing
    /// entries". On full chain exhaustion the primary path falls through to
    /// the legacy `fallbacks` list below — this extra escape is primary-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<StringOrVec>,
    /// Emergency tail appended to `primary` chain after the array is
    /// exhausted. Kept as `Vec<String>` (not `StringOrVec`) because the
    /// fallback list is semantically distinct: these models should only kick
    /// in when everything in `primary` is unreachable, never as the user's
    /// active routing preference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<String>>,
    /// Text-to-image model chain, e.g.
    /// `["doubao/doubao-seedream-5-0-260128", "openai/dall-e-3"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<StringOrVec>,
    /// Text-to-video model chain, e.g. `"doubao/doubao-seedance-2-0-260128"`
    /// or an array across Seedance / Hailuo / Kling. Note: only retried on
    /// **submit** failure — once a clip is submitted the polling loop sticks
    /// with that provider to avoid double-billing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video: Option<StringOrVec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// Whether to send tool definitions to the model. Default: true.
    /// Set to false for reasoning models (deepseek-r1, qwen-r1) that don't
    /// support tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_enabled: Option<bool>,
    /// Tool set level: "minimal" (7 core), "standard" (12, default), "full"
    /// (all 32+).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolset: Option<String>,
    /// Tool whitelist for this agent. When set, the LLM sees ONLY these
    /// tool names (the `toolset` preset becomes the fallback baseline,
    /// not an additive base). Use for hub-router agents and any setup
    /// where you want explicit control over what the model can call.
    /// Examples: `["agent_spoke_aihub", "memory", "ask_user"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// Per-agent always-on PIN list for plugin tools. Each entry is
    /// `"<plugin>__<tool>"` (or legacy `"<plugin>.<tool>"`, auto-converted).
    /// Additive on top of the plugin author's `headline: true` defaults —
    /// promotes a non-headline tool to a real ToolDef in
    /// `dynamic_prefix.user_tools` so the model can call it directly
    /// (without going through the `plugin_search` → `plugin_describe`
    /// → `plugin_invoke` 3-step dance that small (9B-class) models
    /// struggle with).
    ///
    /// Pre-v1.9 semantics rendered these as text inside `user_system`
    /// and capped at MAX_INJECT_TOOLS=20. As of v1.9 the worker exposes
    /// a `user_tools` cache segment that does not dirty the base
    /// prefix, so these now land as structured ToolDefs alongside the
    /// builtin set; the cap is shifted to `user_tools_cap` (default 30)
    /// and applies to the combined headline+pin set across active
    /// plugins.
    ///
    /// Operator-level analog of the per-session `/plugin pin <name>`
    /// slash command (which writes to `PluginOverride.pin`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_tools: Option<Vec<String>>,
    /// Per-agent UNPIN list — subtractive override that removes
    /// headline-tagged or otherwise-defaulted plugin tools from
    /// `user_tools` for this agent. Use to suppress destructive or
    /// agent-irrelevant tools that the plugin author marked
    /// `headline: true` (e.g. an analytics agent doesn't need
    /// `douyin__delete_video`). Entries are `"<plugin>__<tool>"`
    /// (or legacy dot form).
    ///
    /// Operator-level analog of `/plugin unpin <name>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_tools_unpin: Option<Vec<String>>,
    /// Maximum number of plugin tools to expose simultaneously in
    /// `dynamic_prefix.user_tools` for this agent. Defaults to 30 when
    /// unset — generous enough to fit ~10 headlines × 3 active plugins
    /// without blowing a 64k-context small model's prompt budget.
    /// Override upward for larger models (e.g. 100 for doubao-pro)
    /// or downward for ultra-small ones. Excess tools fall back to
    /// `plugin_invoke` lookup at zero prompt cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_tools_cap: Option<usize>,
    /// Token budget for plugin tools in `dynamic_prefix.user_tools` (v2).
    /// When set, replaces the count-based `user_tools_cap`: selections are
    /// admitted in priority order (session inject/pin > config pin >
    /// headline/group) until their summed token estimate hits this budget;
    /// the overflow rides behind the `request_tool` stub instead of being
    /// silently dropped. A 500-token tool now costs what it costs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_tools_budget: Option<usize>,
    /// Pinned plugin tool GROUPS, entries `"<plugin>:<group>"` (v2
    /// toolGroups). Members of a pinned group are injected as real
    /// ToolDefs (budget permitting) even when not headline-tagged.
    /// Session-level analog: `request_tool("<plugin>:<group>")`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_groups: Option<Vec<String>>,
    /// Context window size in tokens. Used to calculate history budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    /// Maximum tokens for LLM response. Default: 2048.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Cheap/fast model chain for internal sub-tasks (query planning, intent
    /// classification, summarization). Falls back to `primary` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flash: Option<StringOrVec>,
    /// Vision model chain used by the GUI agent (`computer_use`) — must
    /// support image inputs. Resolution chain when vlm_drive / VlmDriver
    /// runs:
    ///   1. per-agent `model.vision` (chain — try each)
    ///   2. `agents.defaults.model.vision` (chain — try each)
    ///   3. per-agent `model.primary` head (assumes primary handles vision)
    ///   4. `agents.defaults.model.primary` head
    /// Vision-capability check (in priority order):
    ///   1. The provider's `models[]` entry — if `input` lists `image`, treat
    ///      as vision-capable. If `input` is declared but contains only `text`,
    ///      surface a clear error.
    ///   2. Otherwise the built-in `is_known_vision_model` allow-list.
    ///      Defaulting to text-only is intentional: vision-capable models are
    ///      still the minority, so an unknown model is more likely text-only
    ///      than vision. Misclassifications can be fixed by either adding
    ///      `input: ["text", "image"]` to the model's provider config or
    ///      extending the allow-list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<StringOrVec>,
}

impl ModelConfig {
    /// First model id in the `primary` chain (the most-preferred one), or
    /// `None` if unset or empty. Drop-in replacement for the previous
    /// `cfg.primary.as_deref()`.
    pub fn primary_head(&self) -> Option<&str> {
        self.primary.as_ref().and_then(StringOrVec::head)
    }

    /// Full primary chain as borrowed slice. Length 0 = unset.
    pub fn primary_chain(&self) -> Vec<&str> {
        self.primary
            .as_ref()
            .map(StringOrVec::chain)
            .unwrap_or_default()
    }

    /// First model id in the `flash` chain.
    pub fn flash_head(&self) -> Option<&str> {
        self.flash.as_ref().and_then(StringOrVec::head)
    }

    /// Full flash chain.
    pub fn flash_chain(&self) -> Vec<&str> {
        self.flash
            .as_ref()
            .map(StringOrVec::chain)
            .unwrap_or_default()
    }

    /// First model id in the `vision` chain.
    pub fn vision_head(&self) -> Option<&str> {
        self.vision.as_ref().and_then(StringOrVec::head)
    }

    /// Full vision chain.
    pub fn vision_chain(&self) -> Vec<&str> {
        self.vision
            .as_ref()
            .map(StringOrVec::chain)
            .unwrap_or_default()
    }

    /// First model id in the `image` chain.
    pub fn image_head(&self) -> Option<&str> {
        self.image.as_ref().and_then(StringOrVec::head)
    }

    /// Full image chain.
    pub fn image_chain(&self) -> Vec<&str> {
        self.image
            .as_ref()
            .map(StringOrVec::chain)
            .unwrap_or_default()
    }

    /// First model id in the `video` chain.
    pub fn video_head(&self) -> Option<&str> {
        self.video.as_ref().and_then(StringOrVec::head)
    }

    /// Full video chain.
    pub fn video_chain(&self) -> Vec<&str> {
        self.video
            .as_ref()
            .map(StringOrVec::chain)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Adaptive,
}

impl ThinkingLevel {
    /// Map a thinking level to a recommended budget_tokens value.
    pub fn budget_tokens(&self) -> u32 {
        match self {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 1024,
            ThinkingLevel::Low => 4096,
            ThinkingLevel::Medium => 10240,
            ThinkingLevel::High => 32768,
            ThinkingLevel::Xhigh => 65536,
            ThinkingLevel::Adaptive => 0, // let the model decide
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub enabled: Option<bool>,
    pub level: Option<ThinkingLevel>,
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAlias {
    pub model: Option<String>,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupChatConfig {
    pub enabled: Option<bool>,
    pub mention: Option<bool>,
    pub prefix: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PromptMode {
    Full,
    Minimal,
}

// ---------------------------------------------------------------------------
// contextPruning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPruningConfig {
    pub mode: Option<PruningMode>,
    pub ttl: Option<String>,
    pub keep_last_assistants: Option<u32>,
    pub min_prunable_tool_chars: Option<u32>,
    pub soft_trim: Option<SoftTrimConfig>,
    pub hard_clear: Option<HardClearConfig>,
    pub tools: Option<PruningToolPolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PruningMode {
    Off,
    #[serde(rename = "cache-ttl")]
    CacheTtl,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SoftTrimConfig {
    pub enabled: Option<bool>,
    pub head_chars: Option<u32>,
    pub tail_chars: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HardClearConfig {
    pub enabled: Option<bool>,
    pub threshold: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PruningToolPolicy {
    pub deny: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// compaction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionConfig {
    pub mode: Option<CompactionMode>,
    pub reserve_tokens_floor: Option<u32>,
    pub identifier_policy: Option<String>,
    pub memory_flush: Option<MemoryFlushConfig>,
    pub model: Option<String>,
    /// Number of recent user-assistant pairs to keep intact during layered
    /// compaction (default 5). Only older messages are summarised.
    pub keep_recent_pairs: Option<u32>,
    /// When true, extract key facts from compacted history and store them
    /// in long-term memory (default true).
    pub extract_facts: Option<bool>,
    /// Maximum token budget for the transcript text fed to the compact LLM.
    /// Higher values preserve more tool call/result detail but require a
    /// compact model with sufficient context window. Default: 16000 tokens.
    pub max_transcript_tokens: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum CompactionMode {
    Default,
    Safeguard,
    /// Keep last N turns verbatim, summarise only the older portion.
    Layered,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFlushConfig {
    pub enabled: Option<bool>,
    pub system_prompt: Option<String>,
    pub prompt: Option<String>,
}

// ---------------------------------------------------------------------------
// heartbeat
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatConfig {
    pub enabled: Option<bool>,
}

// ---------------------------------------------------------------------------
// sandbox (agent level)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSandboxConfig {
    pub enabled: Option<bool>,
    pub scope: Option<SandboxScope>,
    pub browser_force: Option<bool>,
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    pub mode: Option<ModelsMode>,
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ModelsMode {
    Merge,
    Replace,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<SecretOrString>,
    pub api: Option<ApiFormat>,
    pub models: Option<Vec<ModelDef>>,
    pub enabled: Option<bool>,
    /// Custom User-Agent for HTTP requests to this provider.
    /// Falls back to gateway.user_agent if not set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Override the wire `prefix_id` for `rsclaw` providers (protocol
    /// §2.1.1 / §2.10.1, namespaced `<ns>/<ver>`). Defaults to
    /// `provider::rsclaw::RSCLAW_DEFAULT_PREFIX_ID` (`rsclaw/2026.6.26`)
    /// when omitted. Ignored for non-rsclaw `api` formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_id: Option<String>,
    /// HTTP timeout (seconds) for the `rsclaw` `/sessions/<id>/compact`
    /// splice call. The server holds the per-session lock and dispatches
    /// the splice to a worker; a large tail can take a while, so this must
    /// be ≥ the server's splice ceiling or the client times out before the
    /// splice returns. Defaults to
    /// `provider::rsclaw::RSCLAW_DEFAULT_COMPACT_TIMEOUT_SECS` (180) when
    /// omitted. Ignored for non-rsclaw `api` formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_timeout_secs: Option<u64>,
    /// `rsclaw` providers only: send `options.constrain_tool_calls` on every
    /// turn so the worker constrains tool-call decoding with the lazy GBNF
    /// grammar it derived from the session's tools at create/replay time.
    /// Eliminates malformed tool-call JSON/XML at the decode stage. Requires
    /// workers that advertise `supports_constrained_tool_calls`; older
    /// workers ignore the option. Defaults to false (off) while the fleet
    /// rolls out. Ignored for non-rsclaw `api` formats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrain_tool_calls: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ApiFormat {
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-completions", alias = "openai")]
    OpenAiCompletions,
    Anthropic,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    Gemini,
    Ollama,
    /// rsclaw stateful incremental session protocol (kvCacheMode=2).
    /// Provider sends `POST /sessions` / `POST /sessions/<id>/turn` /
    /// `POST /sessions/replay`. The runtime auto-forces kv_cache_mode=2
    /// when the resolved provider is `rsclaw`, so model entries don't
    /// need their own `kvCacheMode` field. Set on a `models.providers.*`
    /// entry whose `baseUrl` points at either `rsclaw-server` (e.g.
    /// `https://api.rsclaw.ai/v1/agent`) or a self-hosted rsclaw-llm
    /// worker (e.g. `http://localhost:9999`).
    Rsclaw,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDef {
    pub id: String,
    pub name: Option<String>,
    pub reasoning: Option<bool>,
    pub input: Option<Vec<InputType>>,
    pub cost: Option<CostConfig>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InputType {
    Text,
    Image,
    Audio,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostConfig {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub unit: Option<u64>,
}

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    pub profiles: Option<HashMap<String, AuthProfile>>,
    pub order: Option<HashMap<String, Vec<String>>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum AuthProfile {
    #[serde(rename = "oauth")]
    OAuth { email: Option<String> },
    #[serde(rename = "api_key")]
    ApiKey {
        #[serde(rename = "apiKey")]
        api_key: Option<SecretOrString>,
        provider: Option<String>,
    },
    #[serde(rename = "token")]
    Token {
        token: Option<SecretOrString>,
        provider: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// channels
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
    pub discord: Option<DiscordConfig>,
    pub slack: Option<SlackConfig>,
    pub whatsapp: Option<WhatsAppConfig>,
    pub signal: Option<SignalConfig>,
    pub imessage: Option<IMessageConfig>,
    pub mattermost: Option<MattermostConfig>,
    pub msteams: Option<MSTeamsConfig>,
    pub googlechat: Option<GoogleChatConfig>,
    pub feishu: Option<FeishuConfig>,
    pub dingtalk: Option<DingTalkConfig>,
    pub wecom: Option<WeComConfig>,
    /// Personal WeChat via ilink (openclaw-weixin compatible)
    pub wechat: Option<WeChatPersonalConfig>,
    /// QQ Official Bot API
    pub qq: Option<QQBotConfig>,
    /// LINE Messaging API
    pub line: Option<LineConfig>,
    /// Zalo Official Account API
    pub zalo: Option<ZaloConfig>,
    /// Matrix (Element) via Client-Server API long-poll sync
    pub matrix: Option<MatrixConfig>,
    /// Custom channels defined by the user (webhook or websocket).
    pub custom: Option<Vec<CustomChannelConfig>>,
}

// --- Custom user-defined channels ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomChannelConfig {
    /// Channel name (used in session keys, routing).
    pub name: String,
    /// "webhook" or "websocket".
    #[serde(rename = "type")]
    pub channel_type: String,
    #[serde(flatten)]
    pub base: ChannelBase,

    // -- WebSocket mode --
    /// WebSocket URL to connect to.
    pub ws_url: Option<String>,
    /// Extra headers for WS connection (e.g. auth).
    pub ws_headers: Option<HashMap<String, String>>,
    /// Auth frame to send after WS connect (JSON string with ${VAR} expansion).
    pub auth_frame: Option<String>,
    /// JSON path in auth response to check for success.
    pub auth_success_path: Option<String>,
    /// Expected value at auth_success_path.
    pub auth_success_value: Option<String>,
    /// Heartbeat interval in seconds.
    pub heartbeat_interval: Option<u64>,
    /// Heartbeat frame (JSON string).
    pub heartbeat_frame: Option<String>,

    // -- Inbound message parsing (both modes) --
    /// JSON path to filter which frames/posts are messages (e.g. "$.type").
    pub filter_path: Option<String>,
    /// Expected value at filter_path (e.g. "message").
    pub filter_value: Option<String>,
    /// JSON path to extract message text.
    pub text_path: Option<String>,
    /// JSON path to extract sender ID.
    pub sender_path: Option<String>,
    /// JSON path to extract group/chat ID (if present = group message).
    pub group_path: Option<String>,
    /// JSON path to extract a single image URL from inbound payload.
    pub image_url_path: Option<String>,
    /// JSON path to extract an array of image URLs from inbound payload.
    pub image_urls_path: Option<String>,
    /// JSON path to extract a file download URL from inbound payload.
    pub file_url_path: Option<String>,
    /// JSON path to extract the filename for file attachments.
    pub file_name_path: Option<String>,

    // -- Outbound reply --
    /// For webhook: HTTP callback URL for replies.
    pub reply_url: Option<String>,
    /// HTTP method for reply (default: POST).
    pub reply_method: Option<String>,
    /// Reply body template with {{sender}}, {{chat_id}}, {{reply}},
    /// {{is_group}}, {{images}}, {{files}} placeholders.
    pub reply_template: Option<String>,
    /// Extra headers for reply HTTP call.
    pub reply_headers: Option<HashMap<String, String>>,
    /// For websocket: reply frame template (same placeholders).
    pub reply_frame: Option<String>,
}

// --- LINE Messaging API ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.channelAccessToken` instead")]
    pub channel_access_token: Option<SecretOrString>,
    pub channel_secret: Option<SecretOrString>,
    /// REST API base URL override (for testing). Defaults to https://api.line.me/v2/bot
    pub api_base: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Zalo Official Account API ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZaloConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.accessToken` instead")]
    pub access_token: Option<SecretOrString>,
    pub oa_secret: Option<SecretOrString>,
    /// REST API base URL override (for testing). Defaults to https://openapi.zalo.me/v3.0/oa
    pub api_base: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Matrix (Element) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatrixConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.homeserver` instead")]
    pub homeserver: Option<String>,
    #[deprecated(note = "use `accounts.<name>.accessToken` instead")]
    pub access_token: Option<SecretOrString>,
    #[deprecated(note = "use `accounts.<name>.userId` instead")]
    pub user_id: Option<String>,
    pub device_id: Option<String>,
    pub recovery_key: Option<SecretOrString>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- QQ Official Bot (QQ机器人开放平台) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QQBotConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.appId` instead")]
    pub app_id: Option<String>,
    #[deprecated(note = "use `accounts.<name>.appSecret` instead")]
    pub app_secret: Option<SecretOrString>,
    /// Use sandbox API endpoint (default: false).
    pub sandbox: Option<bool>,
    /// Intent bits to subscribe. Default covers group @bot + C2C + guild.
    pub intents: Option<u32>,
    /// REST API base URL override (for testing). Defaults to https://api.sgroup.qq.com
    pub api_base: Option<String>,
    /// Token endpoint URL override (for testing). Defaults to https://bots.qq.com/app/getAppAccessToken
    pub token_url: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Personal WeChat (个人微信 via ilink) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeChatPersonalConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    /// Saved bot_token from QR login (auto-populated after `channels login
    /// --channel wechat`)
    #[deprecated(note = "use `accounts.<name>.botToken` instead")]
    pub bot_token: Option<SecretOrString>,
    /// Override the ilink API base URL (default: https://ilinkai.weixin.qq.com).
    /// Useful for testing with a mock server.
    pub base_url: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

/// Fields shared by every channel.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelBase {
    pub enabled: Option<bool>,
    pub dm_policy: Option<DmPolicy>,
    pub allow_from: Option<Vec<String>>,
    pub group_policy: Option<GroupPolicy>,
    pub group_allow_from: Option<Vec<String>>,
    pub health_monitor: Option<HealthMonitorConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DmPolicy {
    Pairing,
    Allowlist,
    Open,
    Disabled,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GroupPolicy {
    Allowlist,
    Open,
    Disabled,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthMonitorConfig {
    pub enabled: Option<bool>,
    pub check_interval_min: Option<u32>,
}

// --- Telegram ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelegramConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.botToken` instead")]
    pub bot_token: Option<SecretOrString>,
    #[deprecated(note = "use `accounts.<name>.botToken` instead")]
    pub token_file: Option<String>,
    pub history_limit: Option<u32>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub link_preview: Option<bool>,
    pub streaming: Option<StreamingMode>,
    pub text_chunk_limit: Option<usize>,
    pub media_max_mb: Option<u32>,
    pub actions: Option<Value>,
    pub reaction_notifications: Option<ReactionNotif>,
    pub custom_commands: Option<Vec<BotCommand>>,
    pub groups: Option<HashMap<String, Value>>,
    pub proxy: Option<String>,
    pub webhook_url: Option<String>,
    pub webhook_secret: Option<SecretOrString>,
    pub webhook_path: Option<String>,
    pub network: Option<Value>,
    pub retry: Option<RetryConfig>,
    pub config_writes: Option<bool>,
    pub default_account: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
    pub api_base: Option<String>,
}

// --- Discord ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscordConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.token` instead")]
    pub token: Option<SecretOrString>,
    pub media_max_mb: Option<u32>,
    pub allow_bots: Option<bool>,
    pub streaming: Option<StreamingMode>,
    pub reply_to_mode: Option<ReplyToMode>,
    pub max_lines_per_message: Option<u32>,
    pub actions: Option<Value>,
    pub reaction_notifications: Option<ReactionNotif>,
    pub dm: Option<Value>,
    pub guilds: Option<HashMap<String, Value>>,
    pub accounts: Option<HashMap<String, Value>>,
    pub retry: Option<RetryConfig>,
    /// Gateway WebSocket URL override (for testing). Defaults to
    /// wss://gateway.discord.gg/?v=10&encoding=json
    pub gateway_url: Option<String>,
    /// HTTP API base URL override (for testing). Defaults to https://discord.com/api/v10
    pub api_base: Option<String>,
}

// --- Slack ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlackConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.botToken` instead")]
    pub bot_token: Option<SecretOrString>,
    #[deprecated(note = "use `accounts.<name>.appToken` instead")]
    pub app_token: Option<SecretOrString>,
    /// Override the Slack API base URL (default: https://slack.com/api).
    /// Useful for pointing at a mock server during testing.
    pub api_base: Option<String>,
    pub streaming: Option<StreamingMode>,
    pub native_streaming: Option<bool>,
    pub text_chunk_limit: Option<usize>,
    pub media_max_mb: Option<u32>,
    pub workspaces: Option<HashMap<String, Value>>,
    pub retry: Option<RetryConfig>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- WhatsApp ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WhatsAppConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub text_chunk_limit: Option<usize>,
    pub chunk_mode: Option<ChunkMode>,
    pub media_max_mb: Option<u32>,
    pub send_read_receipts: Option<bool>,
    pub groups: Option<HashMap<String, Value>>,
    pub default_account: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
    pub retry: Option<RetryConfig>,
    /// REST API base URL override (for testing). Defaults to https://graph.facebook.com/v19.0
    pub api_base: Option<String>,
}

// --- Signal ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.phone` instead")]
    pub phone: Option<String>,
    pub text_chunk_limit: Option<usize>,
    pub retry: Option<RetryConfig>,
    /// Path to signal-cli binary (default: "signal-cli"). Can point to a mock
    /// script.
    pub cli_path: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- iMessage ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IMessageConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub server_url: Option<String>,
    pub api_key: Option<SecretOrString>,
    pub retry: Option<RetryConfig>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Mattermost ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MattermostConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub server_url: Option<String>,
    pub bot_token: Option<SecretOrString>,
    pub retry: Option<RetryConfig>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- MS Teams ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MSTeamsConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub tenant_id: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<SecretOrString>,
    pub retry: Option<RetryConfig>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Google Chat ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleChatConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub service_account_key_file: Option<String>,
    pub retry: Option<RetryConfig>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Feishu (Lark) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeishuConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.appId` instead")]
    pub app_id: Option<String>,
    #[deprecated(note = "use `accounts.<name>.appSecret` instead")]
    pub app_secret: Option<SecretOrString>,
    pub verification_token: Option<SecretOrString>,
    pub encrypt_key: Option<SecretOrString>,
    pub streaming: Option<StreamingMode>,
    /// "feishu" (default, China) or "lark" (international)
    #[deprecated(note = "use `accounts.<name>.brand` instead")]
    pub brand: Option<String>,
    /// REST API base URL override (for testing). Defaults to https://open.feishu.cn/open-apis
    pub api_base: Option<String>,
    /// WS endpoint request domain override (for testing). Defaults to https://open.feishu.cn
    pub ws_url: Option<String>,
    /// Seconds to wait between WS reconnect attempts. Defaults to 5.
    pub reconnect_delay_secs: Option<u64>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- DingTalk ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    #[deprecated(note = "use `accounts.<name>.appKey` instead")]
    pub app_key: Option<String>,
    #[deprecated(note = "use `accounts.<name>.appSecret` instead")]
    pub app_secret: Option<SecretOrString>,
    #[deprecated(note = "use `accounts.<name>.robotCode` instead")]
    pub robot_code: Option<String>,
    pub streaming: Option<StreamingMode>,
    /// API base URL override (for testing). Defaults to https://api.dingtalk.com
    pub api_base: Option<String>,
    /// Old API base URL override. Defaults to https://oapi.dingtalk.com
    pub oapi_base: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- WeCom (企业微信) ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeComConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    /// Bot ID (企业微信后台显示)
    #[deprecated(note = "use `accounts.<name>.botId` instead")]
    pub bot_id: Option<String>,
    /// Bot secret (企业微信后台显示)
    #[deprecated(note = "use `accounts.<name>.secret` instead")]
    pub secret: Option<SecretOrString>,
    pub token: Option<SecretOrString>,
    pub encoding_aes_key: Option<SecretOrString>,
    pub streaming: Option<StreamingMode>,
    /// WebSocket URL override (defaults to wss://openws.work.weixin.qq.com)
    #[serde(alias = "wsUrl")]
    #[deprecated(note = "use `accounts.<name>.wsUrl` instead")]
    pub ws_url: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- Shared channel enums / structs ---

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReplyToMode {
    Off,
    First,
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StreamingMode {
    Off,
    Partial,
    Block,
    Progress,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReactionNotif {
    Off,
    Own,
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChunkMode {
    Length,
    Newline,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotCommand {
    pub command: String,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    pub attempts: Option<u32>,
    pub min_delay_ms: Option<u64>,
    pub max_delay_ms: Option<u64>,
    pub jitter: Option<f64>,
}

// ---------------------------------------------------------------------------
// session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    pub dm_scope: Option<DmScope>,
    pub thread_bindings: Option<Value>,
    pub reset: Option<SessionResetConfig>,
    pub identity_links: Option<HashMap<String, Vec<String>>>,
    pub maintenance: Option<SessionMaintenanceConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DmScope {
    Main,
    #[serde(rename = "per-peer")]
    PerPeer,
    #[serde(rename = "per-channel-peer")]
    PerChannelPeer,
    #[serde(rename = "per-account-channel-peer")]
    PerAccountChannelPeer,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResetConfig {
    pub command: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMaintenanceConfig {
    pub prune_after: Option<String>,
    pub max_entries: Option<u32>,
    pub max_disk_bytes: Option<u64>,
    pub rotate_bytes: Option<u64>,
    pub reset_archive_retention: Option<String>,
}

// ---------------------------------------------------------------------------
// bindings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingConfig {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub agent_id: String,
    #[serde(rename = "match")]
    pub match_: BindingMatch,
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingMatch {
    pub channel: Option<String>,
    pub peer_id: Option<String>,
    pub group_id: Option<String>,
    pub account_id: Option<String>,
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// cron
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronConfig {
    pub enabled: Option<bool>,
    pub max_concurrent_runs: Option<u32>,
    pub session_retention: Option<Value>,
    pub run_log: Option<RunLogConfig>,
    pub jobs: Option<Vec<CronJobConfig>>,
    /// Default delivery target for jobs without explicit delivery config.
    /// Jobs can override this by setting their own `delivery` field.
    pub default_delivery: Option<CronDelivery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogConfig {
    pub enabled: Option<bool>,
    pub max_runs: Option<u32>,
    pub retention: Option<String>,
}

/// Delivery target for cron job notifications (OpenClaw compat).
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CronDelivery {
    /// Delivery mode: "announce" sends to channel, "none" disables.
    #[serde(default)]
    pub mode: Option<String>,
    /// Channel to send to: "feishu", "wechat", "telegram", etc.
    pub channel: Option<String>,
    /// Target user/chat ID(s). Accepts a single id or a list (fan-out to
    /// multiple recipients). When omitted, the cron runner falls back to the
    /// peer of the agent's most-recent conversation (incl. a2a-delegated).
    #[serde(rename = "to")]
    pub to: Option<StringOrVec>,
    /// Thread/topic ID for channels that support it.
    pub thread_id: Option<String>,
    /// Account ID for multi-account setups.
    pub account_id: Option<String>,
    /// If true, silently ignore delivery failures.
    #[serde(default)]
    pub best_effort: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronJobConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub schedule: String,
    #[serde(default)]
    pub tz: Option<String>,
    pub agent_id: Option<String>,
    pub message: String,
    pub session: Option<Value>,
    pub enabled: Option<bool>,
    /// Delivery target for notifications when job completes.
    pub delivery: Option<CronDelivery>,
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsConfig {
    pub loop_detection: Option<LoopDetectionConfig>,
    pub deny: Option<Vec<String>>,
    pub allow: Option<Vec<String>>,
    pub exec: Option<ExecToolConfig>,
    pub web_search: Option<WebSearchConfig>,
    pub web_fetch: Option<WebFetchConfig>,
    pub web_browser: Option<WebBrowserConfig>,
    pub computer_use: Option<ComputerUseConfig>,
    pub upload: Option<UploadConfig>,
    /// Max chars to keep in session history per tool result.
    /// Prevents session bloat from large web_fetch/web_search results.
    pub session_result_limits: Option<SessionResultLimits>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResultLimits {
    /// web_search result max chars in session (default: 2000)
    pub web_search: Option<usize>,
    /// web_fetch result max chars in session (default: 5000)
    pub web_fetch: Option<usize>,
    /// web_browser snapshot/action max chars in session (default: 1500)
    /// Browser snapshots are large but mostly ephemeral (refs expire after page
    /// change).
    pub web_browser: Option<usize>,
    /// exec result max chars in session (default: 3000)
    pub exec: Option<usize>,
    /// Default for all other tools (default: 3000)
    pub default: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadConfig {
    /// Max file size before first confirmation (bytes, default 50MB)
    pub max_file_size: Option<usize>,
    /// Idle read timeout (seconds) for inbound resource downloads (e.g. Feishu
    /// file/video/voice). This is a read-idle timeout, not a total cap: a
    /// slow-but-progressing download is never killed; only a fully stalled
    /// connection fails after this many seconds without new bytes. Default 600.
    pub download_timeout_secs: Option<u64>,
    /// Max text chars before token confirmation (default 20000)
    pub max_text_chars: Option<usize>,
    /// Whether current model supports vision/images
    pub supports_vision: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecToolConfig {
    /// Enable exec safety rules (default: false for openclaw compat).
    pub safety: Option<bool>,
    /// Timeout for exec commands in seconds (default: 1800 = 30 minutes).
    /// Matches openclaw's defaultTimeoutSec.
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    /// Default search provider: "duckduckgo" | "google" | "bing" | "brave" |
    /// "serper"
    pub provider: Option<String>,
    /// API keys (alternative to env vars)
    pub brave_api_key: Option<SecretOrString>,
    pub google_api_key: Option<SecretOrString>,
    pub google_cx: Option<String>,
    pub bing_api_key: Option<SecretOrString>,
    /// Serper.dev API key for Google search results
    pub serper_api_key: Option<SecretOrString>,
    /// Max results per search (default: 5)
    pub max_results: Option<usize>,
    /// Disable web_search tool entirely
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    pub enabled: Option<bool>,
    /// Max content length in chars (default: 100000)
    pub max_length: Option<usize>,
    /// Custom User-Agent
    pub user_agent: Option<String>,
    /// Model for content summarization (e.g. "doubao-lite"). If set and
    /// the agent passes a `prompt`, fetched content is summarized by this
    /// model.
    pub summary_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBrowserConfig {
    pub enabled: Option<bool>,
    /// Path to Chrome/Chromium binary (auto-detect if not set)
    pub chrome_path: Option<String>,
    /// Run browser with visible window (default: false = headless)
    pub headed: Option<bool>,
    /// Chrome profile to use. "default" = system default profile,
    /// other string = named profile directory under
    /// ~/.rsclaw/browser-profiles/. If not set, uses a temporary profile
    /// (no cookies/history).
    pub profile: Option<String>,
    /// Ports to probe for existing Chrome with remote debugging (default:
    /// [9222, 9223]).
    pub remote_debug_ports: Option<Vec<u16>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseConfig {
    pub enabled: Option<bool>,
    /// Bypass the permission gate — every action auto-approved. Defaults
    /// to `false`. Used by power users / CI runs. `RedbPermissionStore`
    /// also exposes a runtime toggle (`set_bypass_all`) for the settings
    /// UI; this field is the value applied at gateway startup.
    pub bypass_all: Option<bool>,
    // Note: `ui_analyze_*` fields were removed when the standalone
    // ui_analyze tool was deleted. The new VlmDriver consumes the
    // screenshot directly via the configured vision model — no
    // separate element-detection endpoint is needed.
    // Vision model selection lives in `agents.<id>.model.vision` (see
    // `src/agent/runtime.rs::resolve_vision_model_name`).
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopDetectionConfig {
    pub enabled: Option<bool>,
    pub window: Option<usize>,
    pub threshold: Option<usize>,
    pub overrides: Option<std::collections::HashMap<String, usize>>,
}

// ---------------------------------------------------------------------------
// sandbox (top-level)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfig {
    pub mode: Option<SandboxMode>,
    pub scope: Option<SandboxScope>,
    pub docker: Option<DockerConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SandboxMode {
    Off,
    #[serde(rename = "non-main")]
    NonMain,
    All,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SandboxScope {
    Session,
    Agent,
    Shared,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerConfig {
    pub image: Option<String>,
    pub network: Option<String>,
    pub mounts: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// logging
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub format: Option<LogFormat>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LogFormat {
    Pretty,
    Json,
    Compact,
}

// ---------------------------------------------------------------------------
// skills
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsConfig {
    pub install: Option<SkillInstallConfig>,
    pub entries: Option<HashMap<String, SkillEntryConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInstallConfig {
    pub directory: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntryConfig {
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// ---------------------------------------------------------------------------
// skill_registries — per-registry credentials and overrides
// ---------------------------------------------------------------------------

/// Per-registry credential override. Both fields accept either a plain
/// string (`"sk-..."`), `${VAR}` env expansion, or a `SecretRef` object.
/// At gateway startup any resolved value is exported to the corresponding
/// env var name from `defaults.toml` (e.g. `IWENCAI_API_KEY`,
/// `IWENCAI_BASE_URL`) so spawned skill subprocesses inherit it without
/// the user touching their shell rc.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SkillRegistryEntry {
    pub api_key: Option<SecretOrString>,
    pub base_url: Option<SecretOrString>,
    pub enabled: Option<bool>,
}

// ---------------------------------------------------------------------------
// evolution — self-evolution thresholds for memory crystallization
// ---------------------------------------------------------------------------

/// Top-level evolution config block. All fields optional; missing values fall
/// back to `EvolutionConfig::default` (agent crate).
///
/// `preset = "test"` swaps the base to looser test-mode thresholds before
/// individual field overrides apply.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionConfig {
    pub enabled: Option<bool>,
    pub preset: Option<String>,
    pub cluster: Option<EvolutionClusterConfig>,
    pub promotion: Option<EvolutionPromotionConfig>,
    pub meditation: Option<EvolutionMeditationConfig>,
    /// Workflow-style crystallization: distill complex/hard turns into
    /// SKILL.md immediately rather than waiting for memory clusters to
    /// form. Off by default — opt in via `enabled: true`.
    pub workflow: Option<EvolutionWorkflowConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionClusterConfig {
    pub min_size: Option<usize>,
    pub similarity_threshold: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionPromotionConfig {
    pub access_only: Option<i64>,
    pub importance_only: Option<f32>,
    pub both_access: Option<i64>,
    pub both_importance: Option<f32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionMeditationConfig {
    pub max_per_cycle: Option<usize>,
    pub dedup_threshold: Option<f32>,
    pub crystallized_ttl_days: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct EvolutionWorkflowConfig {
    /// Master kill-switch. Off by default.
    pub enabled: Option<bool>,
    /// Difficulty score threshold for distillation [0.0, 1.0]. Higher =
    /// stricter.
    pub score_threshold: Option<f32>,
    /// Floor on tool calls — turns simpler than this never crystallize.
    pub min_tool_calls: Option<usize>,
    /// Floor on tool errors — set to 0 to allow error-free complex turns
    /// to crystallize anyway, or 1+ to require the agent had to recover
    /// from at least one failure (the "踩坑" signal).
    pub min_errors: Option<usize>,
    /// Hard cap on workflow distillations per hour to bound cost.
    pub max_per_hour: Option<usize>,
}

// ---------------------------------------------------------------------------
// plugins
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginsConfig {
    pub entries: Option<HashMap<String, PluginEntryConfig>>,
    pub slots: Option<PluginSlots>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntryConfig {
    pub enabled: Option<bool>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSlots {
    pub memory: Option<String>,
    pub context_engine: Option<String>,
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HooksConfig {
    pub enabled: bool,
    pub token: Option<SecretOrString>,
    pub path: Option<String>,
    pub default_session_key: Option<String>,
    pub allow_request_session_key: Option<bool>,
    pub allowed_session_key_prefixes: Option<Vec<String>>,
    pub mappings: Option<Vec<HookMapping>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookMapping {
    #[serde(rename = "match")]
    pub match_: HookMatch,
    pub action: HookAction,
    pub agent_id: Option<String>,
    pub session_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookMatch {
    pub path: Option<String>,
    pub method: Option<String>,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum HookAction {
    Agent,
    Script,
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsConfig {
    pub providers: HashMap<String, SecretProviderConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretProviderConfig {
    #[serde(rename = "type")]
    pub kind: SecretProviderKind,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SecretProviderKind {
    Env,
    File,
    Exec,
}

// ---------------------------------------------------------------------------
// memory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConfig {
    pub enabled: Option<bool>,
    pub backend: Option<MemoryBackend>,
    pub auto_capture: Option<bool>,
    pub auto_recall: Option<bool>,
    pub retrieval: Option<Value>,
    pub enable_management_tools: Option<bool>,
    pub scope: Option<ScopeConfig>,
    /// Number of results to retrieve per search backend (vector / BM25)
    /// before RRF fusion. Default 10 (was 5).
    pub recall_top_k: Option<usize>,
    /// Number of final results after RRF fusion. Default 5.
    pub recall_final_k: Option<usize>,
    /// Also search the knowledge base during auto-recall and append the top
    /// hits (with source titles for citation) to the turn-local recall
    /// context. Default true; only fires when the KB actually has indexed
    /// content. Set false to keep auto-recall memory-only (the explicit
    /// `knowledge_base` tool is unaffected).
    pub kb_auto_recall: Option<bool>,
    /// Deliver auto-recall (memory + KB + working plan) to NON-rsclaw
    /// providers too, by injecting it as turn-local text into the request
    /// copy of the user message (never persisted — session history stays
    /// clean). Default true. The cost on paid APIs is the recall budget
    /// (~1.8K tokens max per recall-bearing turn) plus losing prefix-cache
    /// reuse of the final exchange each turn. Set false to restore the old
    /// rsclaw-only behaviour.
    pub recall_external_providers: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MemoryBackend {
    Hnsw,
    None,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeConfig {
    pub default: Option<String>,
    pub definitions: Option<HashMap<String, Value>>,
    pub agent_access: Option<HashMap<String, Vec<String>>>,
}

// ---------------------------------------------------------------------------
// Shared primitives
// ---------------------------------------------------------------------------

/// A value that is either a plain string or a SecretRef object.
///
/// ```json5
/// // Plain string (not recommended for secrets)
/// { apiKey: "sk-..." }
///
/// // SecretRef
/// { apiKey: { source: "env", provider: "default", id: "OPENAI_API_KEY" } }
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum SecretOrString {
    Plain(String),
    Ref(SecretRef),
}

impl SecretOrString {
    /// Return the plain string value if this is not a SecretRef.
    /// For SecretRef variants, resolution happens in `SecretsManager`.
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            SecretOrString::Plain(s) => Some(s.as_str()),
            SecretOrString::Ref(_) => None,
        }
    }

    /// Resolve eagerly without a full `RuntimeConfig`.
    /// - `Plain` → returns the string as-is.
    /// - `Ref { source: Env, id }` → calls `std::env::var(id)`.
    /// - `Ref { source: File | Exec, .. }` → returns `None` (needs
    ///   `SecretsManager`).
    pub fn resolve_early(&self) -> Option<String> {
        match self {
            SecretOrString::Plain(s) => {
                // Support ${VAR} syntax in plain strings.
                let expanded = crate::loader::expand_env_vars(s);
                Some(expanded)
            }
            SecretOrString::Ref(r) if r.source == SecretSource::Env => std::env::var(&r.id).ok(),
            SecretOrString::Ref(_) => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub source: SecretSource,
    pub provider: Option<String>,
    pub id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SecretSource {
    Env,
    File,
    Exec,
}

// ---------------------------------------------------------------------------
// memorySearch (OpenClaw-compatible embedding configuration)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchConfig {
    /// Embedding provider: "openai", "gemini", "voyage", "ollama", "local"
    pub provider: Option<String>,
    /// Embedding model name, e.g. "text-embedding-3-small"
    pub model: Option<String>,
    /// Output vector dimension. Required for OpenAI-compatible models that
    /// `openai_model_dim` doesn't recognize (e.g. Qwen3-Embedding = 1024 on
    /// the GPU fleet). When unset, the dimension is inferred from the model
    /// name. Must match the model's actual output dim or the HNSW index will
    /// reject vectors.
    pub dimensions: Option<i32>,
    /// What to index: ["memory", "sessions"]
    pub sources: Option<Vec<String>>,
    /// Custom base URL for the embedding API
    pub base_url: Option<String>,
    /// API key (plain or SecretRef) for the embedding service
    pub api_key: Option<SecretOrString>,
    /// Local model settings
    pub local: Option<LocalEmbeddingConfig>,
    /// Optional query instruction for asymmetric embedding models (Qwen3-
    /// Embedding). When set, retrieval *queries* (not stored docs) are wrapped
    /// as `Instruct: {this}\nQuery: {q}`, which materially improves recall.
    /// Default unset = symmetric embedding (current behaviour).
    pub query_instruction: Option<String>,
    /// Experimental flags
    pub experimental: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalEmbeddingConfig {
    /// Path to a GGUF model file for local embedding
    pub model_path: Option<String>,
    /// Override download URL for the embedding model.
    /// Default: https://gitfast.org/tools/models/bge-small-zh-v1.5.zip
    pub model_download_url: Option<String>,
    /// Model repo name on HuggingFace (default: "BAAI/bge-small-zh-v1.5")
    pub model_repo: Option<String>,
}

// ---------------------------------------------------------------------------
// kb (Knowledge Base — RAG over local docs)
// ---------------------------------------------------------------------------

/// Embedding-provider configuration. KB's `kb.embed` and the memory subsystem's
/// `memorySearch` share this exact shape, so a KB-specific embedder is
/// configured identically to memory (local BGE or remote OpenAI-compatible).
pub type EmbedConfig = MemorySearchConfig;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbConfig {
    /// Per-KB embedder override. Same shape as `memorySearch` (local or
    /// remote). When unset, KB falls back to the shared `memorySearch` config,
    /// then to a local-model-dir scan, then to the deterministic stub. Set this
    /// to point KB at a different embedder than memory — e.g. KB on the remote
    /// GPU-fleet Qwen3 while memory stays on a fast local BGE. Changing it on a
    /// populated KB requires a reindex (KB does not auto-migrate like memory).
    pub embed: Option<EmbedConfig>,
    /// Optional cross-encoder rerank stage. When set, the search pipeline
    /// sends the fused top-N candidates to an OpenAI/Jina-compatible
    /// `/v1/rerank` endpoint (llama.cpp `--reranking`) and reorders by
    /// relevance before MMR. Best-effort: endpoint failures fall back to the
    /// fused order, never fail the search. Query-time only — no reindex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<KbRerankConfig>,
    /// Optional OCR stage for image / scanned-PDF ingestion. When set, an
    /// image source (png/jpeg/webp/…) is sent to an OCR endpoint
    /// (`/v1/ocr`) and the extracted text becomes the doc body. Without it,
    /// images aren't ingestable. base_url may be empty when `model` is a
    /// `rsclaw-*` name (defaults to the fleet API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr: Option<KbOcrConfig>,
    /// DEPRECATED (unused): superseded by `embed`. Kept only so existing
    /// configs that still set it continue to parse (deny_unknown_fields). No
    /// code reads it — embedder selection goes through `embed` /
    /// `memorySearch`.
    pub embedder: Option<String>,
    /// DEPRECATED (unused): superseded by `embed.local`. Kept for parse
    /// compatibility only; no code reads it.
    pub embedder_model_path: Option<String>,
    /// Persist the original raw bytes under `raw/<doc_id>.<ext>`
    /// (default true; users can disable to save disk).
    pub keep_raw: Option<bool>,
    /// Compactor grace period (seconds) before an orphan file is
    /// reaped. Default 3600 (1h).
    pub compactor_grace_secs: Option<i64>,
    /// Tombstone retention (days) before physical deletion.
    /// Default 30.
    pub tombstone_retention_days: Option<i64>,
    /// Max accepted document size (MB) for the `/api/v1/knowledge` upload
    /// endpoints (JSON body or multipart file). Default 50.
    pub max_doc_mb: Option<i64>,
    /// Allowed roots for the loopback-only `/docs/from-path` ingest endpoint.
    /// A path the gateway is asked to read off disk must canonicalize under
    /// one of these. Leading `~` expands to the user's home. Unset/empty →
    /// the default set (`~/Documents`, `~/Downloads`, `~/Desktop`). This is a
    /// defense-in-depth bound on an arbitrary-file-read surface; widen it only
    /// if you trust every local process that can reach the gateway.
    pub allowed_upload_roots: Option<Vec<String>>,
}

/// Cross-encoder rerank endpoint for the KB search pipeline (`kb.rerank`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KbRerankConfig {
    /// API root of an OpenAI/Jina-compatible rerank server, e.g.
    /// `http://117.50.139.244:5556/v1` (llama.cpp with `--reranking`).
    /// `/rerank` is appended at request time. Optional: when empty and the
    /// `model` is a `rsclaw-*` name, it resolves to the fleet API
    /// (`https://api.rsclaw.ai/v1`) — see `KbReranker::from_config`.
    #[serde(default)]
    pub base_url: String,
    /// Model name sent in the request body. Optional — single-model
    /// llama.cpp servers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Fused candidates sent to the reranker (and kept afterwards —
    /// everything below the window is dropped). Default 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<usize>,
    /// Kill-switch without deleting the block. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// OCR endpoint config (`kb.ocr`). Speaks the rsclaw-native `/v1/ocr`
/// shape (`{model, image, stream:false}` → `{content, …}`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KbOcrConfig {
    /// API root, e.g. `https://api.rsclaw.ai/v1`. May be empty when
    /// `model` is a `rsclaw-*` name (then defaults to the fleet API).
    /// `/ocr` is appended at request time.
    #[serde(default)]
    pub base_url: String,
    /// Model name (e.g. `rsclaw-ocr-v1`). Drives both the request body
    /// and the empty-base_url → fleet-API convention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// API key; falls back to the rsclaw provider key / env when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SecretOrString>,
    /// Optional language hint ("zh"/"en"/…); omitted → endpoint auto-detects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// Kill-switch without deleting the block. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

// ---------------------------------------------------------------------------
// memory (top-level, separate from memorySearch)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryTopConfig {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub search: Option<MemoryTopSearchConfig>,
    /// Per-backend recall count before RRF fusion (default 10).
    pub recall_top_k: Option<usize>,
    /// Final results after RRF fusion + reranking (default 5).
    pub recall_final_k: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryTopSearchConfig {
    pub model: Option<String>,
    pub max_results: Option<u32>,
}

// ---------------------------------------------------------------------------
// mcp (Model Context Protocol servers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    pub enabled: Option<bool>,
    pub servers: Option<Vec<McpServerConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Option<Vec<String>>,
    pub env: Option<HashMap<String, String>>,
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagesConfig {
    pub prefix: Option<String>,
    pub ack_reaction: Option<String>,
    pub ack_reaction_scope: Option<String>,
    pub inbound_debounce: Option<DebounceConfig>,
    pub compaction: Option<MsgCompactionConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebounceConfig {
    pub enabled: Option<bool>,
    pub window: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MsgCompactionConfig {
    pub enabled: Option<bool>,
    pub token_threshold: Option<u32>,
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandsConfig {
    pub enabled: Option<bool>,
    pub native: Option<String>,
    pub native_skills: Option<String>,
    pub restart: Option<bool>,
    pub owner_display: Option<String>,
    pub prefix: Option<String>,
    pub list: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// cli
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliConfig {
    pub banner: Option<bool>,
    pub backend: Option<String>,
}

// ---------------------------------------------------------------------------
// browser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserConfig {
    pub enable_cdp: Option<bool>,
    pub headless: Option<bool>,
    pub user_agent: Option<String>,
}

// ---------------------------------------------------------------------------
// ui
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfig {
    pub theme: Option<String>,
    pub assistant_name: Option<String>,
    pub assistant_emoji: Option<String>,
}

// ---------------------------------------------------------------------------
// approvals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalsConfig {
    pub exec: Option<ExecApprovalConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecApprovalConfig {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
    pub agent_filter: Option<Vec<String>>,
    pub session_filter: Option<Vec<String>>,
    pub targets: Option<Vec<Value>>,
}

// ---------------------------------------------------------------------------
// canvasHost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasHostConfig {
    pub enabled: Option<bool>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub root: Option<String>,
    pub live_reload: Option<bool>,
}

// ---------------------------------------------------------------------------
// talk (TTS)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TalkConfig {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f64>,
    pub instructions: Option<String>,
    pub api_key: Option<SecretOrString>,
}

// ---------------------------------------------------------------------------
// web (control UI settings)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebConfig {
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub tls_enabled: Option<bool>,
}
