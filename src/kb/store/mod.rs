//! redb-backed KB store. `KbStore` owns the database handle; all
//! reads/writes go through tx accessors defined in the submodules.

pub mod schema;
pub mod codec;
pub mod docs;
pub mod chunks;
pub mod seen;
pub mod ledger;
pub mod jobs;

pub use schema::open_db;
pub use seen::{SeenRecord, SyncState};

use anyhow::Result;
use redb::{Database, ReadTransaction, WriteTransaction};
use std::path::Path;

pub struct KbStore {
    pub db: Database,
}

impl KbStore {
    pub fn open(path: &Path) -> Result<Self> {
        let db = open_db(path)?;
        Ok(Self { db })
    }

    pub fn begin_write(&self) -> Result<WriteTransaction> {
        Ok(self.db.begin_write()?)
    }

    pub fn begin_read(&self) -> Result<ReadTransaction> {
        Ok(self.db.begin_read()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_db() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let _rtx = store.begin_read().unwrap();
    }

    #[test]
    fn begin_write_returns_usable_tx() {
        let tmp = TempDir::new().unwrap();
        let store = KbStore::open(&tmp.path().join("kb.redb")).unwrap();
        let wtx = store.begin_write().unwrap();
        wtx.commit().unwrap();
    }
}
