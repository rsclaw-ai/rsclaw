//! Full rebuild of both index layers from redb. Used at startup,
//! after detecting a corrupt tantivy dir, or on manual admin trigger.

use anyhow::Result;

use super::KbIndex;
use crate::store::KbStore;

pub fn from_redb(idx: &KbIndex, store: &KbStore) -> Result<()> {
    idx.hnsw.rebuild(store)?;
    idx.tantivy.rebuild(store)?;
    Ok(())
}
