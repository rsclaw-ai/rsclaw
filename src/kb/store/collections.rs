//! KbCollection accessors. Mirrors `docs.rs`: free functions over a
//! `&WriteTransaction` / `&ReadTransaction`. Maintains a secondary
//! name→id index (`KB_COLLECTION_BY_NAME`) so the API can enforce unique
//! collection names and look up by name.

use crate::kb::model::KbCollection;
use crate::kb::store::codec::{decode, encode};
use crate::kb::store::schema::{KB_COLLECTIONS, KB_COLLECTION_BY_NAME};
use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

/// Insert or replace a collection and (re)index its name. On rename the
/// caller must `unindex_name` the previous name first — `put` only writes the
/// current name, so a stale old-name index would otherwise linger.
pub fn put(wtx: &WriteTransaction, c: &KbCollection) -> Result<()> {
    let bytes = encode(c)?;
    wtx.open_table(KB_COLLECTIONS)?
        .insert(c.id.as_str(), bytes.as_slice())?;
    wtx.open_table(KB_COLLECTION_BY_NAME)?
        .insert(c.name.as_str(), c.id.as_str())?;
    Ok(())
}

pub fn get(rtx: &ReadTransaction, id: &str) -> Result<Option<KbCollection>> {
    let tbl = rtx.open_table(KB_COLLECTIONS)?;
    match tbl.get(id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

/// All collections, in arbitrary (key) order. Callers sort for display.
pub fn list(rtx: &ReadTransaction) -> Result<Vec<KbCollection>> {
    let tbl = rtx.open_table(KB_COLLECTIONS)?;
    let mut out = Vec::new();
    for item in tbl.iter()? {
        let (_k, v) = item?;
        out.push(decode(v.value())?);
    }
    Ok(out)
}

/// The id of the collection with this exact name, if any. Used to reject
/// duplicate names on create.
pub fn find_by_name(rtx: &ReadTransaction, name: &str) -> Result<Option<String>> {
    let tbl = rtx.open_table(KB_COLLECTION_BY_NAME)?;
    match tbl.get(name)? {
        Some(v) => Ok(Some(v.value().to_string())),
        None => Ok(None),
    }
}

/// Remove the name→id index entry (used when renaming).
pub fn unindex_name(wtx: &WriteTransaction, name: &str) -> Result<()> {
    wtx.open_table(KB_COLLECTION_BY_NAME)?.remove(name)?;
    Ok(())
}

/// Delete a collection and its name index. Returns whether it existed. Does
/// NOT touch the collection's docs — the caller removes those by tag.
pub fn delete(wtx: &WriteTransaction, id: &str) -> Result<bool> {
    let name = {
        let tbl = wtx.open_table(KB_COLLECTIONS)?;
        match tbl.get(id)? {
            Some(v) => decode::<KbCollection>(v.value())?.name,
            None => return Ok(false),
        }
    };
    wtx.open_table(KB_COLLECTIONS)?.remove(id)?;
    wtx.open_table(KB_COLLECTION_BY_NAME)?.remove(name.as_str())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase;
    use crate::kb::store::schema::open_db;
    use tempfile::TempDir;

    fn col(id: &str, name: &str) -> KbCollection {
        KbCollection {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            embed_model: Some("Qwen3-Embedding-0.6B".into()),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn put_get_list_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &col("c1", "产品手册")).unwrap();
            put(&wtx, &col("c2", "法律文档")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(get(&rtx, "c1").unwrap().unwrap().name, "产品手册");
        assert_eq!(list(&rtx).unwrap().len(), 2);
    }

    #[test]
    fn find_by_name_for_uniqueness() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &col("c1", "产品手册")).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(find_by_name(&rtx, "产品手册").unwrap().as_deref(), Some("c1"));
        assert!(find_by_name(&rtx, "不存在").unwrap().is_none());
    }

    #[test]
    fn delete_removes_row_and_name_index() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        {
            let wtx = db.begin_write().unwrap();
            put(&wtx, &col("c1", "产品手册")).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            assert!(delete(&wtx, "c1").unwrap());
            assert!(!delete(&wtx, "c1").unwrap()); // idempotent
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert!(get(&rtx, "c1").unwrap().is_none());
        assert!(find_by_name(&rtx, "产品手册").unwrap().is_none());
    }
}
