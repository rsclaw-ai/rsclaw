//! SeenItems + SyncState accessors. See spec §S.

use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_SEEN_ITEMS, KB_SYNC_STATE};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SeenRecord {
    pub raw_sha256: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    pub cursor: String,
    pub last_sync_at: i64,
}

const SEP: char = '\0';

pub fn mark_seen(
    wtx: &WriteTransaction,
    source_id: &str,
    item_id: &str,
    raw_sha256: &str,
    now_ms: i64,
) -> Result<()> {
    let key = compose_seen_key(source_id, item_id);
    let mut tbl = wtx.open_table(KB_SEEN_ITEMS)?;
    // Decode existing inside a scope so the AccessGuard is dropped
    // before the mutable `insert` borrow below.
    let existing: Option<SeenRecord> = match tbl.get(key.as_str())? {
        Some(v) => Some(decode(v.value())?),
        None => None,
    };
    let rec = match existing {
        Some(mut r) => {
            r.last_seen_at = now_ms;
            r.raw_sha256 = raw_sha256.into();
            r
        }
        None => SeenRecord {
            raw_sha256: raw_sha256.into(),
            first_seen_at: now_ms,
            last_seen_at: now_ms,
        },
    };
    let bytes = encode(&rec)?;
    tbl.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn is_seen(
    rtx: &ReadTransaction,
    source_id: &str,
    item_id: &str,
) -> Result<Option<SeenRecord>> {
    let key = compose_seen_key(source_id, item_id);
    let tbl = rtx.open_table(KB_SEEN_ITEMS)?;
    match tbl.get(key.as_str())? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

fn compose_seen_key(source_id: &str, item_id: &str) -> String {
    format!("{source_id}{SEP}{item_id}")
}

pub fn put_sync_state(
    wtx: &WriteTransaction,
    source_id: &str,
    state: &SyncState,
) -> Result<()> {
    let bytes = encode(state)?;
    let mut tbl = wtx.open_table(KB_SYNC_STATE)?;
    tbl.insert(source_id, bytes.as_slice())?;
    Ok(())
}

pub fn get_sync_state(rtx: &ReadTransaction, source_id: &str) -> Result<Option<SyncState>> {
    let tbl = rtx.open_table(KB_SYNC_STATE)?;
    match tbl.get(source_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    #[test]
    fn mark_then_query() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-A", 1000).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let r = is_seen(&rtx, "src1", "item1").unwrap().unwrap();
        assert_eq!(r.raw_sha256, "sha-A");
        assert_eq!(r.first_seen_at, 1000);
        assert_eq!(r.last_seen_at, 1000);
    }

    #[test]
    fn mark_twice_updates_last_seen() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-A", 1000).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "item1", "sha-B", 2000).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let r = is_seen(&rtx, "src1", "item1").unwrap().unwrap();
        assert_eq!(r.first_seen_at, 1000);
        assert_eq!(r.last_seen_at, 2000);
        assert_eq!(r.raw_sha256, "sha-B");
    }

    #[test]
    fn keys_isolate_by_source() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            mark_seen(&wtx, "src1", "x", "a", 1).unwrap();
            mark_seen(&wtx, "src2", "x", "b", 1).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(is_seen(&rtx, "src1", "x").unwrap().unwrap().raw_sha256, "a");
        assert_eq!(is_seen(&rtx, "src2", "x").unwrap().unwrap().raw_sha256, "b");
    }

    #[test]
    fn sync_state_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put_sync_state(
                &wtx,
                "src1",
                &SyncState { cursor: "etag:abc".into(), last_sync_at: 123 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let s = get_sync_state(&rtx, "src1").unwrap().unwrap();
        assert_eq!(s.cursor, "etag:abc");
        assert_eq!(s.last_sync_at, 123);
    }
}
