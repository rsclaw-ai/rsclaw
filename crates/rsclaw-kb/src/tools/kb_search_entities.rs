//! kb_search_entities: surface → entity_id lookup.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::kb::{
    model::{CallerScope, EntityKind},
    search::SearchCtx,
    store::entities,
};

#[derive(Debug, Deserialize)]
pub struct KbSearchEntitiesInput {
    pub query: String,
    pub kind: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub struct KbSearchEntitiesOutput {
    pub matches: Vec<EntityMatch>,
}

#[derive(Debug, Serialize)]
pub struct EntityMatch {
    pub entity_id: String,
    pub canonical_name: String,
    pub kind: String,
    pub aliases: Vec<String>,
}

pub fn run(
    ctx: &SearchCtx,
    input: KbSearchEntitiesInput,
    _scope: &CallerScope,
) -> Result<KbSearchEntitiesOutput> {
    let rtx = ctx.store.begin_read()?;
    let kind_filter: Option<EntityKind> = input.kind.as_deref().and_then(|s| parse_entity_kind(s));
    let rows = entities::find_by_surface(&rtx, &input.query, kind_filter, input.limit)?;
    let out: Vec<EntityMatch> = rows
        .into_iter()
        .map(|e| EntityMatch {
            entity_id: e.canonical_id.clone(),
            // KbEntity has no separate canonical_name; use the first
            // surface form as the display name.
            canonical_name: e.surface_forms.first().cloned().unwrap_or_default(),
            kind: entity_kind_str(e.kind).to_string(),
            aliases: e.surface_forms,
        })
        .collect();
    Ok(KbSearchEntitiesOutput { matches: out })
}

fn parse_entity_kind(s: &str) -> Option<EntityKind> {
    match s.to_lowercase().as_str() {
        "brand" => Some(EntityKind::Brand),
        "person" => Some(EntityKind::Person),
        "org" => Some(EntityKind::Org),
        "email" => Some(EntityKind::Email),
        "url" => Some(EntityKind::Url),
        "hashtag" => Some(EntityKind::Hashtag),
        "other" => Some(EntityKind::Other),
        _ => None,
    }
}

fn entity_kind_str(k: EntityKind) -> &'static str {
    match k {
        EntityKind::Brand => "brand",
        EntityKind::Person => "person",
        EntityKind::Org => "org",
        EntityKind::Email => "email",
        EntityKind::Url => "url",
        EntityKind::Hashtag => "hashtag",
        EntityKind::Other => "other",
    }
}
