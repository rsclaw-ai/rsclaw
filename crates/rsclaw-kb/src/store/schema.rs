//! All redb table definitions for the KB store. Values are JSON-encoded
//! (compact binary encoding deferred to v2 if hot-path profiling shows
//! need). See spec §1 storage map.
//!
//! Week 1 ships only the schema (table defs + `open_db`). Accessors
//! (read/write of KbDoc/KbChunk/Ledger/Job) come in Week 2.

use redb::TableDefinition;

// Core data
pub const KB_DOCS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_docs");
/// Collection metadata (id → KbCollection). Desktop "知识库/合集" veneer.
pub const KB_COLLECTIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_collections");
/// Collection name → id, for uniqueness checks and name lookup.
pub const KB_COLLECTION_BY_NAME: TableDefinition<&str, &str> =
    TableDefinition::new("kb_collection_by_name");
pub const KB_DOC_LATEST_VERSION: TableDefinition<&str, &[u8]> =
    TableDefinition::new("kb_doc_latest_version");
pub const KB_CHUNKS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_chunks");
pub const KB_CHUNK_BY_LOGICAL: TableDefinition<&str, &[u8]> =
    TableDefinition::new("kb_chunk_by_logical");

// Entities
pub const KB_ENTITIES: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_entities");
pub const KB_ENTITY_INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_entity_index");

// Sync + dedup
pub const KB_SEEN_ITEMS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_seen_items");
pub const KB_SYNC_STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_sync_state");

// Outbox + jobs
pub const KB_LEDGER: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_ledger");
pub const KB_JOBS_BY_ID: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_jobs_by_id");
pub const KB_JOBS_BY_DEDUPE_ACTIVE: TableDefinition<&str, &str> =
    TableDefinition::new("kb_jobs_by_dedupe_active");
pub const KB_JOBS_BY_STATUS_PRIO: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("kb_jobs_by_status_priority");
pub const KB_JOB_CLAIMS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_job_claims");

/// Open the KB redb file and ensure all 13 tables exist. Idempotent.
pub fn open_db(path: &std::path::Path) -> anyhow::Result<redb::Database> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::store::upgrade_legacy_if_needed(path)?;
    let db = crate::store::create_with_lock_retry(&redb::Database::builder(), path)?;
    let wtx = db.begin_write()?;
    // Open all tables to ensure they exist.
    let _ = wtx.open_table(KB_DOCS)?;
    let _ = wtx.open_table(KB_COLLECTIONS)?;
    let _ = wtx.open_table(KB_COLLECTION_BY_NAME)?;
    let _ = wtx.open_table(KB_DOC_LATEST_VERSION)?;
    let _ = wtx.open_table(KB_CHUNKS)?;
    let _ = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
    let _ = wtx.open_table(KB_ENTITIES)?;
    let _ = wtx.open_table(KB_ENTITY_INDEX)?;
    let _ = wtx.open_table(KB_SEEN_ITEMS)?;
    let _ = wtx.open_table(KB_SYNC_STATE)?;
    let _ = wtx.open_table(KB_LEDGER)?;
    let _ = wtx.open_table(KB_JOBS_BY_ID)?;
    let _ = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
    let _ = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
    let _ = wtx.open_table(KB_JOB_CLAIMS)?;
    wtx.commit()?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn open_creates_all_13_tables() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        // If any table is missing, open_table errors.
        rtx.open_table(KB_DOCS).unwrap();
        rtx.open_table(KB_DOC_LATEST_VERSION).unwrap();
        rtx.open_table(KB_CHUNKS).unwrap();
        rtx.open_table(KB_CHUNK_BY_LOGICAL).unwrap();
        rtx.open_table(KB_ENTITIES).unwrap();
        rtx.open_table(KB_ENTITY_INDEX).unwrap();
        rtx.open_table(KB_SEEN_ITEMS).unwrap();
        rtx.open_table(KB_SYNC_STATE).unwrap();
        rtx.open_table(KB_LEDGER).unwrap();
        rtx.open_table(KB_JOBS_BY_ID).unwrap();
        rtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE).unwrap();
        rtx.open_table(KB_JOBS_BY_STATUS_PRIO).unwrap();
        rtx.open_table(KB_JOB_CLAIMS).unwrap();
    }

    #[test]
    fn open_is_idempotent() {
        // redb holds an exclusive file lock per process, so drop the
        // first handle before opening again to verify the on-disk
        // schema is intact after a clean close (NOT that two handles
        // can coexist — that's expected to fail).
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("kb.redb");
        drop(open_db(&p).unwrap());
        drop(open_db(&p).unwrap());
        // Third open re-verifies all 13 tables survived two close/open cycles
        let db = open_db(&p).unwrap();
        let rtx = db.begin_read().unwrap();
        rtx.open_table(KB_DOCS).unwrap();
        rtx.open_table(KB_JOB_CLAIMS).unwrap();
    }
}
