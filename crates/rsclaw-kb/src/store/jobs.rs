//! Job queue accessors. The queue is redb-native: four tables tracked
//! atomically via single write transactions. See spec §J.
//!
//! Layout of the priority key:
//! `{status_byte}{prio_byte}{created_at_be}{job_id}`. Lower byte values sort
//! first → Ready=0 before Running=1; lower priority byte = higher actual
//! priority. created_at big-endian makes older jobs sort before newer at same
//! priority. job_id disambiguates the tail.

use anyhow::Result;
use redb::{ReadTransaction, ReadableTable, WriteTransaction};

use crate::{
    jobs::{ClaimToken, Job, JobStatus, status_priority_key},
    store::{
        codec::{decode, encode},
        schema::{KB_JOB_CLAIMS, KB_JOBS_BY_DEDUPE_ACTIVE, KB_JOBS_BY_ID, KB_JOBS_BY_STATUS_PRIO},
    },
};

/// Enqueue a new job. Idempotent on `dedupe_key`.
pub fn enqueue(wtx: &WriteTransaction, job: &Job) -> Result<String> {
    let dedupe_key = job.kind.dedupe_key();
    {
        let dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        if let Some(existing) = dedupe.get(dedupe_key.as_str())? {
            return Ok(existing.value().to_string());
        }
    }
    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        let bytes = encode(job)?;
        by_id.insert(job.id.as_str(), bytes.as_slice())?;
    }
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.insert(dedupe_key.as_str(), job.id.as_str())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.insert(status_priority_key(job).as_slice(), b"".as_slice())?;
    }
    Ok(job.id.clone())
}

/// Find the highest-priority Ready job and atomically claim it.
pub fn claim_next(
    wtx: &WriteTransaction,
    worker_id: &str,
    now_ms: i64,
    claim_ttl_ms: i64,
) -> Result<Option<(Job, ClaimToken)>> {
    let lo: &[u8] = &[JobStatus::Ready.as_byte()];
    let hi: &[u8] = &[JobStatus::Ready.as_byte() + 1];
    let old_key = {
        let prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        let mut iter = prio.range::<&[u8]>(lo..hi)?;
        let Some(first) = iter.next() else {
            return Ok(None);
        };
        let (k, _) = first?;
        k.value().to_vec()
    };

    let job_id = job_id_from_priority_key(&old_key);

    let job: Job = {
        let by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        let v = by_id
            .get(job_id.as_str())?
            .ok_or_else(|| anyhow::anyhow!("priority index points at missing job {job_id}"))?;
        decode(v.value())?
    };

    let mut new_job = job.clone();
    new_job.status = JobStatus::Running;

    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        by_id.insert(new_job.id.as_str(), encode(&new_job)?.as_slice())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.remove(old_key.as_slice())?;
        prio.insert(status_priority_key(&new_job).as_slice(), b"".as_slice())?;
    }
    let token = ClaimToken {
        worker_id: worker_id.into(),
        claimed_at: now_ms,
        expires_at: now_ms + claim_ttl_ms,
        token: ulid::Ulid::new().to_string(),
    };
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.insert(new_job.id.as_str(), encode(&token)?.as_slice())?;
    }

    Ok(Some((new_job, token)))
}

/// Mark a job Done. Verifies `token` matches the active ClaimToken.
pub fn mark_done(wtx: &WriteTransaction, job_id: &str, token: &str) -> Result<()> {
    verify_claim_token(wtx, job_id, token)?;
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    let dedupe_key = job.kind.dedupe_key();
    job.status = JobStatus::Done;
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.remove(dedupe_key.as_str())?;
    }
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

/// Mark a job Failed. Same fencing semantics as `mark_done`.
pub fn mark_failed(wtx: &WriteTransaction, job_id: &str, token: &str, error: &str) -> Result<()> {
    verify_claim_token(wtx, job_id, token)?;
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    let dedupe_key = job.kind.dedupe_key();
    job.status = JobStatus::Failed;
    job.attempts += 1;
    job.last_error = Some(error.into());
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
        dedupe.remove(dedupe_key.as_str())?;
    }
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

fn verify_claim_token(wtx: &WriteTransaction, job_id: &str, expected: &str) -> Result<()> {
    let claims = wtx.open_table(KB_JOB_CLAIMS)?;
    let Some(v) = claims.get(job_id)? else {
        return Err(anyhow::anyhow!(
            "stale claim: no active claim for job {job_id} (reclaim_stale may have requeued it)"
        ));
    };
    let active: ClaimToken = decode(v.value())?;
    if active.token != expected {
        return Err(anyhow::anyhow!(
            "stale claim: token mismatch for job {job_id} (held by a different worker)"
        ));
    }
    Ok(())
}

/// Reset a job back to Ready.
pub fn requeue(wtx: &WriteTransaction, job_id: &str) -> Result<()> {
    let (mut job, old_key) = read_and_old_key(wtx, job_id)?;
    job.status = JobStatus::Ready;
    job.attempts += 1;
    write_status_transition(wtx, &job, &old_key)?;
    {
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(job_id)?;
    }
    Ok(())
}

/// Find Running jobs whose claim token has expired and recover them.
/// A job under `max_attempts` is reset to Ready (annotated
/// `last_error = "claim_token_expired"`); a job that has exhausted its
/// attempts is marked Failed instead of requeued, so a worker that keeps
/// dying mid-job can't requeue it forever. Either way the claim is
/// released and the audit trail records why. Returns the affected job ids.
pub fn reclaim_stale(
    wtx: &WriteTransaction,
    now_ms: i64,
    max_attempts: u32,
) -> Result<Vec<String>> {
    let mut to_reclaim = Vec::new();
    {
        let claims = wtx.open_table(KB_JOB_CLAIMS)?;
        for entry in claims.iter()? {
            let (k, v) = entry?;
            let token: ClaimToken = decode(v.value())?;
            if token.expires_at < now_ms {
                to_reclaim.push(k.value().to_string());
            }
        }
    }
    for id in &to_reclaim {
        let (mut job, old_key) = read_and_old_key(wtx, id)?;
        job.attempts += 1;
        if job.attempts >= max_attempts {
            job.status = JobStatus::Failed;
            job.last_error = Some("claim_token_expired (max attempts exceeded)".into());
            write_status_transition(wtx, &job, &old_key)?;
            let dedupe_key = job.kind.dedupe_key();
            let mut dedupe = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
            dedupe.remove(dedupe_key.as_str())?;
        } else {
            job.status = JobStatus::Ready;
            job.last_error = Some("claim_token_expired".into());
            write_status_transition(wtx, &job, &old_key)?;
        }
        let mut claims = wtx.open_table(KB_JOB_CLAIMS)?;
        claims.remove(id.as_str())?;
    }
    Ok(to_reclaim)
}

pub fn get(rtx: &ReadTransaction, job_id: &str) -> Result<Option<Job>> {
    let by_id = rtx.open_table(KB_JOBS_BY_ID)?;
    match by_id.get(job_id)? {
        Some(v) => Ok(Some(decode(v.value())?)),
        None => Ok(None),
    }
}

pub fn list_by_status(rtx: &ReadTransaction, status: JobStatus) -> Result<Vec<Job>> {
    let lo: &[u8] = &[status.as_byte()];
    let hi: &[u8] = &[status.as_byte() + 1];
    let prio = rtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
    let by_id = rtx.open_table(KB_JOBS_BY_ID)?;
    let mut out = Vec::new();
    for entry in prio.range::<&[u8]>(lo..hi)? {
        let (k, _) = entry?;
        let id = job_id_from_priority_key(k.value());
        if let Some(v) = by_id.get(id.as_str())? {
            out.push(decode(v.value())?);
        }
    }
    Ok(out)
}

// ----- internals -----

fn read_and_old_key(wtx: &WriteTransaction, job_id: &str) -> Result<(Job, Vec<u8>)> {
    let by_id = wtx.open_table(KB_JOBS_BY_ID)?;
    let v = by_id
        .get(job_id)?
        .ok_or_else(|| anyhow::anyhow!("job {job_id} not found"))?;
    let job: Job = decode(v.value())?;
    let old_key = status_priority_key(&job);
    Ok((job, old_key))
}

fn write_status_transition(wtx: &WriteTransaction, new_job: &Job, old_key: &[u8]) -> Result<()> {
    {
        let mut by_id = wtx.open_table(KB_JOBS_BY_ID)?;
        by_id.insert(new_job.id.as_str(), encode(new_job)?.as_slice())?;
    }
    {
        let mut prio = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
        prio.remove(old_key)?;
        prio.insert(status_priority_key(new_job).as_slice(), b"".as_slice())?;
    }
    Ok(())
}

fn job_id_from_priority_key(key: &[u8]) -> String {
    // key = status_byte(1) + prio_byte(1) + created_at_be(8) + job_id_bytes
    if key.len() <= 10 {
        return String::new();
    }
    String::from_utf8_lossy(&key[10..]).to_string()
}

#[cfg(test)]
mod tests {
    use redb::ReadableDatabase;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        jobs::{Job, JobKind},
        store::open_db,
    };

    fn fresh() -> (TempDir, redb::Database) {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        (tmp, db)
    }

    #[test]
    fn enqueue_then_claim() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        });
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(enqueue(&wtx, &job).unwrap(), job_id);
            wtx.commit().unwrap();
        }
        let claimed = {
            let wtx = db.begin_write().unwrap();
            let (j, _t) = claim_next(&wtx, "worker-1", 1000, 60_000).unwrap().unwrap();
            wtx.commit().unwrap();
            j
        };
        assert_eq!(claimed.id, job_id);
        assert_eq!(claimed.status, JobStatus::Running);
    }

    #[test]
    fn enqueue_dedupes_active_jobs() {
        let (_tmp, db) = fresh();
        let kind = JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        };
        let j1 = Job::new(kind.clone());
        let j2 = Job::new(kind);
        let id_first = j1.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            let id_a = enqueue(&wtx, &j1).unwrap();
            let id_b = enqueue(&wtx, &j2).unwrap();
            assert_eq!(id_a, id_first);
            assert_eq!(id_b, id_first, "dedupe must return existing job_id");
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(list_by_status(&rtx, JobStatus::Ready).unwrap().len(), 1);
    }

    #[test]
    fn claim_next_returns_none_when_empty() {
        let (_tmp, db) = fresh();
        let wtx = db.begin_write().unwrap();
        assert!(claim_next(&wtx, "w", 0, 60_000).unwrap().is_none());
    }

    #[test]
    fn mark_done_removes_from_dedupe_and_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        let token = {
            let wtx = db.begin_write().unwrap();
            let (_j, t) = claim_next(&wtx, "w", 0, 60_000).unwrap().unwrap();
            wtx.commit().unwrap();
            t.token
        };
        {
            let wtx = db.begin_write().unwrap();
            mark_done(&wtx, &job_id, &token).unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Done);
        let new_job = Job::new(JobKind::RebuildHnsw);
        let new_id = new_job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(enqueue(&wtx, &new_job).unwrap(), new_id);
            wtx.commit().unwrap();
        }
    }

    #[test]
    fn mark_failed_increments_attempts() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RunCompactor);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        let token = {
            let wtx = db.begin_write().unwrap();
            let (_j, t) = claim_next(&wtx, "w", 0, 60_000).unwrap().unwrap();
            wtx.commit().unwrap();
            t.token
        };
        {
            let wtx = db.begin_write().unwrap();
            mark_failed(&wtx, &job_id, &token, "boom").unwrap();
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Failed);
        assert_eq!(j.attempts, 1);
        assert_eq!(j.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn mark_done_with_wrong_token_errors() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        let real_token = {
            let wtx = db.begin_write().unwrap();
            let (_j, t) = claim_next(&wtx, "w1", 0, 60_000).unwrap().unwrap();
            wtx.commit().unwrap();
            t.token
        };
        {
            let wtx = db.begin_write().unwrap();
            assert!(mark_done(&wtx, &job_id, "not-the-real-token").is_err());
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        assert_eq!(
            get(&rtx, &job_id).unwrap().unwrap().status,
            JobStatus::Running
        );
        drop(rtx);
        {
            let wtx = db.begin_write().unwrap();
            mark_done(&wtx, &job_id, &real_token).unwrap();
            wtx.commit().unwrap();
        }
    }

    #[test]
    fn mark_done_after_reclaim_errors() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        let stale_token = {
            let wtx = db.begin_write().unwrap();
            let (_j, t) = claim_next(&wtx, "zombie", 100, 50).unwrap().unwrap();
            wtx.commit().unwrap();
            t.token
        };
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(reclaim_stale(&wtx, 200, 5).unwrap().len(), 1);
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        assert!(mark_done(&wtx, &job_id, &stale_token).is_err());
    }

    #[test]
    fn reclaim_stale_resets_expired_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        {
            let wtx = db.begin_write().unwrap();
            claim_next(&wtx, "w", 100, 50).unwrap();
            wtx.commit().unwrap();
        }
        let reclaimed = {
            let wtx = db.begin_write().unwrap();
            let ids = reclaim_stale(&wtx, 200, 5).unwrap();
            wtx.commit().unwrap();
            ids
        };
        assert_eq!(reclaimed, vec![job_id.clone()]);
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Ready);
        assert_eq!(j.attempts, 1);
        // Audit trail: reclaim must annotate why the job came back.
        assert_eq!(j.last_error.as_deref(), Some("claim_token_expired"));
    }

    #[test]
    fn reclaim_stale_fails_job_past_max_attempts() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        let job_id = job.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            wtx.commit().unwrap();
        }
        // claim with a short TTL so it is immediately stale
        {
            let wtx = db.begin_write().unwrap();
            claim_next(&wtx, "zombie", 100, 50).unwrap();
            wtx.commit().unwrap();
        }
        // max_attempts=1: the first reclaim pushes attempts to 1 == max,
        // so the job is failed out instead of requeued forever.
        {
            let wtx = db.begin_write().unwrap();
            assert_eq!(reclaim_stale(&wtx, 200, 1).unwrap().len(), 1);
            wtx.commit().unwrap();
        }
        let rtx = db.begin_read().unwrap();
        let j = get(&rtx, &job_id).unwrap().unwrap();
        assert_eq!(j.status, JobStatus::Failed);
        assert_eq!(j.attempts, 1);
        assert_eq!(
            j.last_error.as_deref(),
            Some("claim_token_expired (max attempts exceeded)")
        );
        drop(rtx);
        // dedupe key freed → an identical job can be enqueued fresh.
        let again = Job::new(JobKind::RebuildHnsw);
        let again_id = again.id.clone();
        let wtx = db.begin_write().unwrap();
        assert_eq!(enqueue(&wtx, &again).unwrap(), again_id);
    }

    #[test]
    fn reclaim_stale_skips_fresh_claims() {
        let (_tmp, db) = fresh();
        let job = Job::new(JobKind::RebuildHnsw);
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &job).unwrap();
            claim_next(&wtx, "w", 100, 60_000).unwrap();
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        assert!(reclaim_stale(&wtx, 200, 5).unwrap().is_empty());
    }

    #[test]
    fn priority_order_oldest_highest_priority_first() {
        let (_tmp, db) = fresh();
        let mut low = Job::new(JobKind::ChunkAndEmbed {
            doc_id: "d1".into(),
            doc_version: 1,
        });
        low.priority = 200;
        let mut high = Job::new(JobKind::ChunkAndEmbed {
            doc_id: "d2".into(),
            doc_version: 1,
        });
        high.priority = 10;
        let high_id = high.id.clone();
        {
            let wtx = db.begin_write().unwrap();
            enqueue(&wtx, &low).unwrap();
            enqueue(&wtx, &high).unwrap();
            wtx.commit().unwrap();
        }
        let wtx = db.begin_write().unwrap();
        let (j, _) = claim_next(&wtx, "w", 0, 60_000).unwrap().unwrap();
        assert_eq!(j.id, high_id, "lower priority byte must claim first");
    }
}
