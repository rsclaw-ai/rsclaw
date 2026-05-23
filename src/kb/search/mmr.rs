//! Maximum Marginal Relevance — greedy diversity selector.
//!
//! `mmr_score = λ * relevance - (1-λ) * max_sim_to_selected`.

pub fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < 1e-12 || nb < 1e-12 {
        0.0
    } else {
        dot / (na * nb)
    }
}

pub struct MmrCandidate<'a> {
    pub chunk_id: String,
    pub relevance: f32,
    pub vector: &'a [f32],
}

pub fn mmr_select<'a>(
    candidates: Vec<MmrCandidate<'a>>,
    k: usize,
    lambda: f32,
) -> Vec<(String, f32)> {
    let mut remaining = candidates;
    let mut selected: Vec<(String, f32, &[f32])> = Vec::new();
    let target = k.min(remaining.len());
    while selected.len() < target {
        let mut best_idx: Option<usize> = None;
        let mut best_score: f32 = f32::NEG_INFINITY;
        for (i, c) in remaining.iter().enumerate() {
            let max_sim_to_selected = selected
                .iter()
                .map(|(_, _, v)| cosine_sim(c.vector, v))
                .fold(0.0_f32, f32::max);
            let score = lambda * c.relevance - (1.0 - lambda) * max_sim_to_selected;
            if score > best_score {
                best_score = score;
                best_idx = Some(i);
            }
        }
        if let Some(i) = best_idx {
            let c = remaining.remove(i);
            selected.push((c.chunk_id, best_score, c.vector));
        } else {
            break;
        }
    }
    selected.into_iter().map(|(id, sc, _)| (id, sc)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand<'a>(id: &str, rel: f32, v: &'a [f32]) -> MmrCandidate<'a> {
        MmrCandidate {
            chunk_id: id.into(),
            relevance: rel,
            vector: v,
        }
    }

    #[test]
    fn mmr_lambda_1_picks_by_relevance() {
        let v1 = vec![1.0, 0.0];
        let v2 = vec![1.0, 0.0];
        let v3 = vec![0.0, 1.0];
        let r = mmr_select(
            vec![
                cand("c1", 0.9, &v1),
                cand("c2", 0.5, &v2),
                cand("c3", 0.4, &v3),
            ],
            3,
            1.0,
        );
        assert_eq!(r[0].0, "c1");
        assert_eq!(r[1].0, "c2");
    }

    #[test]
    fn mmr_lambda_0_picks_diverse() {
        let v1 = vec![1.0, 0.0];
        let v2 = vec![1.0, 0.0]; // identical to c1
        let v3 = vec![0.0, 1.0]; // orthogonal
        let r = mmr_select(
            vec![
                cand("c1", 0.9, &v1),
                cand("c2", 0.85, &v2),
                cand("c3", 0.4, &v3),
            ],
            2,
            0.0,
        );
        assert_eq!(r[0].0, "c1");
        assert_eq!(r[1].0, "c3");
    }

    #[test]
    fn mmr_handles_fewer_candidates_than_k() {
        let v = vec![1.0, 0.0];
        let r = mmr_select(vec![cand("c1", 0.9, &v)], 5, 0.5);
        assert_eq!(r.len(), 1);
    }
}
