//! Entity / mention types. Stored in `kb_entities` (canonical →
//! KbEntity) and `kb_entity_index` (entity → mention edges).
//! Week 1 only ships the types; Week 2 wires extraction + the
//! `kb_search_entities` tool.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Brand,
    Person,
    Org,
    Email,
    Url,
    Hashtag,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbEntity {
    pub canonical_id: String,
    pub surface_forms: Vec<String>,
    pub kind: EntityKind,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbEntityIndex {
    pub entity_id: String,
    pub chunk_id: String,
    pub doc_id: String,
    pub mention_count: u32,
    pub score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_serde_roundtrip() {
        let e = KbEntity {
            canonical_id: "ent_x".into(),
            surface_forms: vec!["X".into(), "Brand X".into()],
            kind: EntityKind::Brand,
            created_at: 0,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<KbEntity>(&s).unwrap(), e);
    }

    #[test]
    fn entity_index_serde_roundtrip() {
        let i = KbEntityIndex {
            entity_id: "ent_x".into(),
            chunk_id: "c1".into(),
            doc_id: "d1".into(),
            mention_count: 3,
            score: 0.75,
        };
        let s = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<KbEntityIndex>(&s).unwrap(), i);
    }
}
