//! kb_list_docs: paginated listing of visible docs.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{
    model::{CallerScope, KbSourceKind},
    search::{
        SearchCtx,
        filter::{SearchFilter, is_latest_version, keep_doc},
    },
    store::{codec::decode, schema::KB_DOCS},
};

#[derive(Debug, Deserialize)]
pub struct KbListDocsInput {
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_limit() -> usize {
    50
}

#[derive(Debug, Serialize)]
pub struct KbListDocsOutput {
    pub docs: Vec<DocSummary>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DocSummary {
    pub doc_id: String,
    pub title: String,
    pub source_kind: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub version: u32,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbListDocsInput,
    scope: &CallerScope,
) -> Result<KbListDocsOutput> {
    let filter = SearchFilter {
        tags: input.tags,
        source_kind: input
            .source_kind
            .as_deref()
            .and_then(|s| KbSourceKind::parse(s).ok()),
        doc_ids: None,
        require_entities: vec![],
    };
    let rtx = ctx.store.begin_read()?;
    let tbl = rtx.open_table(KB_DOCS)?;
    let cursor_key = input.cursor.unwrap_or_default();
    let mut out = Vec::new();
    let mut next: Option<String> = None;
    for entry in tbl.range::<&str>(cursor_key.as_str()..)? {
        let (k, v) = entry?;
        let key = k.value().to_string();
        if !cursor_key.is_empty() && key == cursor_key {
            continue;
        }
        let d: crate::model::KbDoc = decode(v.value())?;
        if !keep_doc(&d, scope, &filter) {
            continue;
        }
        if !is_latest_version(&rtx, &d)? {
            continue;
        }
        if out.len() == input.limit {
            next = Some(key);
            break;
        }
        out.push(DocSummary {
            doc_id: d.id.clone(),
            title: d.title.clone(),
            source_kind: d.source_kind.as_str().to_string(),
            tags: d.tags.clone(),
            created_at: d.created_at,
            version: d.version,
        });
    }
    Ok(KbListDocsOutput {
        docs: out,
        next_cursor: next,
    })
}
