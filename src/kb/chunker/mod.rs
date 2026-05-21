//! Slice canonical markdown into chunks with deterministic ids
//! derived from `logical_source_id`. See spec §I + §2.
//!
//! Targets ~512 token chunks (DEFAULT_TARGET_TOKENS); near-dup
//! chunks (Hamming distance ≤ SIMHASH_DEDUP_THRESHOLD on the
//! 64-bit SimHash) are dropped to avoid double-indexing.

pub mod tokens;
pub mod splitter;

use crate::kb::canonicalize::md::heading_path_at;
use crate::kb::model::{
    chunk_id, hamming64, simhash64, ChunkStatus, KbChunk, KbLocator, LogicalSourceId,
};
use splitter::split_paragraphs;
use tokens::approx_token_count;

pub const DEFAULT_TARGET_TOKENS: u32 = 512;
pub const DEFAULT_MIN_TOKENS: u32 = 50;
pub const SIMHASH_DEDUP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone)]
pub struct ChunkerInput<'a> {
    pub logical_source_id: &'a LogicalSourceId,
    pub doc_id: &'a str,
    pub doc_version: u32,
    pub markdown_body: &'a str,
    pub default_locator_kind: LocatorKind,
}

#[derive(Debug, Clone, Copy)]
pub enum LocatorKind {
    Offset,
    MdSection,
}

pub fn chunk_markdown(input: ChunkerInput<'_>) -> Vec<KbChunk> {
    let paras = split_paragraphs(input.markdown_body);
    let mut chunks: Vec<KbChunk> = Vec::new();
    let mut buf = String::new();
    let mut buf_start: Option<usize> = None;
    let mut buf_end: usize = 0;
    let mut seq = 0u32;

    for p in &paras {
        let tentative = approx_token_count(&buf) + approx_token_count(&p.text);
        if !buf.is_empty() && tentative > DEFAULT_TARGET_TOKENS {
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
            buf.clear();
            buf_start = None;
        }
        if buf.is_empty() {
            buf_start = Some(p.start);
        }
        if !buf.is_empty() {
            buf.push_str("\n\n");
        }
        buf.push_str(&p.text);
        buf_end = p.end;
        if approx_token_count(&buf) >= DEFAULT_TARGET_TOKENS {
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
            buf.clear();
            buf_start = None;
        }
    }
    if !buf.is_empty() {
        flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
    }
    deduplicate(&mut chunks);
    chunks
}

fn flush(
    out: &mut Vec<KbChunk>,
    seq: &mut u32,
    input: &ChunkerInput<'_>,
    start: usize,
    end: usize,
    body: &str,
) {
    // Use `end`, not `start`: a chunk that contains its own headings
    // (e.g. "# Title\n## Sub\nbody" as a single paragraph) should
    // index under "Title > Sub", which is the heading state at the
    // END of the chunk, not the empty state before the first heading.
    let path = heading_path_at(input.markdown_body, end);
    let indexed = if path.is_empty() {
        body.to_string()
    } else {
        format!("{}\n\n{body}", path.join(" > "))
    };
    let id = chunk_id(input.logical_source_id, *seq, body);
    let sim = simhash64(body);
    let locator = match input.default_locator_kind {
        LocatorKind::Offset => KbLocator::Offset { start, end },
        LocatorKind::MdSection => KbLocator::MdSection {
            heading_path: path.clone(),
        },
    };
    out.push(KbChunk {
        id,
        doc_id: input.doc_id.to_string(),
        logical_source_id: input.logical_source_id.0.clone(),
        doc_version: input.doc_version,
        seq: *seq,
        heading_path: path,
        byte_offset: (start as u64, end as u64),
        indexed_text: indexed,
        vector: vec![], // Week 2 fills
        simhash: sim,
        locator,
        status: ChunkStatus::Active,
        source_quality: 1.0,
        embedder_id: String::new(),
    });
    *seq += 1;
}

fn deduplicate(chunks: &mut Vec<KbChunk>) {
    let mut kept: Vec<u64> = Vec::with_capacity(chunks.len());
    chunks.retain(|c| {
        for sh in &kept {
            if hamming64(*sh, c.simhash) <= SIMHASH_DEDUP_THRESHOLD {
                return false;
            }
        }
        kept.push(c.simhash);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsid() -> LogicalSourceId {
        LogicalSourceId::for_file("hash")
    }

    #[test]
    fn short_doc_one_chunk() {
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: "tiny.",
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn long_doc_multi_chunks() {
        let md = "para one.\n\n".to_string() + &"para text here.\n\n".repeat(500);
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: &md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert!(chunks.len() > 1);
    }

    #[test]
    fn heading_path_in_indexed_text() {
        let md = "# Mengniu\n## Recipe\n100g + 100ml.";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::MdSection,
        });
        assert!(chunks[0].indexed_text.starts_with("Mengniu > Recipe"));
        assert_eq!(
            chunks[0].heading_path,
            vec!["Mengniu".to_string(), "Recipe".to_string()]
        );
    }

    /// Critical idempotency invariant from spec §I: chunk_ids must
    /// not depend on doc_id or doc_version, only on logical_source_id.
    #[test]
    fn idempotent_chunk_ids() {
        let md = "hello.\n\nworld.";
        let a = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        let b = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d2",
            doc_version: 5, // different doc_id and version
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(
                x.id, y.id,
                "chunk_id must NOT depend on doc_id/version, only on logical_source_id"
            );
        }
    }

    #[test]
    fn near_dup_dedup() {
        let md = "the quick brown fox jumps over the lazy dog\n\n\
                  the quick brown fox jumps over the lazy dog";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(chunks.len(), 1, "identical chunks must be deduped");
    }

    #[test]
    fn byte_offsets_in_bounds() {
        let md = "first.\n\nsecond.";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1",
            doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        for c in chunks {
            let (s, e) = (c.byte_offset.0 as usize, c.byte_offset.1 as usize);
            assert!(e <= md.len());
            assert!(s < e);
            assert!(!md[s..e].trim().is_empty());
        }
    }
}
