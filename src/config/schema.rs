//! rsclaw JSON5 config schema — full field coverage.
//! Unknown fields cause deserialization to fail (deny_unknown_fields).

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
    pub hooks: Option<HooksConfig>,
    pub secrets: Option<SecretsConfig>,
    pub memory_search: Option<MemorySearchConfig>,
    pub memory: Option<MemoryTopConfig>,
    pub mcp: Option<McpConfig>,

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
    /// Seconds before sending "Processing..." indicator. 0 = disabled. Default:
    /// 10.
    pub processing_timeout: Option<u64>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAgentConfig {
    /// Logical agent ID (used as `agent_<id>` tool name).
    pub id: String,
    /// Base URL of the remote rsclaw/OpenClaw gateway, e.g. "http://host:18888".
    pub url: String,
    /// Optional bearer token for the remote gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Remote agent ID to target. If omitted, uses the remote gateway's default
    /// agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_agent_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentsConfig {
    pub defaults: Option<AgentDefaults>,
    pub list: Option<Vec<AgentEntry>>,
    pub external: Option<Vec<ExternalAgentConfig>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefaults {
    pub workspace: Option<String>,
    pub model: Option<ModelConfig>,
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
    pub timezone: Option<String>,
    pub timestamp: Option<Value>,
    pub thinking: Option<ThinkingConfig>,
    pub streaming: Option<StreamingMode>,
    pub timeout: Option<Value>,
    pub tools: Option<Value>,
    pub subagent: Option<Value>,
    pub cli: Option<Value>,
    pub media: Option<Value>,
    pub embedded: Option<Value>,
    pub archive: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub id: String,
    pub default: Option<bool>,
    pub name: Option<String>,
    pub workspace: Option<String>,
    pub model: Option<ModelConfig>,
    pub lane: Option<String>,
    pub lane_concurrency: Option<u32>,
    pub group_chat: Option<GroupChatConfig>,
    /// rsclaw extension: which channels this agent handles (None = all)
    pub channels: Option<Vec<String>>,
    /// Custom slash commands for this agent (in addition to built-in ones)
    pub commands: Option<Vec<AgentCommand>>,
    /// Allowed pre-parsed commands: "*" = all (default for main agent),
    /// pipe-separated list = specific (e.g. "/help|/search|/version"),
    /// empty/null = none (default for non-main agents).
    pub allowed_commands: Option<String>,
    /// rsclaw extension: use OpenCode ACP client instead of LLM
    /// When set, this agent spawns opencode acp subprocess and routes all
    /// prompts through it.
    pub opencode: Option<OpenCodeConfig>,
    /// OpenClaw-specific
    pub agent_dir: Option<String>,
    pub system: Option<String>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub primary: Option<String>,
    pub fallbacks: Option<Vec<String>>,
    pub image_fallbacks: Option<Vec<String>>,
    pub thinking: Option<ThinkingConfig>,
    /// Whether to send tool definitions to the model. Default: true.
    /// Set to false for reasoning models (deepseek-r1, qwen-r1) that don't
    /// support tools.
    pub tools_enabled: Option<bool>,
    /// Tool set level: "minimal" (7 core), "standard" (12, default), "full"
    /// (all 32+).
    pub toolset: Option<String>,
    /// Extra tool names to include on top of the toolset level.
    /// Also used as a whitelist when toolset is not set.
    pub tools: Option<Vec<String>>,
    /// Context window size in tokens. Used to calculate history budget.
    pub context_tokens: Option<u32>,
    /// Maximum tokens for LLM response. Default: 2048.
    pub max_tokens: Option<u32>,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    pub every: Option<String>,
    pub target: Option<HeartbeatTarget>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatTarget {
    pub channel: Option<String>,
    pub peer_id: Option<String>,
    pub group_id: Option<String>,
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
    /// Custom User-Agent header for HTTP requests to this provider (rsclaw
    /// extension). Does not affect OpenClaw config files to maintain
    /// compatibility. Can be overridden by RSCLAW_<PROVIDER>_USER_AGENT
    /// environment variable.
    #[serde(default, skip_deserializing)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ApiFormat {
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    Anthropic,
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    Gemini,
    Ollama,
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

    // -- Outbound reply --
    /// For webhook: HTTP callback URL for replies.
    pub reply_url: Option<String>,
    /// HTTP method for reply (default: POST).
    pub reply_method: Option<String>,
    /// Reply body template with {{sender}}, {{chat_id}}, {{reply}},
    /// {{is_group}} placeholders.
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
    pub homeserver: Option<String>,
    pub access_token: Option<SecretOrString>,
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
    pub app_id: Option<String>,
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
    pub bot_token: Option<SecretOrString>,
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
    pub bot_token: Option<SecretOrString>,
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
    pub app_id: Option<String>,
    pub app_secret: Option<SecretOrString>,
    pub verification_token: Option<SecretOrString>,
    pub encrypt_key: Option<SecretOrString>,
    pub streaming: Option<StreamingMode>,
    /// "feishu" (default, China) or "lark" (international)
    pub brand: Option<String>,
    /// REST API base URL override (for testing). Defaults to https://open.feishu.cn/open-apis
    pub api_base: Option<String>,
    /// WS endpoint request domain override (for testing). Defaults to https://open.feishu.cn
    pub ws_url: Option<String>,
    pub accounts: Option<HashMap<String, Value>>,
}

// --- DingTalk ---

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkConfig {
    #[serde(flatten)]
    pub base: ChannelBase,
    pub app_key: Option<String>,
    pub app_secret: Option<SecretOrString>,
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
    pub bot_id: Option<String>,
    /// Bot secret (企业微信后台显示)
    pub secret: Option<SecretOrString>,
    pub token: Option<SecretOrString>,
    pub encoding_aes_key: Option<SecretOrString>,
    pub streaming: Option<StreamingMode>,
    /// WebSocket URL override (defaults to wss://openws.work.weixin.qq.com)
    #[serde(alias = "wsUrl")]
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLogConfig {
    pub enabled: Option<bool>,
    pub max_runs: Option<u32>,
    pub retention: Option<String>,
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
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    /// Default search provider: "duckduckgo" | "google" | "bing" | "brave"
    pub provider: Option<String>,
    /// API keys (alternative to env vars)
    pub brave_api_key: Option<SecretOrString>,
    pub google_api_key: Option<SecretOrString>,
    pub google_cx: Option<String>,
    pub bing_api_key: Option<SecretOrString>,
    /// Max results per search (default: 5)
    pub max_results: Option<usize>,
    /// Disable web_search tool entirely
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    pub enabled: Option<bool>,
    /// Max content length in chars (default: 50000)
    pub max_length: Option<usize>,
    /// Custom User-Agent
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBrowserConfig {
    pub enabled: Option<bool>,
    /// Path to Chrome/Chromium binary (auto-detect if not set)
    pub chrome_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseConfig {
    pub enabled: Option<bool>,
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
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum MemoryBackend {
    LanceDb,
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
                let expanded = crate::config::loader::expand_env_vars(s);
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
    /// What to index: ["memory", "sessions"]
    pub sources: Option<Vec<String>>,
    /// Custom base URL for the embedding API
    pub base_url: Option<String>,
    /// API key (plain or SecretRef) for the embedding service
    pub api_key: Option<SecretOrString>,
    /// Local model settings
    pub local: Option<LocalEmbeddingConfig>,
    /// Experimental flags
    pub experimental: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalEmbeddingConfig {
    /// Path to a GGUF model file for local embedding
    pub model_path: Option<String>,
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
