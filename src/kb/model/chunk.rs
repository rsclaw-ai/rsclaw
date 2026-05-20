//! Chunk-level types. `chunk_id` is deterministic and keyed on
//! `logical_source_id` (NOT `doc_id`), so re-ingesting the same source
//! produces identical chunk_ids and the indexing/upsert path is
//! naturally idempotent. See spec §I + §2 chunker.

use crate::kb::model::{KbLocator, LogicalSourceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Deterministic chunk id.
///
/// Layout: `sha256(logical_source_id | 0x00 | seq_be32 | 0x00 | content)`
/// truncated to 32 hex chars. The null-byte separators stop adjacent-field
/// collisions (e.g. `("abc", 1, "d")` vs `("ab", 12, "cd")`); seq is fixed
/// 4-byte big-endian for the same reason.
///
/// Crucially keyed on **logical_source_id** so re-ingest of the same source
/// (which produces a new `doc_id` ULID) lands on the same chunk_ids.
pub fn chunk_id(lsid: &LogicalSourceId, seq: u32, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(lsid.as_str().as_bytes());
    h.update([0u8]);
    h.update(seq.to_be_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in h.finalize().iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex.truncate(32);
    hex
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStatus {
    Active,
    Tombstoned,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbChunk {
    pub id: String,                   // 32-hex deterministic
    pub doc_id: String,               // ULID, links to current KbDoc instance
    pub logical_source_id: String,    // links across versions, used for dedup
    pub doc_version: u32,             // matches KbDoc.version
    pub seq: u32,
    pub heading_path: Vec<String>,
    pub byte_offset: (u64, u64),      // start..end within the doc body
    pub indexed_text: String,         // heading_path > ... \n\n body
    pub vector: Vec<f32>,             // empty in Week 1; Week 2 embedder fills
    pub simhash: u64,
    pub locator: KbLocator,
    pub status: ChunkStatus,
    pub source_quality: f32,
    pub embedder_id: String,          // empty in Week 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_deterministic() {
        let lsid = LogicalSourceId::for_file("abc");
        let a = chunk_id(&lsid, 0, "hello world");
        let b = chunk_id(&lsid, 0, "hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn chunk_id_varies_with_seq() {
        let lsid = LogicalSourceId::for_file("abc");
        assert_ne!(chunk_id(&lsid, 0, "x"), chunk_id(&lsid, 1, "x"));
    }

    #[test]
    fn chunk_id_varies_with_content() {
        let lsid = LogicalSourceId::for_file("abc");
        assert_ne!(chunk_id(&lsid, 0, "hello"), chunk_id(&lsid, 0, "world"));
    }

    #[test]
    fn chunk_id_varies_with_logical_source() {
        let l1 = LogicalSourceId::for_file("abc");
        let l2 = LogicalSourceId::for_file("def");
        assert_ne!(chunk_id(&l1, 0, "x"), chunk_id(&l2, 0, "x"));
    }

    /// Critical guarantee for idempotency: re-ingesting the same file
    /// (= same logical_source_id) produces the same chunk_ids,
    /// regardless of doc_id.
    #[test]
    fn reingest_same_file_same_chunk_ids() {
        let lsid = LogicalSourceId::for_file("hash_of_file_contents");
        let body = "the actual canonical markdown body";
        let id_first = chunk_id(&lsid, 0, body);
        let id_second = chunk_id(&lsid, 0, body);
        assert_eq!(
            id_first, id_second,
            "re-ingest must produce same chunk_id (idempotency invariant)"
        );
    }

    #[test]
    fn null_separators_avoid_field_collisions() {
        // ("abc", 1, "d") vs ("ab", 12, "cd") would collide under naive concat
        // because their concatenation is "abc1d" vs "ab12cd" → different,
        // but adjacent-field merges (e.g. seq written as ASCII) could collide.
        // We use null separators + fixed-width seq to make the encoding
        // unambiguous. This test guards the invariant.
        let l1 = LogicalSourceId("ns1".into());
        let l2 = LogicalSourceId("ns".into());
        // 1 vs 12 (different u32) ensures different fixed-width encoding too
        assert_ne!(chunk_id(&l1, 1, "d"), chunk_id(&l2, 12, "cd"));
    }

    #[test]
    fn struct_serde_roundtrip() {
        let lsid = LogicalSourceId::for_file("abc");
        let c = KbChunk {
            id: chunk_id(&lsid, 0, "hi"),
            doc_id: "doc1".into(),
            logical_source_id: lsid.0.clone(),
            doc_version: 1,
            seq: 0,
            heading_path: vec!["A".into()],
            byte_offset: (0, 2),
            indexed_text: "A\n\nhi".into(),
            vector: vec![],
            simhash: 0,
            locator: KbLocator::Offset { start: 0, end: 2 },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: String::new(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: KbChunk = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }
}
