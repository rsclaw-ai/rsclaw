//! `rsclaw-types` — stable, cross-crate DTOs with no first-party dependencies.
//!
//! These types were historically embedded inside `agent`, `channel`, and
//! `gateway`. They are lifted here so lower crates (config, provider, store,
//! kb, channel) can depend on them without depending on the runtime knot.
//!
//! Policy: append-only / frozen. A type belongs here only if it has been
//! stable for months AND is referenced across crate boundaries. Anything with
//! behaviour or non-trivial deps (e.g. provider wire DTOs) stays in its
//! domain crate.

/// The four kinds of agent in the system.
///
/// (Distinct from `cap::AgentKind`, which enumerates external CLI drivers like
/// Claudecode/Codex — same name, different domain, different crate.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AgentKind {
    /// The default entry point. Cannot be deleted. `default: true` in config.
    Main,
    /// User-created persistent agent. Saved to config file, survives restarts.
    Named,
    /// LLM-spawned temporary agent (`persistent: false`). Lives in memory, gone
    /// on restart.
    Sub,
    /// One-shot task agent. Automatically destroyed after completion.
    Task,
}

/// An image attachment sent by the user.
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    /// Base64-encoded data URI or URL.
    pub data: String,
    /// MIME type (e.g. "image/png", "image/jpeg").
    pub mime_type: String,
    /// Best-effort original on-disk source path, when the image came from a
    /// path-referencing client (e.g. `[file:/abs/path]` from desktop, or a
    /// channel that downloaded to a known cache location). `None` for
    /// inline-only attachments (e.g. pasted base64).
    ///
    /// Surfaced in vision-failure fallback messages so the user can re-attach
    /// or the agent can retry with the same file via a different tool.
    pub source_path: Option<String>,
}

/// A file attachment sent by the user (raw bytes, not yet processed).
#[derive(Debug, Clone)]
pub struct FileAttachment {
    pub filename: String,
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// A reply message ready to be sent to a channel.
#[derive(Debug, Clone, Default)]
pub struct OutboundMessage {
    /// Destination peer/group ID.
    pub target_id: String,
    /// Whether `target_id` is a group.
    pub is_group: bool,
    /// Text content.
    pub text: String,
    /// Optional reply-to message ID (platform-specific).
    pub reply_to: Option<String>,
    /// Image attachments (base64 data URIs).
    pub images: Vec<String>,
    /// File attachments: Vec<(filename, mime_type, file_path_or_url)>.
    /// Supported by channels that can send files (feishu, telegram, etc.).
    pub files: Vec<(String, String, String)>,
    /// Channel name to use for sending (e.g., "feishu", "telegram").
    /// Used by background tasks (opencode, claudecode) to route notifications.
    pub channel: Option<String>,
    /// Multi-account routing tag — which account in this channel received
    /// the inbound message that produced this reply. Set by inbound parsers
    /// that support multiple credentials (e.g. feishu accounts.<name>) so
    /// the outbound dispatcher can send via the same account's API token.
    /// `None` = single-account / bare `{channel}` lookup.
    pub account: Option<String>,
}

/// Names of all built-in agent tools. Lifted from agent/prompt_builder.rs
/// (crate-split) so rsclaw-provider can reference it without depending on the
/// runtime knot. Re-exported at crate::agent::prompt_builder::BUILTIN_TOOL_NAMES.
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "memory",
    "todo",
    "skill_use",
    "task",
    "task_finish",
    "read_file",
    "write_file",
    "edit_file",
    "send_file",
    "shell",
    "agent",
    "ask_user",
    "install_tool",
    "list_dir",
    "search_file",
    "search_content",
    "web_search",
    "web_fetch",
    "web_download",
    "web_browser",
    "computer_use",
    "image_gen",
    "video_gen",
    "pdf",
    "text_to_voice",
    "send_message",
    "cron",
    "session",
    "gateway",
    "cap",
    "cap_live",
    "cap_live_end",
    "cap_bind_sticky",
    "cap_unbind_sticky",
    "channel",
    "anycli",
    "clarify",
    "pairing",
    "create_docx",
    "create_pdf",
    "create_xlsx",
    "create_pptx",
    "doc",
    // Context-recovery tools: static, byte-identical for every client of
    // this prefix version, so they belong in the cacheable builtin prefix.
    // Previously misclassified as user_tools, which under prefix_id mode
    // (dynamic_prefix omitted) meant they were NEVER sent to the model —
    // so the model could never call read_session_archive despite the
    // summary/system-prompt telling it to.
    "read_session_archive",
    "read_artifact",
    "knowledge_base",
    // A2A-only tool, but unconditionally registered in build_tool_list and
    // byte-identical across clients — same builtin class. Errors gracefully
    // if called on a non-A2A turn.
    "wait_input",
    // Self-service skill management (discover → install → use → remove).
    "skill_list",
    "skill_search",
    "skill_install",
    "skill_remove",
    // Plugin meta tools (the 4 dispatchers): byte-identical for every client
    // of this version, so they belong in the cacheable builtin prefix. Live
    // per-plugin tool schemas are NOT registered here — they're rendered as
    // text into user_system (KV-cache friendly) by `render_active_plugin_tools_text`.
    "plugin_list",
    "plugin_search",
    "plugin_describe",
    "plugin_invoke",
];
