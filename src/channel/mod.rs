//! Channel subsystem.
//!
//! A *channel* is an integration point where messages arrive (Telegram,
//! Discord, CLI, etc.). Each channel implementation:
//!   1. Receives inbound messages and forwards them to the gateway router.
//!   2. Sends outbound replies (with optional chunking / preview streaming).
//!   3. Enforces `dmPolicy` (pairing / allowlist / open / disabled).
//!
//! Modules:
//!   - `mod`      — `Channel` trait, `DmPolicyEnforcer`, `PairingStore`
//!   - `chunker`  — text chunking with code-fence protection
//!   - `telegram` — Telegram Bot API client
//!   - `discord`  — Discord Bot WebSocket + REST
//!   - `slack`    — Slack Socket Mode + Web API
//!   - `whatsapp` — WhatsApp Cloud API (webhook)
//!   - `signal`   — Signal via signal-cli JSON-RPC
//!   - `dingtalk` — DingTalk Robot Stream Mode + REST
//!   - `line`     — LINE Messaging API (webhook)
//!   - `zalo`     — Zalo Official Account API (webhook)
//!   - `matrix`   — Matrix Client-Server API (long-poll sync)
//!   - `cli`      — CLI interactive channel

pub mod auth;
pub mod chunker;
pub mod cli;
pub mod custom;
pub mod desktop;
pub mod dingtalk;
pub mod discord;
pub mod feishu;
pub mod qq;
pub mod signal;
pub mod slack;
pub mod telegram;
pub mod transcription;
pub mod tts;
pub mod wechat;
pub mod wecom;
pub mod whatsapp;
pub mod line;
pub mod matrix;
pub mod zalo;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::future::BoxFuture;
use rand::Rng;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::{
    config::schema::DmPolicy,
    provider::{RetryConfig, backoff_delay},
};

// ---------------------------------------------------------------------------
// OutboundMessage
// ---------------------------------------------------------------------------

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
}

// ---------------------------------------------------------------------------
// Channel trait
// ---------------------------------------------------------------------------

// BoxFuture is required here because this trait is used as `dyn Channel`
// (see ChannelManager, gateway/channels). Native async fn in traits
// does not support dynamic dispatch.
/// Every channel integration implements this trait.
pub trait Channel: Send + Sync {
    /// Human-readable name of this channel, e.g. "telegram", "discord".
    fn name(&self) -> &str;

    /// Send a message to the channel.
    fn send(&self, msg: OutboundMessage) -> BoxFuture<'_, Result<()>>;

    /// Start the inbound message loop (long-running task).
    fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>>;
}

// ---------------------------------------------------------------------------
// DmPolicy enforcer
// ---------------------------------------------------------------------------

/// Maximum number of pending pairing requests per channel.
const MAX_PENDING_PAIRINGS: usize = 3;
/// How long a pairing code is valid.
const PAIRING_TTL: Duration = Duration::from_secs(3600);

/// Pairing code entry.
#[derive(Debug, Clone)]
struct PairingEntry {
    code: String,
    peer_id: String,
    created_at: Instant,
}

/// In-memory store for dmPolicy = "pairing" state.
#[derive(Debug, Default)]
pub struct PairingStore {
    approved: HashSet<String>,
    pending: Vec<PairingEntry>,
}

impl PairingStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_approved(&self, peer_id: &str) -> bool {
        self.approved.contains(peer_id)
    }

    /// Create a pairing code for `peer_id`. Returns `None` when queue is full.
    pub fn create_pairing(&mut self, peer_id: &str) -> Option<String> {
        self.pending
            .retain(|e| e.created_at.elapsed() < PAIRING_TTL);

        // Reuse existing code for the same peer.
        if let Some(existing) = self.pending.iter().find(|e| e.peer_id == peer_id) {
            return Some(existing.code.clone());
        }

        if self.pending.len() >= MAX_PENDING_PAIRINGS {
            return None;
        }

        let code = generate_pairing_code();
        self.pending.push(PairingEntry {
            code: code.clone(),
            peer_id: peer_id.to_owned(),
            created_at: Instant::now(),
        });
        Some(code)
    }

    /// Approve a pairing code. Returns the peer ID on success.
    pub fn approve(&mut self, code: &str) -> Option<String> {
        self.pending
            .retain(|e| e.created_at.elapsed() < PAIRING_TTL);
        let code_upper = code.to_uppercase();
        let pos = self
            .pending
            .iter()
            .position(|e| e.code.to_uppercase() == code_upper)?;
        let entry = self.pending.remove(pos);
        self.approved.insert(entry.peer_id.clone());
        Some(entry.peer_id)
    }

    pub fn revoke(&mut self, peer_id: &str) {
        self.approved.remove(peer_id);
    }

    /// List pending pairing requests (not yet approved). Returns (code, peer_id, seconds_remaining).
    pub fn list_pending(&mut self) -> Vec<(String, String, u64)> {
        self.pending.retain(|e| e.created_at.elapsed() < PAIRING_TTL);
        self.pending
            .iter()
            .map(|e| {
                let remaining = PAIRING_TTL.as_secs().saturating_sub(e.created_at.elapsed().as_secs());
                (e.code.clone(), e.peer_id.clone(), remaining)
            })
            .collect()
    }

    /// List approved peer IDs.
    pub fn list_approved(&self) -> Vec<String> {
        self.approved.iter().cloned().collect()
    }
}

fn generate_pairing_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"; // no 0/O/1/I to avoid confusion
    let mut rng = rand::rng();
    let part = |rng: &mut rand::rngs::ThreadRng| -> String {
        (0..4).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
    };
    format!("{}-{}", part(&mut rng), part(&mut rng))
}

// ---------------------------------------------------------------------------
// DmPolicyEnforcer
// ---------------------------------------------------------------------------

/// Evaluates the configured `dmPolicy` for an inbound DM.
/// Approved peers are persisted to redb so they survive restarts.
#[derive(Debug)]
pub struct DmPolicyEnforcer {
    policy: DmPolicy,
    allow_from: HashSet<String>,
    pairing: Mutex<PairingStore>,
    channel_name: String,
    store: Option<Arc<crate::store::redb_store::RedbStore>>,
}

/// Outcome of a dmPolicy check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyResult {
    Allow,
    Deny,
    SendPairingCode(String),
    PairingQueueFull,
}

impl DmPolicyEnforcer {
    pub fn new(policy: DmPolicy, allow_from: Vec<String>) -> Self {
        Self {
            policy,
            allow_from: allow_from.into_iter().collect(),
            pairing: Mutex::new(PairingStore::new()),
            channel_name: String::new(),
            store: None,
        }
    }

    /// Enable persistence: load approved peers from redb on init, write-through on approve/revoke.
    pub fn with_persistence(mut self, channel: &str, store: Arc<crate::store::redb_store::RedbStore>) -> Self {
        self.channel_name = channel.to_owned();
        // Load previously approved peers from redb.
        if let Ok(pairs) = store.list_pairings(channel) {
            let mut ps = self.pairing.try_lock().expect("lock during init");
            for peer_id in pairs {
                ps.approved.insert(peer_id);
            }
            if !ps.approved.is_empty() {
                info!(channel, count = ps.approved.len(), "loaded persisted pairing approvals");
            }
        }
        self.store = Some(store);
        self
    }

    pub async fn check(&self, peer_id: &str) -> PolicyResult {
        match &self.policy {
            DmPolicy::Disabled => {
                debug!(peer_id, "DM rejected: policy=disabled");
                PolicyResult::Deny
            }
            DmPolicy::Open => PolicyResult::Allow,
            DmPolicy::Allowlist => {
                if self.allow_from.contains(peer_id) || self.allow_from.contains("*") {
                    PolicyResult::Allow
                } else {
                    debug!(peer_id, "DM rejected: not in allowlist");
                    PolicyResult::Deny
                }
            }
            DmPolicy::Pairing => {
                let mut store = self.pairing.lock().await;
                if store.is_approved(peer_id) {
                    PolicyResult::Allow
                } else {
                    match store.create_pairing(peer_id) {
                        Some(code) => {
                            info!(peer_id, code, "pairing code generated");
                            PolicyResult::SendPairingCode(code)
                        }
                        None => {
                            warn!(peer_id, "pairing queue full");
                            PolicyResult::PairingQueueFull
                        }
                    }
                }
            }
        }
    }

    pub async fn approve_pairing(&self, code: &str) -> Option<String> {
        let peer = self.pairing.lock().await.approve(code);
        // Persist to redb.
        if let (Some(peer_id), Some(db)) = (&peer, &self.store) {
            let state = crate::store::redb_store::PairingState::Approved;
            if let Err(e) = db.put_pairing(&self.channel_name, peer_id, &state) {
                warn!(channel = %self.channel_name, peer_id, error = %e, "failed to persist pairing approval");
            }
        }
        peer
    }

    pub async fn revoke(&self, peer_id: &str) {
        self.pairing.lock().await.revoke(peer_id);
        // Remove from redb.
        if let Some(ref db) = self.store {
            if let Err(e) = db.delete_pairing(&self.channel_name, peer_id) {
                warn!(channel = %self.channel_name, peer_id, error = %e, "failed to delete pairing from store");
            }
        }
    }

    /// List pending pairing requests for this channel.
    pub async fn list_pending(&self) -> Vec<(String, String, u64)> {
        self.pairing.lock().await.list_pending()
    }

    /// List approved peers for this channel.
    pub async fn list_approved(&self) -> Vec<String> {
        self.pairing.lock().await.list_approved()
    }

    /// Get the channel name.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }
}

// ---------------------------------------------------------------------------
// Media type detection — shared by all channels
// ---------------------------------------------------------------------------

/// Detect if an attachment is an image based on content_type and filename.
pub fn is_image_attachment(content_type: &str, filename: &str) -> bool {
    if content_type.starts_with("image/") {
        return true;
    }
    let lower = filename.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
        || lower.ends_with(".svg")
        || lower.ends_with(".tiff")
        || lower.ends_with(".ico")
        || lower.ends_with(".heic")
        || lower.ends_with(".heif")
        || lower.ends_with(".avif")
}

/// Detect if an attachment is audio/voice based on content_type and filename.
pub fn is_audio_attachment(content_type: &str, filename: &str) -> bool {
    if content_type.starts_with("audio/") || content_type == "voice" {
        return true;
    }
    let lower = filename.to_lowercase();
    lower.ends_with(".amr")
        || lower.ends_with(".ogg")
        || lower.ends_with(".opus")
        || lower.ends_with(".silk")
        || lower.ends_with(".wav")
        || lower.ends_with(".mp3")
        || lower.ends_with(".m4a")
        || lower.ends_with(".aac")
        || lower.ends_with(".flac")
        || lower.ends_with(".wma")
}

/// Detect if an attachment is video based on content_type and filename.
pub fn is_video_attachment(content_type: &str, filename: &str) -> bool {
    if content_type.starts_with("video/") {
        return true;
    }
    let lower = filename.to_lowercase();
    lower.ends_with(".mp4")
        || lower.ends_with(".mov")
        || lower.ends_with(".avi")
        || lower.ends_with(".mkv")
        || lower.ends_with(".webm")
        || lower.ends_with(".wmv")
        || lower.ends_with(".flv")
        || lower.ends_with(".3gp")
}

// ---------------------------------------------------------------------------
// ChannelManager — concurrent channel limit (AGENTS.md §18)
// ---------------------------------------------------------------------------

use crate::MemoryTier;

pub struct ChannelManager {
    channels: HashMap<String, Arc<dyn Channel>>,
    tier: MemoryTier,
}

impl ChannelManager {
    pub fn new(tier: MemoryTier) -> Self {
        Self {
            channels: HashMap::new(),
            tier,
        }
    }

    pub fn max_concurrent(&self) -> usize {
        match self.tier {
            MemoryTier::Low => 3,
            MemoryTier::Standard => 8,
            MemoryTier::High => usize::MAX,
        }
    }

    pub fn register(&mut self, ch: Arc<dyn Channel>) -> Result<()> {
        if self.channels.len() >= self.max_concurrent() {
            anyhow::bail!(
                "channel limit reached ({}) for memory tier {:?}",
                self.max_concurrent(),
                self.tier
            );
        }
        self.channels.insert(ch.name().to_owned(), ch);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Channel>> {
        self.channels.get(name).cloned()
    }
}

// ---------------------------------------------------------------------------
// send_with_retry — agents.md §22
// ---------------------------------------------------------------------------

/// Send `msg` via `channel`, retrying up to `config.attempts` times with
/// exponential back-off on transient failures.
pub async fn send_with_retry(
    channel: &dyn Channel,
    msg: OutboundMessage,
    config: &RetryConfig,
) -> Result<()> {
    let mut last_err = anyhow::anyhow!("no attempts made");
    for attempt in 0..config.attempts {
        match channel.send(msg.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = e;
                if attempt + 1 < config.attempts {
                    let delay = backoff_delay(attempt, config);
                    warn!(
                        channel = channel.name(),
                        attempt,
                        ?delay,
                        "channel send failed, retrying"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    Err(last_err)
}

// ---------------------------------------------------------------------------
// Office document text extraction (docx/xlsx/pptx)
// ---------------------------------------------------------------------------

/// Extract text from an Office document (docx/xlsx/pptx) based on filename.
/// Returns None if the file is not a recognized Office format or extraction
/// fails.
pub fn extract_office_text(filename: &str, bytes: &[u8]) -> Option<String> {
    let lower = filename.to_lowercase();
    if lower.ends_with(".docx") {
        return extract_docx_text(bytes);
    }
    if lower.ends_with(".xlsx") {
        return extract_xlsx_text(bytes);
    }
    if lower.ends_with(".pptx") {
        return extract_pptx_text(bytes);
    }
    None
}

/// Extract text from a .docx file (ZIP containing word/document.xml).
fn extract_docx_text(bytes: &[u8]) -> Option<String> {
    use std::io::Cursor;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut doc = archive.by_name("word/document.xml").ok()?;
    let mut xml = String::new();
    std::io::Read::read_to_string(&mut doc, &mut xml).ok()?;
    // Strip XML tags to get plain text
    let text = regex::Regex::new(r"<[^>]+>")
        .ok()?
        .replace_all(&xml, " ")
        .to_string();
    // Clean up whitespace
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(text)
}

/// Extract text from a .xlsx file (ZIP containing xl/sharedStrings.xml).
fn extract_xlsx_text(bytes: &[u8]) -> Option<String> {
    use std::io::Cursor;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut text = String::new();

    // Try shared strings first (contains all text values)
    if let Ok(mut ss) = archive.by_name("xl/sharedStrings.xml") {
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut ss, &mut xml).ok()?;
        let clean = regex::Regex::new(r"<[^>]+>")
            .ok()?
            .replace_all(&xml, " ");
        text.push_str(&clean);
    }

    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(text)
}

/// Extract text from a .pptx file (ZIP containing ppt/slides/slide*.xml).
fn extract_pptx_text(bytes: &[u8]) -> Option<String> {
    use std::io::Cursor;
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).ok()?;
    let mut text = String::new();

    for i in 0..archive.len() {
        if let Ok(mut file) = archive.by_index(i) {
            let name = file.name().to_owned();
            if name.starts_with("ppt/slides/slide") && name.ends_with(".xml") {
                let mut xml = String::new();
                let _ = std::io::Read::read_to_string(&mut file, &mut xml);
                if let Ok(re) = regex::Regex::new(r"<[^>]+>") {
                    let clean = re.replace_all(&xml, " ").to_string();
                    text.push_str(&clean);
                    text.push('\n');
                }
            }
        }
    }

    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(text)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pairing_policy_generates_code() {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);
        let result = enforcer.check("user_1").await;
        assert!(
            matches!(result, PolicyResult::SendPairingCode(_)),
            "expected pairing code, got {result:?}"
        );
    }

    #[tokio::test]
    async fn pairing_approved_allows_subsequent() {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);
        let code = match enforcer.check("user_1").await {
            PolicyResult::SendPairingCode(c) => c,
            other => panic!("expected code, got {other:?}"),
        };
        let approved = enforcer.approve_pairing(&code).await;
        assert_eq!(approved.as_deref(), Some("user_1"));
        assert_eq!(enforcer.check("user_1").await, PolicyResult::Allow);
    }

    #[tokio::test]
    async fn allowlist_policy() {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Allowlist, vec!["alice".to_owned()]);
        assert_eq!(enforcer.check("alice").await, PolicyResult::Allow);
        assert_eq!(enforcer.check("bob").await, PolicyResult::Deny);
    }

    #[tokio::test]
    async fn disabled_policy_always_denies() {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Disabled, vec![]);
        assert_eq!(enforcer.check("anyone").await, PolicyResult::Deny);
    }

    #[tokio::test]
    async fn pairing_queue_full() {
        let enforcer = DmPolicyEnforcer::new(DmPolicy::Pairing, vec![]);
        for i in 0..MAX_PENDING_PAIRINGS {
            let r = enforcer.check(&format!("user_{i}")).await;
            assert!(matches!(r, PolicyResult::SendPairingCode(_)));
        }
        assert_eq!(
            enforcer.check("overflow_user").await,
            PolicyResult::PairingQueueFull
        );
    }

    #[test]
    fn pairing_code_format() {
        let code = generate_pairing_code();
        let parts: Vec<&str> = code.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 4);
    }

    #[test]
    fn channel_manager_low_tier_limit() {
        let mut mgr = ChannelManager::new(MemoryTier::Low);
        assert_eq!(mgr.max_concurrent(), 3);

        struct Dummy(String);
        impl Channel for Dummy {
            fn name(&self) -> &str {
                &self.0
            }
            fn send(&self, _: OutboundMessage) -> BoxFuture<'_, Result<()>> {
                Box::pin(async move { Ok(()) })
            }
            fn run(self: Arc<Self>) -> BoxFuture<'static, Result<()>> {
                Box::pin(async move { Ok(()) })
            }
        }

        for i in 0..3 {
            mgr.register(Arc::new(Dummy(format!("ch{i}")))).expect("ok");
        }
        assert!(mgr.register(Arc::new(Dummy("ch4".into()))).is_err());
    }
}
