//! SyncRegistry: per-source state load/save. Reuses
//! `store::seen::SyncState` for persistence.

use crate::kb::store::seen::{get_sync_state, put_sync_state, SyncState};
use crate::kb::store::KbStore;
use anyhow::Result;

pub struct SyncRegistry;

impl SyncRegistry {
    pub fn load(store: &KbStore, source_id: &str) -> Result<Option<SyncState>> {
        let rtx = store.begin_read()?;
        get_sync_state(&rtx, source_id)
    }

    pub fn save(store: &KbStore, source_id: &str, state: &SyncState) -> Result<()> {
        let wtx = store.begin_write()?;
        put_sync_state(&wtx, source_id, state)?;
        wtx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_returns_none_for_missing() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        assert!(SyncRegistry::load(&store, "nope").unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let state = SyncState { cursor: "etag:abc".into(), last_sync_at: 123 };
        SyncRegistry::save(&store, "src1", &state).unwrap();
        let got = SyncRegistry::load(&store, "src1").unwrap().unwrap();
        assert_eq!(got.cursor, "etag:abc");
        assert_eq!(got.last_sync_at, 123);
    }
}
