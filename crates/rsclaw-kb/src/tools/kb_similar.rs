//! kb_similar: vector neighbors of a chunk.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    model::CallerScope,
    search::{
        SearchCtx,
        filter::{SearchFilter, is_latest_version, keep_doc},
    },
    store::{chunks, docs},
};

#[derive(Debug, Deserialize)]
pub struct KbSimilarInput {
    pub chunk_id: String,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default = "default_min_score")]
    pub min_score: f32,
    #[serde(default)]
    pub exclude_neighbors: bool,
}

fn default_k() -> usize {
    8
}
fn default_scope() -> String {
    "any".into()
}
fn default_min_score() -> f32 {
    0.0
}

#[derive(Debug, Serialize)]
pub struct KbSimilarOutput {
    pub neighbors: Vec<NeighborHit>,
}

#[derive(Debug, Serialize)]
pub struct NeighborHit {
    pub chunk_id: String,
    pub doc_id: String,
    pub score: f32,
}

pub fn run(ctx: &SearchCtx, input: KbSimilarInput, scope: &CallerScope) -> Result<KbSimilarOutput> {
    let rtx = ctx.store.begin_read()?;
    let seed = match chunks::get(&rtx, &input.chunk_id)? {
        Some(c) => c,
        None => return Ok(KbSimilarOutput { neighbors: vec![] }),
    };
    let raw = ctx.index.hnsw.search(&seed.vector, input.k * 3);
    let mut out = Vec::new();
    for (cid, score) in raw {
        if cid == input.chunk_id {
            continue;
        }
        if score < input.min_score {
            continue;
        }
        let c = match chunks::get(&rtx, &cid)? {
            Some(c) => c,
            None => continue,
        };
        let d = match docs::get(&rtx, &c.doc_id)? {
            Some(d) => d,
            None => continue,
        };
        if !keep_doc(&d, scope, &SearchFilter::default()) || !is_latest_version(&rtx, &d)? {
            continue;
        }
        match input.scope.as_str() {
            "same_doc" if c.doc_id != seed.doc_id => continue,
            "other_docs" if c.doc_id == seed.doc_id => continue,
            _ => {}
        }
        if input.exclude_neighbors
            && c.logical_source_id == seed.logical_source_id
            && (c.seq + 1 == seed.seq || c.seq == seed.seq + 1)
        {
            continue;
        }
        out.push(NeighborHit {
            chunk_id: cid,
            doc_id: c.doc_id,
            score,
        });
        if out.len() == input.k {
            break;
        }
    }
    Ok(KbSimilarOutput { neighbors: out })
}
