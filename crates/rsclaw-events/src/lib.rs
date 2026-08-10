//! Shared event types — used by agent runtimes and the SSE broadcast layer.
//!
//! Defined here (not in `server`) to avoid a circular dependency:
//!   agent → events ← server
//!
//! TODO: Current limitations and improvement plan:
//! - AgentEvent is a flat struct; richer event types (tool calls, errors, usage
//!   updates) require either new structs or an enum-based approach.
//! - The broadcast channel drops events when subscribers lag; consider a
//!   bounded replay buffer or per-subscriber mpsc for guaranteed delivery.
//! - No event filtering: every subscriber receives every agent's events. Add
//!   topic-based or session-based filtering when load requires it.

use serde::{Deserialize, Serialize};

/// Why the gateway is asking the user to restart.
///
/// Multi-source: the config file watcher, the BGE auto-downloader, and any
/// future installer (plugin / model / migration) all publish into the same
/// `restart_request_tx` broadcast channel.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RestartReason {
    /// User edited `~/.rsclaw/rsclaw.json5` (file watcher trigger).
    /// `sections` is best-effort; empty if diff was not computed.
    ConfigChanged {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sections: Vec<String>,
    },
}

/// How urgently the gateway recommends restarting.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartUrgency {
    /// New behavior takes effect after restart, but the gateway works without
    /// it.
    Recommended,
    /// The gateway is already in a degraded state; restart is required to
    /// recover.
    Required,
}

/// What the UI should offer to make a pending config change take effect.
///
/// A full restart costs a cold boot (DB open, provider registry, first-run
/// KB/tantivy index — 30-60s on desktop) and drops every channel connection.
/// Most changes only need the owning component rebuilt, which
/// `POST /api/v1/reload?scope=…` does in-process in about a second. This
/// tells the banner which of the two to offer.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemedyKind {
    /// Scoped in-process reload — cheap, keeps the listener and channels up.
    Reload,
    /// Full process restart — the only way to rebind a socket or reopen the
    /// store.
    Restart,
}

/// Published by any source that wants the user to restart the gateway.
/// Latched in `AppState::pending_restart` so late-connecting UIs see it.
#[derive(Debug, Clone, Serialize)]
pub struct RestartRequest {
    /// Wall-clock time the request was generated, milliseconds since epoch.
    pub at_ms: u64,
    pub reason: RestartReason,
    pub urgency: RestartUrgency,
    /// Pre-translated, human-readable message for the banner.
    pub message: String,
    /// In-flight work count when this event was published. The frontend uses
    /// `0` to skip the auto-restart countdown and restart immediately. When
    /// `> 0`, `publish_restart` spawns a watcher that re-publishes (latch +
    /// broadcast) with `inflight = 0` once the gateway drains, so the UI
    /// short-circuits the countdown.
    #[serde(default)]
    pub inflight: u64,
    /// Whether a scoped reload suffices, or a full restart is unavoidable.
    /// Defaults to `Restart` so any publisher that has not been taught the
    /// distinction keeps the old, conservative behaviour.
    pub remedy: RemedyKind,
    /// `/api/v1/reload?scope=` values that apply the change when `remedy` is
    /// [`RemedyKind::Reload`]. Empty for restart-only requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reload_scopes: Vec<String>,
}

impl RestartRequest {
    /// Construct a new restart-required request stamped with `now`.
    /// `inflight` defaults to 0; `publish_restart` overwrites it with the
    /// live count before broadcast.
    pub fn new(reason: RestartReason, urgency: RestartUrgency, message: String) -> Self {
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            at_ms,
            reason,
            urgency,
            message,
            inflight: 0,
            remedy: RemedyKind::Restart,
            reload_scopes: Vec::new(),
        }
    }

    /// Construct a request that a scoped reload can satisfy.
    ///
    /// `scopes` are `/api/v1/reload?scope=` values; the UI posts them instead
    /// of restarting the process.
    pub fn new_reload(reason: RestartReason, message: String, scopes: Vec<String>) -> Self {
        Self {
            remedy: RemedyKind::Reload,
            reload_scopes: scopes,
            // A reload is cheap and non-disruptive to the listener, so it
            // never escalates past "recommended".
            urgency: RestartUrgency::Recommended,
            ..Self::new(reason, RestartUrgency::Recommended, message)
        }
    }
}

/// One option in an `AskUserPrompt`.
///
/// Wire format is camelCase (`label`, `description`) — UI consumes via WS
/// relays in `ws::handshake` / `ws::methods::sessions`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Structured multi-choice question emitted by the `ask_user` tool.
///
/// Channels that support interactive UI (Desktop / Telegram / Feishu / etc.)
/// can render this as buttons or a modal. Channels that don't (WeChat, Signal)
/// receive the agent's plain-text reply which already lists numbered options.
///
/// Wire format is camelCase (`multiSelect`, `recommendedIndex`) per project
/// convention — Rust fields stay snake_case, serde renames at the boundary.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AskUserPrompt {
    /// The full question.
    pub question: String,
    /// 2-8 distinct choices.
    pub options: Vec<AskUserOption>,
    /// True if multiple selections are allowed.
    #[serde(default)]
    pub multi_select: bool,
    /// 0-based index of the option the agent recommends (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_index: Option<usize>,
    /// Optional short tag (e.g. "Library", "Approach").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// Which logical channel a streamed text delta belongs to. Mirrors
/// `cap_rs::core::TextChannel` so rsclaw can preserve the distinction
/// when a cap driver (claudecode / opencode / codex / openclaude)
/// emits reasoning tokens separately from visible assistant text.
///
/// Why this exists: codex (GPT-5 reasoning) spends most of a turn in
/// the `Thought` channel — the user sees nothing for 30-60s, then a
/// burst of visible reply. Without forwarding Thought through the bus,
/// IM chunkers and the desktop UI have no way to render reasoning
/// progress. With it, the UI can choose to render reasoning inline
/// (greyed out, collapsed) or hide it entirely; IM chunkers can
/// prefix thought lines with `💭 ` so users see codex thinking in
/// real time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextChannel {
    /// Visible reply text — what the user expects in the chat bubble.
    /// Default for back-compat with pre-existing AgentEvent producers
    /// (every existing rsclaw call site emits assistant text).
    #[default]
    Assistant,
    /// Reasoning / planning tokens. Distinct stream, lower display
    /// priority. UIs SHOULD render but MAY hide based on user prefs.
    Thought,
    /// System notices (e.g. "cap driver started", environment info).
    /// Rare; surfaced primarily for debugging. UIs MAY drop entirely.
    System,
}

/// Emitted by `AgentRuntime` and broadcast to SSE subscribers via the
/// `AppState::event_bus` channel.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentEvent {
    pub session_id: String,
    pub agent_id: String,
    /// Incremental text delta.  Empty when `done = true`.
    pub delta: String,
    /// `true` on the final "turn complete" event.
    pub done: bool,
    /// File attachments produced this turn: (filename, mime_type,
    /// local_path_or_url). Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<(String, String, String)>,
    /// Image attachments (base64 data URIs or local paths).
    /// Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Tool call log for this turn: (name, args_json, output_text).
    /// Non-empty only on the final `done = true` event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_log: Vec<(String, String, String)>,
    /// Structured multi-choice question from the `ask_user` tool. Present
    /// only on the event that carries the ask. Capable channels render
    /// this as native UI; others rely on the agent's text reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<AskUserPrompt>,
    /// Which text channel this delta is on. `None` = legacy producer
    /// that pre-dates this field, treat as Assistant. `Some(Thought)`
    /// = reasoning tokens from a cap driver (codex). UIs and IM
    /// chunkers decide rendering per channel.
    ///
    /// `Option<…>` instead of `TextChannel` with a default to keep the
    /// blast radius of this field tiny: existing AgentEvent construction
    /// sites scattered across runtime.rs / startup.rs / tools_misc.rs
    /// don't need to be updated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<TextChannel>,
}
