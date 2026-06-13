//! KbEntity + KbEntityIndex accessors.
//!
//! `kb_entities`: entity_id (canonical_id) → KbEntity.
//! `kb_entity_index`: composite key `entity_id\0chunk_id` →
//!     KbEntityIndex (entity↔chunk membership edge with mention_count
//!     + score; used by boost_entities / require_entities filters at
//!     retrieval time).
//!
//! Surface→entity lookup (for `kb_search_entities`) scans the entities
//! table filtering by case-insensitive `surface_forms` membership.
//! O(n) for MVP; a dedicated inverted index lands in Week 4 when
//! entity extraction starts emitting non-trivial entity counts.

use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::kb::{
    model::{EntityKind, KbEntity, KbEntityIndex},
    store::{
        codec::{decode, encode},
        schema::{KB_ENTITIES, KB_ENTITY_INDEX},
    },
};

pub fn put_entity(wtx: &WriteTransaction, e: &KbEntity) -> Result<()> {
    let bytes = encode(e)?;
    let mut tbl = wtx.open_table(KB_ENTITIES)?;
    tbl.insert(e.canonical_id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get_entity(rtx: &ReadTransaction, entity_id: &str) -> Result<Option<KbEntity>> {
    let tbl = rtx.open_table(KB_ENTITIES)?;
    match tbl.get(entity_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Linear scan over `kb_entities`, returns entities whose surface_forms
/// contain `surface` (case-insensitive). Optional `kind_filter`
/// further narrows by entity kind. Capped at `limit` matches.
pub fn find_by_surface(
    rtx: &ReadTransaction,
    surface: &str,
    kind_filter: Option<EntityKind>,
    limit: usize,
) -> Result<Vec<KbEntity>> {
    let needle = surface.to_lowercase();
    let tbl = rtx.open_table(KB_ENTITIES)?;
    let mut out = Vec::new();
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let e: KbEntity = decode(v.value())?;
        if let Some(k) = kind_filter {
            if e.kind != k {
                continue;
            }
        }
        if e.surface_forms
            .iter()
            .any(|s| s.to_lowercase() == needle || s.to_lowercase().contains(&needle))
        {
            out.push(e);
            if out.len() >= limit {
                break;
            }
        }
    }
    Ok(out)
}

/// Write an entity↔chunk edge. Composite key keeps multiple mentions
/// of the same entity across many chunks indexable.
pub fn put_index(wtx: &WriteTransaction, idx: &KbEntityIndex) -> Result<()> {
    let key = compose_idx_key(&idx.entity_id, &idx.chunk_id);
    let bytes = encode(idx)?;
    let mut tbl = wtx.open_table(KB_ENTITY_INDEX)?;
    tbl.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}

/// All chunks that mention `entity_id`.
pub fn chunks_for_entity(rtx: &ReadTransaction, entity_id: &str) -> Result<Vec<KbEntityIndex>> {
    let prefix = format!("{entity_id}\0");
    let end = format!("{entity_id}\u{1}");
    let tbl = rtx.open_table(KB_ENTITY_INDEX)?;
    let mut out = Vec::new();
    for entry in tbl.range(prefix.as_str()..end.as_str())? {
        let (_, v) = entry?;
        out.push(decode(v.value())?);
    }
    Ok(out)
}

fn compose_idx_key(entity_id: &str, chunk_id: &str) -> String {
    format!("{entity_id}\0{chunk_id}")
}

#[cfg(test)]
mod tests {
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    use super::*;
    use crate::kb::store::open_db;

    fn sample_entity(canonical_id: &str, surfaces: &[&str], kind: EntityKind) -> KbEntity {
        KbEntity {
            canonical_id: canonical_id.into(),
            surface_forms: surfaces.iter().map(|s| s.to_string()).collect(),
            kind,
            created_at: 0,
        }
    }

    #[test]
    fn put_get_entity_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_entity(
                &wtx,
                &sample_entity("ent_mengniu", &["蒙牛", "Mengniu"], EntityKind::Brand),
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let e = get_entity(&rtx, "ent_mengniu").unwrap().unwrap();
        assert_eq!(
            e.surface_forms,
            vec!["蒙牛".to_string(), "Mengniu".to_string()]
        );
    }

    #[test]
    fn find_by_surface_case_insensitive() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_entity(
                &wtx,
                &sample_entity("ent_apple", &["Apple", "苹果"], EntityKind::Brand),
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(find_by_surface(&rtx, "apple", None, 10).unwrap().len(), 1);
        assert_eq!(find_by_surface(&rtx, "APPLE", None, 10).unwrap().len(), 1);
        assert_eq!(find_by_surface(&rtx, "苹果", None, 10).unwrap().len(), 1);
        assert_eq!(find_by_surface(&rtx, "missing", None, 10).unwrap().len(), 0);
    }

    #[test]
    fn find_by_surface_filters_by_kind() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_entity(
                &wtx,
                &sample_entity("ent_brand", &["Apple"], EntityKind::Brand),
            )
            .unwrap();
            put_entity(
                &wtx,
                &sample_entity("ent_org", &["Apple Inc"], EntityKind::Org),
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let brand_only = find_by_surface(&rtx, "apple", Some(EntityKind::Brand), 10).unwrap();
        assert_eq!(brand_only.len(), 1);
        assert_eq!(brand_only[0].canonical_id, "ent_brand");
    }

    #[test]
    fn chunks_for_entity_isolates_by_entity_id() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_index(
                &wtx,
                &KbEntityIndex {
                    entity_id: "ent_a".into(),
                    chunk_id: "c1".into(),
                    doc_id: "d1".into(),
                    mention_count: 2,
                    score: 0.8,
                },
            )
            .unwrap();
            put_index(
                &wtx,
                &KbEntityIndex {
                    entity_id: "ent_a".into(),
                    chunk_id: "c2".into(),
                    doc_id: "d1".into(),
                    mention_count: 1,
                    score: 0.5,
                },
            )
            .unwrap();
            put_index(
                &wtx,
                &KbEntityIndex {
                    entity_id: "ent_b".into(),
                    chunk_id: "c1".into(),
                    doc_id: "d1".into(),
                    mention_count: 1,
                    score: 0.4,
                },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let for_a = chunks_for_entity(&rtx, "ent_a").unwrap();
        assert_eq!(for_a.len(), 2);
        let for_b = chunks_for_entity(&rtx, "ent_b").unwrap();
        assert_eq!(for_b.len(), 1);
    }
}
