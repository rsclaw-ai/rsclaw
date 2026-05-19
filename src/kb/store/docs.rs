//! KbDoc accessors. Free functions that take a `&WriteTransaction`
//! (writes) or `&ReadTransaction` (reads) so the ingest pipeline can
//! compose multiple table writes in one transaction.

use crate::kb::model::{KbDoc, VersionPointer};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_DOCS, KB_DOC_LATEST_VERSION};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

/// Insert or replace a doc. The caller is responsible for setting
/// `version` correctly (use `next_version_for` before calling).
pub fn put(wtx: &WriteTransaction, doc: &KbDoc) -> Result<()> {
    let bytes = encode(doc)?;
    let mut tbl = wtx.open_table(KB_DOCS)?;
    tbl.insert(doc.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get(rtx: &ReadTransaction, doc_id: &str) -> Result<Option<KbDoc>> {
    let tbl = rtx.open_table(KB_DOCS)?;
    match tbl.get(doc_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Set the active-version pointer for a logical source. Called inside
/// the same tx as `put` so re-ingest's version bump is atomic.
pub fn set_latest_version(
    wtx: &WriteTransaction,
    logical_source_id: &str,
    pointer: &VersionPointer,
) -> Result<()> {
    let bytes = encode(pointer)?;
    let mut tbl = wtx.open_table(KB_DOC_LATEST_VERSION)?;
    tbl.insert(logical_source_id, bytes.as_slice())?;
    Ok(())
}

pub fn latest_version(
    rtx: &ReadTransaction,
    logical_source_id: &str,
) -> Result<Option<VersionPointer>> {
    let tbl = rtx.open_table(KB_DOC_LATEST_VERSION)?;
    match tbl.get(logical_source_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Compute the next `KbDoc.version` for a logical source. Returns 1
/// for first ingest, `prev.version + 1` for re-ingest. Reads only.
pub fn next_version_for(rtx: &ReadTransaction, logical_source_id: &str) -> Result<u32> {
    Ok(latest_version(rtx, logical_source_id)?
        .map(|p| p.version + 1)
        .unwrap_or(1))
}

/// Find the latest doc for a logical_source_id whose `raw_sha256`
/// matches `expected_raw_sha`. Used by the NOOP short-circuit in the
/// ingest pipeline. Returns `None` if no match (caller proceeds with
/// fresh ingest).
pub fn find_by_logical_and_hash(
    rtx: &ReadTransaction,
    logical_source_id: &str,
    expected_raw_sha: &str,
) -> Result<Option<String>> {
    let Some(ptr) = latest_version(rtx, logical_source_id)? else {
        return Ok(None);
    };
    let Some(doc) = get(rtx, &ptr.doc_id)? else {
        return Ok(None);
    };
    if doc.raw_sha256 == expected_raw_sha {
        Ok(Some(doc.id))
    } else {
        Ok(None)
    }
}

// --- WriteTransaction read variants ---
// Mirror the rtx readers but use `wtx.open_table()` so the ingest
// pipeline can do its NOOP re-check, version compute, and old_paths
// lookup inside the same write transaction (race-safe — redb is
// single-writer once `begin_write()` returns). redb's
// WriteTransaction tables implement `ReadableTable`, so the read
// logic is identical to the rtx variants.

pub fn get_in_wtx(wtx: &WriteTransaction, doc_id: &str) -> Result<Option<KbDoc>> {
    let tbl = wtx.open_table(KB_DOCS)?;
    match tbl.get(doc_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

pub fn latest_version_in_wtx(
    wtx: &WriteTransaction,
    logical_source_id: &str,
) -> Result<Option<VersionPointer>> {
    let tbl = wtx.open_table(KB_DOC_LATEST_VERSION)?;
    match tbl.get(logical_source_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

pub fn next_version_for_in_wtx(
    wtx: &WriteTransaction,
    logical_source_id: &str,
) -> Result<u32> {
    Ok(latest_version_in_wtx(wtx, logical_source_id)?
        .map(|p| p.version + 1)
        .unwrap_or(1))
}

pub fn find_by_logical_and_hash_in_wtx(
    wtx: &WriteTransaction,
    logical_source_id: &str,
    expected_raw_sha: &str,
) -> Result<Option<String>> {
    let Some(ptr) = latest_version_in_wtx(wtx, logical_source_id)? else {
        return Ok(None);
    };
    let Some(doc) = get_in_wtx(wtx, &ptr.doc_id)? else {
        return Ok(None);
    };
    if doc.raw_sha256 == expected_raw_sha {
        Ok(Some(doc.id))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{KbSource, KbSourceKind, KbStatus, KbVisibility};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample(id: &str, lsid: &str, raw_sha: &str, version: u32) -> KbDoc {
        KbDoc {
            id: id.into(),
            logical_source_id: lsid.into(),
            source: KbSource::Doc { path: "/x".into() },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: raw_sha.into(),
            markdown_path: "md/doc/x--12345678.md".into(),
            markdown_sha256: "md".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0,
            updated_at: 0,
            version,
            status: KbStatus::Active,
            visibility: KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn put_then_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("d1", "lsid1", "rawA", 1)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let got = get(&rtx, "d1").unwrap().unwrap();
        assert_eq!(got.raw_sha256, "rawA");
    }

    #[test]
    fn missing_doc_returns_none() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        assert!(get(&rtx, "nope").unwrap().is_none());
    }

    #[test]
    fn next_version_starts_at_1() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        assert_eq!(next_version_for(&rtx, "fresh-lsid").unwrap(), 1);
    }

    #[test]
    fn next_version_increments_after_pointer_set() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            set_latest_version(
                &wtx,
                "lsid",
                &VersionPointer { doc_id: "d1".into(), version: 1 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(next_version_for(&rtx, "lsid").unwrap(), 2);
    }

    #[test]
    fn find_by_logical_and_hash_matches() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("d1", "lsid", "rawA", 1)).unwrap();
            set_latest_version(
                &wtx,
                "lsid",
                &VersionPointer { doc_id: "d1".into(), version: 1 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(
            find_by_logical_and_hash(&rtx, "lsid", "rawA").unwrap().as_deref(),
            Some("d1")
        );
        assert!(find_by_logical_and_hash(&rtx, "lsid", "rawB").unwrap().is_none());
        assert!(find_by_logical_and_hash(&rtx, "other", "rawA").unwrap().is_none());
    }

    #[test]
    fn wtx_read_variants_see_committed_state() {
        // The `_in_wtx` accessors are the race-safety hinge for the
        // ingest NOOP re-check. They must observe data committed by a
        // prior wtx exactly the same way an rtx does.
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("d1", "lsid", "rawA", 1)).unwrap();
            set_latest_version(
                &wtx,
                "lsid",
                &VersionPointer { doc_id: "d1".into(), version: 1 },
            )
            .unwrap();
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        assert_eq!(get_in_wtx(&wtx, "d1").unwrap().unwrap().raw_sha256, "rawA");
        assert_eq!(next_version_for_in_wtx(&wtx, "lsid").unwrap(), 2);
        assert_eq!(
            find_by_logical_and_hash_in_wtx(&wtx, "lsid", "rawA").unwrap().as_deref(),
            Some("d1"),
        );
        assert!(find_by_logical_and_hash_in_wtx(&wtx, "lsid", "rawB").unwrap().is_none());
    }
}
