//! IngestLedger + Outbox types. The ledger records every doc
//! creation/update/delete so a crash mid-pipeline can be resumed
//! deterministically. See spec §J.
//!
//! Week 1 only ships the types. Week 2 adds the accessors
//! (`ledger::append`, `ledger::mark_complete`, etc.) and wires the
//! outbox into the ingest pipeline.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerOp {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerStatus {
    Pending,
    IndexingComplete,
    CleanupPending,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IngestLedgerEntry {
    pub id: String, // ulid
    pub created_at: i64,
    pub updated_at: i64,
    pub doc_id: String,
    pub logical_source_id: String,
    pub op: LedgerOp,
    pub new_paths: Vec<String>, // relative to kb_root
    pub old_paths: Vec<String>,
    pub status: LedgerStatus,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let e = IngestLedgerEntry {
            id: "L1".into(),
            created_at: 0,
            updated_at: 0,
            doc_id: "D1".into(),
            logical_source_id: "file:sha256:abc".into(),
            op: LedgerOp::Create,
            new_paths: vec!["md/doc/x--12345678.md".into()],
            old_paths: vec![],
            status: LedgerStatus::Pending,
            error: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<IngestLedgerEntry>(&s).unwrap(), e);
    }

    #[test]
    fn status_lifecycle_serde() {
        for st in [
            LedgerStatus::Pending,
            LedgerStatus::IndexingComplete,
            LedgerStatus::CleanupPending,
            LedgerStatus::Done,
            LedgerStatus::Failed,
        ] {
            let s = serde_json::to_string(&st).unwrap();
            assert_eq!(serde_json::from_str::<LedgerStatus>(&s).unwrap(), st);
        }
    }
}
