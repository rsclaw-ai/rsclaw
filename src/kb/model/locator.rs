//! Where inside a doc a chunk lives. Renders to a human-readable
//! citation string for retrieval responses (`p.12`, `§A > B`, …).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbLocator {
    PdfPage { page: u32, bbox: Option<(f32, f32, f32, f32)> },
    MdSection { heading_path: Vec<String> },
    UrlAnchor { fragment: Option<String> },
    ChatMsgs { first_ts: i64, last_ts: i64 },
    Image { bbox: Option<(f32, f32, f32, f32)> },
    Offset { start: usize, end: usize },
}

impl KbLocator {
    pub fn human(&self) -> String {
        match self {
            Self::PdfPage { page, .. } => format!("p.{page}"),
            Self::MdSection { heading_path } => {
                if heading_path.is_empty() {
                    String::from("§")
                } else {
                    format!("§{}", heading_path.join(" > "))
                }
            }
            Self::UrlAnchor { fragment } => fragment
                .as_deref()
                .map(|f| format!("#{f}"))
                .unwrap_or_default(),
            Self::ChatMsgs { first_ts, last_ts } => format!("{first_ts}..{last_ts}"),
            Self::Image { .. } => String::from("image"),
            Self::Offset { start, end } => format!("bytes {start}..{end}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_format() {
        assert_eq!(KbLocator::PdfPage { page: 12, bbox: None }.human(), "p.12");
        assert_eq!(
            KbLocator::MdSection { heading_path: vec!["A".into(), "B".into()] }.human(),
            "§A > B"
        );
        assert_eq!(
            KbLocator::MdSection { heading_path: vec![] }.human(),
            "§"
        );
        assert_eq!(
            KbLocator::UrlAnchor { fragment: Some("s1".into()) }.human(),
            "#s1"
        );
        assert_eq!(
            KbLocator::UrlAnchor { fragment: None }.human(),
            ""
        );
        assert_eq!(KbLocator::Image { bbox: None }.human(), "image");
        assert_eq!(KbLocator::Offset { start: 0, end: 5 }.human(), "bytes 0..5");
        assert_eq!(
            KbLocator::ChatMsgs { first_ts: 100, last_ts: 200 }.human(),
            "100..200"
        );
    }

    #[test]
    fn serde_roundtrip() {
        let l = KbLocator::PdfPage { page: 7, bbox: Some((1.0, 2.0, 3.0, 4.0)) };
        let s = serde_json::to_string(&l).unwrap();
        let back: KbLocator = serde_json::from_str(&s).unwrap();
        assert_eq!(l, back);
    }
}
