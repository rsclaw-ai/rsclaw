//! kb_fetch: by chunk_id, return chunk + optional neighbor context.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    content_store::read::{read_doc_body, read_doc_range},
    model::CallerScope,
    search::{
        SearchCtx,
        filter::{SearchFilter, is_latest_version, keep_doc},
    },
    store::{chunks, docs},
};

#[derive(Debug, Deserialize)]
pub struct KbFetchInput {
    pub chunk_id: String,
    #[serde(default)]
    pub expand: String,
}

#[derive(Debug, Serialize)]
pub struct KbFetchOutput {
    pub chunk: ChunkPayload,
    pub neighbors: Vec<ChunkPayload>,
    pub full_doc: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChunkPayload {
    pub chunk_id: String,
    pub doc_id: String,
    pub heading_path: Vec<String>,
    pub text: String,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbFetchInput,
    scope: &CallerScope,
) -> Result<Option<KbFetchOutput>> {
    let rtx = ctx.store.begin_read()?;
    let c = match chunks::get(&rtx, &input.chunk_id)? {
        Some(c) => c,
        None => return Ok(None),
    };
    let d = match docs::get(&rtx, &c.doc_id)? {
        Some(d) => d,
        None => return Ok(None),
    };
    if !keep_doc(&d, scope, &SearchFilter::default()) {
        return Ok(None);
    }
    if !is_latest_version(&rtx, &d)? {
        return Ok(None);
    }
    let abs = ctx.paths.root.join(&d.markdown_path);
    let chunk_text = read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1)?;
    let main = ChunkPayload {
        chunk_id: c.id.clone(),
        doc_id: c.doc_id.clone(),
        heading_path: c.heading_path.clone(),
        text: chunk_text,
    };

    let neighbors: Vec<ChunkPayload> = match input.expand.as_str() {
        "neighbor" => {
            let all = chunks::chunks_for_logical(&rtx, &c.logical_source_id)?;
            let mut adj: Vec<_> = all
                .into_iter()
                .filter(|x| x.doc_id == c.doc_id && (x.seq + 1 == c.seq || x.seq == c.seq + 1))
                .collect();
            adj.sort_by_key(|x| x.seq);
            adj.into_iter()
                .map(|x| ChunkPayload {
                    chunk_id: x.id.clone(),
                    doc_id: x.doc_id.clone(),
                    heading_path: x.heading_path.clone(),
                    text: read_doc_range(&abs, x.byte_offset.0, x.byte_offset.1)
                        .unwrap_or_default(),
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let full_doc = if input.expand == "full_doc" {
        Some(read_doc_body(&abs)?)
    } else {
        None
    };

    Ok(Some(KbFetchOutput {
        chunk: main,
        neighbors,
        full_doc,
    }))
}
