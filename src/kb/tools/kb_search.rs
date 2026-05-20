//! kb_search tool. JSON-friendly request/response wrapper around
//! search::pipeline. CallerScope is injected by the agent runtime;
//! agent tool calls cannot supply it.

use crate::kb::entities::extract::{canonical_id, extract_entities};
use crate::kb::model::{CallerScope, KbSourceKind};
use crate::kb::search::{
    Diversity, RetrievalHit, SearchCtx, SearchFilter, SearchMode, SearchRequest,
};
use crate::kb::store::entities;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
pub struct KbSearchInput {
    pub query: String,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default)]
    pub filter: KbSearchFilter,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub diversity: String,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f32,
    #[serde(default)]
    pub boost_entities: Vec<String>,
}

fn default_k() -> usize { 8 }
fn default_mmr_lambda() -> f32 { 0.5 }

#[derive(Debug, Default, Deserialize)]
pub struct KbSearchFilter {
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_kind: Option<String>,
    #[serde(default)]
    pub doc_ids: Vec<String>,
    #[serde(default)]
    pub entity_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct KbSearchOutput {
    pub results: Vec<RetrievalHit>,
    pub entity_alignment: Vec<EntityAlignment>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct EntityAlignment {
    pub entity_surface: String,
    pub canonical_id: String,
    pub matched_chunks: usize,
    pub total: usize,
}

pub fn run(ctx: &SearchCtx, input: KbSearchInput, scope: &CallerScope) -> Result<KbSearchOutput> {
    let filter = SearchFilter {
        tags: input.filter.tags,
        source_kind: input
            .filter
            .source_kind
            .as_deref()
            .and_then(|s| KbSourceKind::parse(s).ok()),
        doc_ids: if input.filter.doc_ids.is_empty() {
            None
        } else {
            Some(input.filter.doc_ids.into_iter().collect::<HashSet<_>>())
        },
        require_entities: input.filter.entity_ids,
    };
    let mode = match input.mode.as_str() {
        "dense" => SearchMode::Dense,
        "bm25" => SearchMode::Bm25,
        "auto" | "" => SearchMode::Auto,
        _ => SearchMode::Hybrid,
    };
    let diversity = match input.diversity.as_str() {
        "off" => Diversity::Off,
        _ => Diversity::Mmr,
    };
    let req = SearchRequest {
        query: input.query,
        k: input.k,
        filter,
        mode,
        diversity,
        mmr_lambda: input.mmr_lambda,
        boost_entities: input.boost_entities,
    };
    let results = ctx.search(&req, scope)?;

    // Entity alignment: run the same regex extractor on the query
    // string. For each mention, compare its canonical_id against the
    // entity edges of the returned hits AND against the corpus-wide
    // chunk count for that entity. If the query mentions an entity
    // that doesn't appear in the results, surface it as a warning so
    // the agent doesn't hallucinate cross-entity answers.
    let mut entity_alignment: Vec<EntityAlignment> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let query_mentions = extract_entities(&req.query);
    if !query_mentions.is_empty() {
        let rtx = ctx.store.begin_read()?;
        let result_chunk_ids: HashSet<&str> =
            results.iter().map(|h| h.chunk_id.as_str()).collect();
        for m in query_mentions {
            let cid = canonical_id(m.kind, &m.surface);
            let chunk_edges = match entities::chunks_for_entity(&rtx, &cid) {
                Ok(edges) => edges,
                Err(e) => {
                    tracing::warn!(
                        entity = %cid,
                        "kb_search: entity_alignment lookup failed: {e}"
                    );
                    Vec::new()
                }
            };
            let total = chunk_edges.len();
            let matched = chunk_edges
                .iter()
                .filter(|e| result_chunk_ids.contains(e.chunk_id.as_str()))
                .count();
            entity_alignment.push(EntityAlignment {
                entity_surface: m.surface.clone(),
                canonical_id: cid,
                matched_chunks: matched,
                total,
            });
            if matched == 0 && total > 0 {
                warnings.push(format!(
                    "query mentions [{}] but none of the {} chunks containing it appear in results — possible entity mismatch",
                    m.surface, total
                ));
            } else if total == 0 {
                warnings.push(format!(
                    "query mentions [{}] but the knowledge base has no chunks containing it",
                    m.surface
                ));
            }
        }
    }

    Ok(KbSearchOutput {
        results,
        entity_alignment,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_defaults() {
        let i: KbSearchInput = serde_json::from_str(r#"{"query":"hi"}"#).unwrap();
        assert_eq!(i.k, 8);
        assert_eq!(i.mmr_lambda, 0.5);
    }

    #[test]
    fn input_filter_parses() {
        let i: KbSearchInput =
            serde_json::from_str(r#"{"query":"hi","filter":{"tags":["a"]}}"#).unwrap();
        assert_eq!(i.filter.tags, vec!["a"]);
    }
}
