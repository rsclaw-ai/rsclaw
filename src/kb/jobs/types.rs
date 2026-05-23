//! Job-queue types backing the redb-native jobs queue. The queue
//! itself (workers, claim/extend/release) lands in Week 2; this
//! module only carries the types so Week 1 can compile-check
//! references and serde shape.
//!
//! See spec §J jobs queue.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Chunk + embed + index a doc whose KbDoc and markdown are
    /// already in place. Week 2 worker consumes this.
    ChunkAndEmbed { doc_id: String, doc_version: u32 },
    /// Rebuild HNSW cache from redb. Background trigger.
    RebuildHnsw,
    /// Compactor work item.
    RunCompactor,
}

impl JobKind {
    /// Dedupe key for the `jobs_by_dedupe_active` index. Same logical
    /// work in flight collapses to one job.
    pub fn dedupe_key(&self) -> String {
        match self {
            Self::ChunkAndEmbed {
                doc_id,
                doc_version,
            } => {
                format!("chunk_embed:{doc_id}:{doc_version}")
            }
            Self::RebuildHnsw => "rebuild_hnsw:singleton".into(),
            Self::RunCompactor => "run_compactor:singleton".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Ready,
    Running,
    Done,
    Failed,
}

impl JobStatus {
    pub fn as_byte(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Running => 1,
            Self::Done => 2,
            Self::Failed => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String, // ulid
    pub kind: JobKind,
    pub status: JobStatus,
    pub priority: u8, // 0=highest, 255=lowest
    pub created_at: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl Job {
    pub fn new(kind: JobKind) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            kind,
            status: JobStatus::Ready,
            priority: 128,
            created_at: chrono::Utc::now().timestamp_millis(),
            attempts: 0,
            last_error: None,
        }
    }
}

/// Composite key for the `kb_jobs_by_status_priority` index. Order is
/// `(status_byte, priority, created_at_be, job_id)` so a range scan
/// with `status=Ready` prefix returns highest-priority-oldest first.
pub fn status_priority_key(j: &Job) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 1 + 8 + j.id.len());
    k.push(j.status.as_byte());
    k.push(j.priority);
    k.extend_from_slice(&j.created_at.to_be_bytes());
    k.extend_from_slice(j.id.as_bytes());
    k
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimToken {
    pub worker_id: String,
    pub claimed_at: i64,
    pub expires_at: i64,
    pub token: String, // random ulid; mark_done verifies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_key_per_kind() {
        let j1 = JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        };
        let j2 = JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 2,
        };
        assert_ne!(j1.dedupe_key(), j2.dedupe_key());
        assert_eq!(JobKind::RebuildHnsw.dedupe_key(), "rebuild_hnsw:singleton");
        assert_eq!(
            JobKind::RunCompactor.dedupe_key(),
            "run_compactor:singleton"
        );
    }

    #[test]
    fn status_priority_key_orders_correctly() {
        let mut j1 = Job::new(JobKind::RebuildHnsw);
        j1.priority = 50;
        let mut j2 = Job::new(JobKind::RunCompactor);
        j2.priority = 100;
        let k1 = status_priority_key(&j1);
        let k2 = status_priority_key(&j2);
        assert!(k1 < k2, "lower priority byte sorts first");
    }

    #[test]
    fn status_byte_values_stable() {
        assert_eq!(JobStatus::Ready.as_byte(), 0);
        assert_eq!(JobStatus::Running.as_byte(), 1);
        assert_eq!(JobStatus::Done.as_byte(), 2);
        assert_eq!(JobStatus::Failed.as_byte(), 3);
    }

    #[test]
    fn job_new_defaults() {
        let j = Job::new(JobKind::RebuildHnsw);
        assert_eq!(j.status, JobStatus::Ready);
        assert_eq!(j.priority, 128);
        assert_eq!(j.attempts, 0);
        assert!(j.last_error.is_none());
        assert!(!j.id.is_empty());
    }

    #[test]
    fn serde_roundtrip() {
        let j = Job::new(JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 2,
        });
        let s = serde_json::to_string(&j).unwrap();
        assert_eq!(serde_json::from_str::<Job>(&s).unwrap(), j);
    }
}
