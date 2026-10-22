//! KbCollection — metadata for a user-facing "collection" (知识库/合集).
//!
//! Collections are a thin veneer over the single KB store: a collection is a
//! reserved-prefix tag (`collection:<id>`) on the docs that belong to it, plus
//! this metadata row carrying the name/description/timestamps the desktop UI
//! needs. There is no per-collection store or embedder — see the project note
//! `kb-desktop-collections`.

use serde::{Deserialize, Serialize};

/// Tag prefix that marks a doc's collection membership. A doc in collection
/// `col_ab12` carries the tag `collection:col_ab12`.
pub const COLLECTION_TAG_PREFIX: &str = "collection:";

/// The membership tag for a collection id.
pub fn collection_tag(id: &str) -> String {
    format!("{COLLECTION_TAG_PREFIX}{id}")
}

/// Collection-id prefix of a subscribed knowledge pack. Pack `parenting` is
/// the collection `pack_parenting`; its docs come from the pack's server and
/// are read-only here.
pub const PACK_COLLECTION_PREFIX: &str = "pack_";

/// The local collection id of knowledge pack `pack_id`.
pub fn pack_collection_id(pack_id: &str) -> String {
    format!("{PACK_COLLECTION_PREFIX}{pack_id}")
}

/// Whether `collection_id` holds a subscribed knowledge pack.
pub fn is_pack_collection(collection_id: &str) -> bool {
    collection_id.starts_with(PACK_COLLECTION_PREFIX)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KbCollection {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Embedder model label for display. v1 uses one embedder across all
    /// collections; this records what was configured at creation.
    pub embed_model: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
