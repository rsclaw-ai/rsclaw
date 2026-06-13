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
