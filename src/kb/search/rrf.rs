//! Reciprocal Rank Fusion across N ranked lists.
//!
//! Each list is `(chunk_id, score)` sorted by descending score; RRF
//! re-ranks by summing `1/(k + rank_i + 1)` across lists. Tie-break
//! is deterministic by `(rrf_score desc, chunk_id asc)`.

use std::collections::HashMap;

const RRF_K: f32 = 60.0;

pub fn rrf_fuse(lists: &[&[(String, f32)]]) -> Vec<(String, f32)> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    for list in lists {
        for (rank, (id, _)) in list.iter().enumerate() {
            let contrib = 1.0 / (RRF_K + rank as f32 + 1.0);
            *scores.entry(id.clone()).or_insert(0.0) += contrib;
        }
    }
    let mut out: Vec<(String, f32)> = scores.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rl(items: &[(&str, f32)]) -> Vec<(String, f32)> {
        items.iter().map(|(a, b)| (a.to_string(), *b)).collect()
    }

    #[test]
    fn rrf_empty_returns_empty() {
        let out = rrf_fuse(&[]);
        assert!(out.is_empty());
    }

    #[test]
    fn rrf_single_list_preserves_order() {
        let a = rl(&[("c1", 0.9), ("c2", 0.8), ("c3", 0.7)]);
        let out = rrf_fuse(&[&a]);
        assert_eq!(out[0].0, "c1");
        assert_eq!(out[1].0, "c2");
        assert_eq!(out[2].0, "c3");
    }

    #[test]
    fn rrf_two_lists_chunks_in_both_rank_higher() {
        let a = rl(&[("c1", 0.9), ("c2", 0.8)]);
        let b = rl(&[("c1", 0.9), ("c4", 0.7)]);
        let out = rrf_fuse(&[&a, &b]);
        assert_eq!(out[0].0, "c1");
    }

    #[test]
    fn rrf_ties_break_by_chunk_id() {
        let a = rl(&[("c2", 0.9)]);
        let b = rl(&[("c1", 0.9)]);
        let out = rrf_fuse(&[&a, &b]);
        assert_eq!(out[0].0, "c1");
        assert_eq!(out[1].0, "c2");
    }
}
