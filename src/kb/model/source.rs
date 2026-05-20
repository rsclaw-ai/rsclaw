//! Source-side types: what kind of source a doc came from
//! (`KbSourceKind`), the typed source descriptor (`KbSource`), and
//! the idempotency key (`LogicalSourceId`) that lets us recognise
//! re-ingest of the same content/source across runs.
//!
//! See spec §1 model types + §I SourceIdentity.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbSourceKind {
    Doc,
    Chat,   // v2
    Url,
    Img,    // v2
    Mail,   // v2
}

impl KbSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Chat => "chat",
            Self::Url => "url",
            Self::Img => "img",
            Self::Mail => "mail",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "doc" => Ok(Self::Doc),
            "chat" => Ok(Self::Chat),
            "url" => Ok(Self::Url),
            "img" => Ok(Self::Img),
            "mail" => Ok(Self::Mail),
            o => Err(format!("unknown KbSourceKind: {o}")),
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Doc, Self::Chat, Self::Url, Self::Img, Self::Mail]
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbSource {
    Doc { path: PathBuf },
    Url { url: String, fetched_at: i64 },
    Chat { channel: String, range: (i64, i64) },
    Img { path: PathBuf },
    Mail { source: MailSource },
}

impl KbSource {
    pub fn kind(&self) -> KbSourceKind {
        match self {
            Self::Doc { .. } => KbSourceKind::Doc,
            Self::Url { .. } => KbSourceKind::Url,
            Self::Chat { .. } => KbSourceKind::Chat,
            Self::Img { .. } => KbSourceKind::Img,
            Self::Mail { .. } => KbSourceKind::Mail,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailSource {
    EmlFile { path: PathBuf },
    MboxFile { path: PathBuf },
    Imap { account: String, folder: String, uid: u64 },
    Gmail { account: String, thread_id: String, msg_id: String },
}

/// Idempotency key. Same content from the same source produces the
/// same id no matter how many times re-ingested — that's how we know
/// to reuse the existing markdown file, reuse chunk_ids, and avoid
/// double-indexing. Decoupled from `KbDoc.id` (a per-ingest ULID).
/// See spec §I SourceIdentity for the per-namespace generation rules.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct LogicalSourceId(pub String);

impl LogicalSourceId {
    pub fn for_file(sha256_hex: &str) -> Self {
        Self(format!("file:sha256:{sha256_hex}"))
    }

    pub fn for_url(normalized_url: &str) -> Self {
        Self(format!("url:{normalized_url}"))
    }

    pub fn for_chat_bucket(channel: &str, window_start_unix: i64) -> Self {
        Self(format!("chat:{channel}:{window_start_unix}"))
    }

    pub fn for_mail(message_id: &str) -> Self {
        Self(format!("mail:{message_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip() {
        for k in KbSourceKind::all() {
            assert_eq!(KbSourceKind::parse(k.as_str()).unwrap(), *k);
        }
    }

    #[test]
    fn kind_parse_rejects_unknown() {
        assert!(KbSourceKind::parse("audio").is_err());
    }

    #[test]
    fn source_to_kind() {
        assert_eq!(KbSource::Doc { path: "/x".into() }.kind(), KbSourceKind::Doc);
        assert_eq!(
            KbSource::Mail { source: MailSource::EmlFile { path: "/x.eml".into() } }.kind(),
            KbSourceKind::Mail
        );
    }

    #[test]
    fn logical_source_id_namespaces() {
        assert_eq!(LogicalSourceId::for_file("abc").as_str(), "file:sha256:abc");
        assert_eq!(LogicalSourceId::for_url("https://x").as_str(), "url:https://x");
        assert_eq!(
            LogicalSourceId::for_chat_bucket("feishu:pm", 1234567890).as_str(),
            "chat:feishu:pm:1234567890"
        );
        assert_eq!(LogicalSourceId::for_mail("<msg@host>").as_str(), "mail:<msg@host>");
    }

    #[test]
    fn logical_source_id_distinguishes_namespaces() {
        assert_ne!(LogicalSourceId::for_file("x"), LogicalSourceId::for_url("x"));
    }

    #[test]
    fn source_serde_roundtrip() {
        let s = KbSource::Url { url: "https://x".into(), fetched_at: 123 };
        let json = serde_json::to_string(&s).unwrap();
        let back: KbSource = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }
}
