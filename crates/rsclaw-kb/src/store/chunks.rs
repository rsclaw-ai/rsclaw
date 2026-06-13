//! KbChunk accessors + secondary index by logical_source_id.

use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::kb::{
    model::KbChunk,
    store::{
        codec::{decode, encode},
        schema::{KB_CHUNK_BY_LOGICAL, KB_CHUNKS},
    },
};

const SEP: u8 = 0;

/// Insert or replace a chunk, updating the by_logical secondary index
/// in the same tx so callers don't have to remember.
pub fn put(wtx: &WriteTransaction, chunk: &KbChunk) -> Result<()> {
    let bytes = encode(chunk)?;
    {
        let mut tbl = wtx.open_table(KB_CHUNKS)?;
        tbl.insert(chunk.id.as_str(), bytes.as_slice())?;
    }
    {
        let mut idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
        let key = compose_logical_key(&chunk.logical_source_id, &chunk.id);
        idx.insert(key.as_str(), b"".as_slice())?;
    }
    Ok(())
}

pub fn get(rtx: &ReadTransaction, chunk_id: &str) -> Result<Option<KbChunk>> {
    let tbl = rtx.open_table(KB_CHUNKS)?;
    match tbl.get(chunk_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// Return all `chunk_id`s for a logical source, in `chunk_id` order.
pub fn chunk_ids_for_logical(
    rtx: &ReadTransaction,
    logical_source_id: &str,
) -> Result<Vec<String>> {
    let prefix = format!("{logical_source_id}\0");
    let end = format!("{logical_source_id}\u{1}"); // 0x00 + 1 = 0x01
    let idx = rtx.open_table(KB_CHUNK_BY_LOGICAL)?;
    let mut out = Vec::new();
    for entry in idx.range(prefix.as_str()..end.as_str())? {
        let (k, _) = entry?;
        let key = k.value();
        // key = "{lsid}\0{chunk_id}"
        if let Some(pos) = key.bytes().position(|b| b == SEP) {
            out.push(key[pos + 1..].to_string());
        }
    }
    Ok(out)
}

/// All chunks for a logical source, materialised. Convenience for tests
/// + small docs; for large docs prefer `chunk_ids_for_logical` + per-id
/// `get` so the caller can stream.
pub fn chunks_for_logical(rtx: &ReadTransaction, logical_source_id: &str) -> Result<Vec<KbChunk>> {
    let ids = chunk_ids_for_logical(rtx, logical_source_id)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(c) = get(rtx, &id)? {
            out.push(c);
        }
    }
    Ok(out)
}

fn compose_logical_key(logical_source_id: &str, chunk_id: &str) -> String {
    format!("{logical_source_id}\0{chunk_id}")
}

/// Remove all chunks for `logical_source_id` whose `doc_version <
/// keep_from_version`. Used by the ChunkAndEmbed handler when a new
/// doc version supersedes the prior one — same-version re-runs are
/// idempotent because chunk_ids are deterministic, but a v(N-1)→vN
/// transition would otherwise leak vN-1's chunks. Returns the count
/// of chunks removed. Runs inside the caller's wtx so cleanup +
/// new-version inserts are atomic.
pub fn delete_for_doc_version_below(
    wtx: &WriteTransaction,
    logical_source_id: &str,
    keep_from_version: u32,
) -> Result<usize> {
    let ids = chunk_ids_for_logical_in_wtx(wtx, logical_source_id)?;
    let mut to_remove = Vec::new();
    {
        let tbl = wtx.open_table(KB_CHUNKS)?;
        for id in &ids {
            if let Some(v) = tbl.get(id.as_str())? {
                let c: KbChunk = decode(v.value())?;
                if c.doc_version < keep_from_version {
                    to_remove.push(id.clone());
                }
            }
        }
    }
    let mut removed = 0;
    {
        let mut tbl = wtx.open_table(KB_CHUNKS)?;
        for id in &to_remove {
            if tbl.remove(id.as_str())?.is_some() {
                removed += 1;
            }
        }
    }
    {
        let mut idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
        for id in &to_remove {
            let key = compose_logical_key(logical_source_id, id);
            idx.remove(key.as_str())?;
        }
    }
    Ok(removed)
}

fn chunk_ids_for_logical_in_wtx(
    wtx: &WriteTransaction,
    logical_source_id: &str,
) -> Result<Vec<String>> {
    let prefix = format!("{logical_source_id}\0");
    let end = format!("{logical_source_id}\u{1}");
    let idx = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
    let mut out = Vec::new();
    for entry in idx.range(prefix.as_str()..end.as_str())? {
        let (k, _) = entry?;
        let key = k.value();
        if let Some(pos) = key.bytes().position(|b| b == SEP) {
            out.push(key[pos + 1..].to_string());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    use super::*;
    use crate::kb::{
        model::{ChunkStatus, KbLocator, LogicalSourceId, chunk_id},
        store::open_db,
    };

    fn sample(lsid: &LogicalSourceId, seq: u32, body: &str) -> KbChunk {
        KbChunk {
            id: chunk_id(lsid, seq, body),
            doc_id: "d1".into(),
            logical_source_id: lsid.0.clone(),
            doc_version: 1,
            seq,
            heading_path: vec![],
            byte_offset: (0, body.len() as u64),
            indexed_text: body.into(),
            vector: vec![0.1, 0.2, 0.3],
            simhash: 0,
            locator: KbLocator::Offset {
                start: 0,
                end: body.len(),
            },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: "stub".into(),
        }
    }

    #[test]
    fn put_then_get_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let c = sample(&lsid, 0, "hello");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &c).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(get(&rtx, &c.id).unwrap().unwrap(), c);
    }

    #[test]
    fn chunks_for_logical_returns_in_order() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let chunks = (0..3)
            .map(|i| sample(&lsid, i, &format!("body{i}")))
            .collect::<Vec<_>>();
        {
            let wtx = db.begin_write().unwrap();
            for c in &chunks {
                put(&wtx, c).unwrap();
            }
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let got = chunks_for_logical(&rtx, lsid.as_str()).unwrap();
        assert_eq!(got.len(), 3);
        for c in &got {
            assert_eq!(c.logical_source_id, lsid.0);
        }
    }

    #[test]
    fn chunks_for_logical_isolates_by_source() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let l1 = LogicalSourceId::for_file("abc");
        let l2 = LogicalSourceId::for_file("def");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &sample(&l1, 0, "a")).unwrap();
            put(&wtx, &sample(&l2, 0, "b")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(chunks_for_logical(&rtx, l1.as_str()).unwrap().len(), 1);
        assert_eq!(chunks_for_logical(&rtx, l2.as_str()).unwrap().len(), 1);
    }

    #[test]
    fn put_overwrites_same_chunk_id() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let c = sample(&lsid, 0, "x");
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &c).unwrap();
            put(&wtx, &c).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(chunks_for_logical(&rtx, lsid.as_str()).unwrap().len(), 1);
    }

    #[test]
    fn delete_for_doc_version_below_removes_older_only() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let lsid = LogicalSourceId::for_file("abc");
        let mut v1 = sample(&lsid, 0, "v1-body");
        v1.doc_version = 1;
        let mut v2 = sample(&lsid, 1, "v2-body");
        v2.doc_version = 2;
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &v1).unwrap();
            put(&wtx, &v2).unwrap();
            wtx.commit().unwrap();
        }
        let removed = {
            let wtx = db.begin_write().unwrap();
            let n = delete_for_doc_version_below(&wtx, lsid.as_str(), 2).unwrap();
            wtx.commit().unwrap();
            n
        };
        assert_eq!(removed, 1);
        let rtx = db.begin_read().unwrap();
        let remaining = chunks_for_logical(&rtx, lsid.as_str()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].doc_version, 2);
        let ids = chunk_ids_for_logical(&rtx, lsid.as_str()).unwrap();
        assert_eq!(ids.len(), 1);
    }
}
