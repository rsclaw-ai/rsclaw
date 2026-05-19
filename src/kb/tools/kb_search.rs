//! kb_search tool. JSON-friendly request/response wrapper around
//! search::pipeline. CallerScope is injected by the agent runtime;
//! agent tool calls cannot supply it.

use crate::kb::model::{CallerScope, KbSourceKind};
use crate::kb::search::{
    Diversity, RetrievalHit, SearchCtx, SearchFilter, SearchMode, SearchRequest,
};
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
    Ok(KbSearchOutput {
        results,
        entity_alignment: Vec::new(),
        warnings: Vec::new(),
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
