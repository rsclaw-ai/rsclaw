//! IngestLedger accessors. See spec §J.

use crate::kb::ledger::{IngestLedgerEntry, LedgerStatus};
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::KB_LEDGER;
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

pub fn put(wtx: &WriteTransaction, entry: &IngestLedgerEntry) -> Result<()> {
    let bytes = encode(entry)?;
    let mut tbl = wtx.open_table(KB_LEDGER)?;
    tbl.insert(entry.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub fn get(rtx: &ReadTransaction, ledger_id: &str) -> Result<Option<IngestLedgerEntry>> {
    let tbl = rtx.open_table(KB_LEDGER)?;
    match tbl.get(ledger_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Scan the entire ledger table and return entries matching `status`.
/// Week 2: linear scan is fine — ledger is small (1 entry per ingest).
pub fn list_by_status(
    rtx: &ReadTransaction,
    status: LedgerStatus,
) -> Result<Vec<IngestLedgerEntry>> {
    let tbl = rtx.open_table(KB_LEDGER)?;
    let mut out = Vec::new();
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let e: IngestLedgerEntry = decode(v.value())?;
        if e.status == status {
            out.push(e);
        }
    }
    Ok(out)
}

/// Update an existing ledger entry's status + updated_at. Errors if
/// the entry doesn't exist.
pub fn update_status(
    wtx: &WriteTransaction,
    ledger_id: &str,
    new_status: LedgerStatus,
    now_ms: i64,
) -> Result<()> {
    let mut tbl = wtx.open_table(KB_LEDGER)?;
    let mut entry: IngestLedgerEntry = match tbl.get(ledger_id)? {
        Some(v) => decode(v.value())?,
        None => return Err(anyhow::anyhow!("ledger {ledger_id} not found")),
    };
    entry.status = new_status;
    entry.updated_at = now_ms;
    let bytes = encode(&entry)?;
    tbl.insert(ledger_id, bytes.as_slice())?;
    Ok(())
}

/// Find the (single) Pending ledger entry for a given `doc_id`.
pub fn find_pending_by_doc(
    rtx: &ReadTransaction,
    doc_id: &str,
) -> Result<Option<IngestLedgerEntry>> {
    for entry in list_by_status(rtx, LedgerStatus::Pending)? {
        if entry.doc_id == doc_id {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// wtx variant of `find_pending_by_doc`.
pub fn find_pending_by_doc_in_wtx(
    wtx: &WriteTransaction,
    doc_id: &str,
) -> Result<Option<IngestLedgerEntry>> {
    let tbl = wtx.open_table(KB_LEDGER)?;
    for entry in tbl.iter()? {
        let (_, v) = entry?;
        let e: IngestLedgerEntry = decode(v.value())?;
        if e.status == LedgerStatus::Pending && e.doc_id == doc_id {
            return Ok(Some(e));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase;
    use crate::kb::ledger::{LedgerOp, LedgerStatus};
    use crate::kb::store::open_db;
    use tempfile::TempDir;

    fn sample(id: &str, status: LedgerStatus) -> IngestLedgerEntry {
        IngestLedgerEntry {
            id: id.into(),
            created_at: 0,
            updated_at: 0,
            doc_id: "d1".into(),
            logical_source_id: "lsid".into(),
            op: LedgerOp::Create,
            new_paths: vec![],
            old_paths: vec![],
            status,
            error: None,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(
            get(&rtx, "L1").unwrap().unwrap().status,
            LedgerStatus::Pending
        );
    }

    #[test]
    fn list_by_status_filters_correctly() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            put(&wtx, &sample("L2", LedgerStatus::Pending)).unwrap();
            put(&wtx, &sample("L3", LedgerStatus::Done)).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let pending = list_by_status(&rtx, LedgerStatus::Pending).unwrap();
        assert_eq!(pending.len(), 2);
        let done = list_by_status(&rtx, LedgerStatus::Done).unwrap();
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn update_status_changes_state() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample("L1", LedgerStatus::Pending)).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            update_status(&wtx, "L1", LedgerStatus::IndexingComplete, 999).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let e = get(&rtx, "L1").unwrap().unwrap();
        assert_eq!(e.status, LedgerStatus::IndexingComplete);
        assert_eq!(e.updated_at, 999);
    }

    #[test]
    fn update_status_errors_when_missing() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let wtx = db.begin_write().unwrap();
        assert!(update_status(&wtx, "nope", LedgerStatus::Done, 0).is_err());
    }

    #[test]
    fn find_pending_by_doc_returns_only_pending() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            let mut p = sample("L1", LedgerStatus::Pending);
            p.doc_id = "doc-a".into();
            put(&wtx, &p).unwrap();
            let mut d = sample("L2", LedgerStatus::IndexingComplete);
            d.doc_id = "doc-b".into();
            put(&wtx, &d).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(find_pending_by_doc(&rtx, "doc-a").unwrap().unwrap().id, "L1");
        assert!(find_pending_by_doc(&rtx, "doc-b").unwrap().is_none());
        assert!(find_pending_by_doc(&rtx, "missing").unwrap().is_none());
    }
}
