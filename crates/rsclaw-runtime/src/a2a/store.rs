//! redb-backed persistence for A2A v1.0 tasks + push notification configs.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use rsclaw_a2a_types::types::{
    A2aArtifact, A2aMessage, A2aTask, AttemptRecord, DurableTaskRecord, OperationRecord,
    PushNotificationConfig, TaskEvent, TaskEventPage, TaskEventReplay, TaskEventState, TaskState,
    WorkLease, WorkReceipt, WorkRecord,
};
use thiserror::Error;

const TASKS: TableDefinition<&str, &str> = TableDefinition::new("a2a_tasks");
/// Push configs keyed by "{task_id}:{config_id}".
const PUSH_CONFIGS: TableDefinition<&str, &str> = TableDefinition::new("a2a_push_configs");
/// Task owner index: task_id -> A2A principal id that created it. Kept out of
/// the `A2aTask` wire type so the owning principal never leaks in responses;
/// used only server-side to enforce per-caller access (A2A spec §7.5).
const TASK_OWNERS: TableDefinition<&str, &str> = TableDefinition::new("a2a_task_owners");

// Durable-execution tables are deliberately separate from the existing A2A task
// projection tables. Nothing in the legacy task path reads or mutates them.
const DURABLE_TASKS: TableDefinition<&str, &str> = TableDefinition::new("a2a_durable_tasks");
const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("a2a_attempts");
const WORKS: TableDefinition<&str, &str> = TableDefinition::new("a2a_works");
const OPERATIONS: TableDefinition<&str, &str> = TableDefinition::new("a2a_operations");
/// Append-only task events keyed by canonical task ID and zero-padded sequence.
const TASK_EVENTS: TableDefinition<&str, &str> = TableDefinition::new("a2a_task_events");
/// Per-task event high-water mark and retained replay floor.
const TASK_EVENT_STATES: TableDefinition<&str, &str> =
    TableDefinition::new("a2a_task_event_states");
/// Consumer cursors keyed by collision-safe task and consumer encoding.
const TASK_EVENT_CURSORS: TableDefinition<&str, &str> =
    TableDefinition::new("a2a_task_event_cursors");
const MAX_EVENT_PAGE: usize = 1000;
const MAX_EVENT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_OPERATION_ACTOR_BYTES: usize = 512;
const MAX_OPERATION_KIND_BYTES: usize = 64;
const SHA256_HEX_BYTES: usize = 64;

fn composite_key(parts: &[&str]) -> String {
    let mut key = String::new();
    for part in parts {
        use std::fmt::Write as _;
        write!(&mut key, "{}:{part}", part.len()).expect("writing to a String cannot fail");
    }
    key
}

fn event_key(task_id: &str, event_seq: u64) -> String {
    format!("{task_id}:{event_seq:020}")
}

fn valid_request_digest(digest: &str) -> bool {
    digest.len() == SHA256_HEX_BYTES
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Errors returned by durable execution persistence operations.
#[derive(Debug, Error)]
pub enum DurableStoreError {
    /// An idempotency key was reused with a different request digest.
    #[error("idempotency conflict")]
    IdempotencyConflict,
    /// Submitted records or cursors violate durable relationships.
    #[error("state conflict: {0}")]
    StateConflict(&'static str),
    /// A mutation used a stale or invalid lease epoch.
    #[error("fenced by lease epoch {current_epoch}")]
    Fenced { current_epoch: u64 },
    /// A receipt or renewal used an expired lease.
    #[error("work lease expired")]
    LeaseExpired,
    /// A consumer cursor decreased or exceeded emitted events.
    #[error("invalid consumer cursor: {0}")]
    CursorConflict(&'static str),
    /// A bounded request exceeded its hard maximum.
    #[error("limit exceeded: {0}")]
    LimitExceeded(&'static str),
    /// Underlying storage or serialization failed.
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
}

fn lease_expiry(
    expires_at: &str,
) -> std::result::Result<chrono::DateTime<chrono::Utc>, DurableStoreError> {
    chrono::DateTime::parse_from_rfc3339(expires_at)
        .map(|value| value.with_timezone(&chrono::Utc))
        .map_err(|_| DurableStoreError::StateConflict("lease expiry is not RFC 3339"))
}

fn ensure_unexpired_lease(expires_at: &str) -> std::result::Result<(), DurableStoreError> {
    if lease_expiry(expires_at)? <= chrono::Utc::now() {
        return Err(DurableStoreError::LeaseExpired);
    }
    Ok(())
}

/// The outcome of atomically admitting an idempotent operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationAdmission {
    /// The operation and execution records were newly persisted.
    Applied(OperationRecord),
    /// A matching prior operation was returned without mutation.
    Existing(OperationRecord),
}

/// redb-backed A2A task and inactive durable-execution foundation store.
pub struct TaskStore {
    db: Database,
}

impl TaskStore {
    /// Opens the authoritative task database, failing closed after one retry.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create a2a store dir")?;
        }
        rsclaw_store::upgrade_legacy_if_needed(path)?;
        // Durable tables make this database authoritative. Retrying protects
        // against transient opens, but every final failure is surfaced unchanged:
        // moving aside or recreating could silently destroy execution history.
        let builder = Database::builder();
        let db = match rsclaw_store::create_with_lock_retry(&builder, path) {
            Ok(db) => db,
            Err(first_err) => {
                tracing::warn!(path = %path.display(), error = %first_err, "a2a task store open failed, retrying");
                std::thread::sleep(std::time::Duration::from_millis(500));
                rsclaw_store::create_with_lock_retry(&builder, path).map_err(|second_err| {
                    anyhow!(
                        "failed to open durable a2a task store at {} after retry: {second_err}",
                        path.display()
                    )
                })?
            }
        };
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(TASKS)?;
            let _ = txn.open_table(PUSH_CONFIGS)?;
            let _ = txn.open_table(TASK_OWNERS)?;
            let _ = txn.open_table(DURABLE_TASKS)?;
            let _ = txn.open_table(ATTEMPTS)?;
            let _ = txn.open_table(WORKS)?;
            let _ = txn.open_table(OPERATIONS)?;
            let _ = txn.open_table(TASK_EVENTS)?;
            let _ = txn.open_table(TASK_EVENT_STATES)?;
            let _ = txn.open_table(TASK_EVENT_CURSORS)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    // -----------------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------------

    /// Atomically create a task and its optional owner if the task ID is
    /// unused.
    ///
    /// Returns `false` without modifying either table when the task or an owner
    /// reservation already exists. This prevents caller-supplied task IDs from
    /// overwriting another principal's task or claiming an orphaned owner
    /// entry.
    pub fn create_task(&self, task: &A2aTask, principal: Option<&str>) -> Result<bool> {
        let json = serde_json::to_string(task)?;
        let txn = self.db.begin_write()?;
        let created = {
            let mut tasks = txn.open_table(TASKS)?;
            let mut owners = txn.open_table(TASK_OWNERS)?;
            let task_exists = tasks.get(task.id.as_str())?.is_some();
            let owner_exists = owners.get(task.id.as_str())?.is_some();
            if task_exists || owner_exists {
                false
            } else {
                tasks.insert(task.id.as_str(), json.as_str())?;
                if let Some(principal) = principal {
                    owners.insert(task.id.as_str(), principal)?;
                }
                true
            }
        };
        if created {
            txn.commit()?;
        }
        Ok(created)
    }

    /// Record the principal that owns `task_id` (A2A §7.5 access control).
    pub fn put_owner(&self, task_id: &str, principal: &str) -> Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASK_OWNERS)?;
            tbl.insert(task_id, principal)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// The principal that owns `task_id`, if recorded. `None` for tasks created
    /// before ownership tracking or while auth was disabled (dev mode).
    pub fn get_owner(&self, task_id: &str) -> Result<Option<String>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASK_OWNERS)?;
        Ok(tbl.get(task_id)?.map(|v| v.value().to_owned()))
    }

    pub fn put(&self, task: &A2aTask) -> Result<()> {
        let json = serde_json::to_string(task)?;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(TASKS)?;
            tbl.insert(task.id.as_str(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<A2aTask>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASKS)?;
        match tbl.get(id)? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    /// Newest-first listing (sorted by id which we use as a UUID — purely a
    /// stable ordering, not a real recency sort; for that we'd need
    /// indexed timestamps). Pagination via offset + limit.
    pub fn list(&self, offset: usize, limit: usize) -> Result<Vec<A2aTask>> {
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(TASKS)?;
        let mut all: Vec<A2aTask> = Vec::new();
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            all.push(serde_json::from_str(v.value())?);
        }
        all.sort_by(|a, b| b.id.cmp(&a.id));
        Ok(all.into_iter().skip(offset).take(limit).collect())
    }

    pub fn set_status(&self, id: &str, state: TaskState) -> Result<()> {
        let mut task = self
            .get(id)?
            .ok_or_else(|| anyhow!("task not found: {id}"))?;
        task.status.state = state;
        task.status.timestamp = Some(chrono::Utc::now().to_rfc3339());
        self.put(&task)
    }

    /// Merge `{ outcome: ... }` into the task's `metadata` object. Creates
    /// the metadata object if absent; preserves any pre-existing keys.
    ///
    /// Used to surface agent-declared structured outcomes (from the
    /// `task_finish` tool) to A2A consumers in a protocol-compliant way —
    /// `metadata` is the A2A v1.0 extension slot, so unknown keys are
    /// ignored by strict consumers but available to richer ones.
    pub fn attach_outcome_metadata(
        &self,
        id: &str,
        outcome: &crate::gateway::task_queue::StructuredOutcome,
    ) -> Result<()> {
        let mut task = self
            .get(id)?
            .ok_or_else(|| anyhow!("task not found: {id}"))?;

        let outcome_value =
            serde_json::to_value(outcome).map_err(|e| anyhow!("serialize outcome: {e}"))?;

        let mut meta = task
            .metadata
            .clone()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        meta.insert("outcome".to_owned(), outcome_value);
        task.metadata = Some(serde_json::Value::Object(meta));

        self.put(&task)
    }

    pub fn append_history(&self, id: &str, msg: A2aMessage) -> Result<()> {
        let mut task = self
            .get(id)?
            .ok_or_else(|| anyhow!("task not found: {id}"))?;
        task.history.push(msg);
        self.put(&task)
    }

    /// Append or replace artifact parts. If an artifact with the same
    /// `artifact_id` already exists, the new parts are appended to it
    /// (mirroring the v1.0 streaming `append=true` semantics). Otherwise
    /// the artifact is added.
    pub fn append_artifact(&self, id: &str, artifact: A2aArtifact) -> Result<()> {
        let mut task = self
            .get(id)?
            .ok_or_else(|| anyhow!("task not found: {id}"))?;
        if let Some(existing) = task
            .artifacts
            .iter_mut()
            .find(|a| a.artifact_id == artifact.artifact_id)
        {
            existing.parts.extend(artifact.parts);
        } else {
            task.artifacts.push(artifact);
        }
        self.put(&task)
    }

    // -----------------------------------------------------------------------
    // Push notification configs
    // -----------------------------------------------------------------------

    pub fn put_push_config(&self, cfg: &PushNotificationConfig) -> Result<()> {
        let key = format!("{}:{}", cfg.task_id, cfg.id);
        let json = serde_json::to_string(cfg)?;
        let txn = self.db.begin_write()?;
        {
            let mut tbl = txn.open_table(PUSH_CONFIGS)?;
            tbl.insert(key.as_str(), json.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<Option<PushNotificationConfig>> {
        let key = format!("{task_id}:{config_id}");
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(PUSH_CONFIGS)?;
        match tbl.get(key.as_str())? {
            Some(v) => Ok(Some(serde_json::from_str(v.value())?)),
            None => Ok(None),
        }
    }

    pub fn list_push_configs(&self, task_id: &str) -> Result<Vec<PushNotificationConfig>> {
        let prefix = format!("{task_id}:");
        let txn = self.db.begin_read()?;
        let tbl = txn.open_table(PUSH_CONFIGS)?;
        let mut out = Vec::new();
        for entry in tbl.range(prefix.as_str()..)? {
            let (k, v) = entry?;
            if !k.value().starts_with(&prefix) {
                break;
            }
            out.push(serde_json::from_str(v.value())?);
        }
        Ok(out)
    }

    pub fn delete_push_config(&self, task_id: &str, config_id: &str) -> Result<bool> {
        let key = format!("{task_id}:{config_id}");
        let txn = self.db.begin_write()?;
        let removed = {
            let mut tbl = txn.open_table(PUSH_CONFIGS)?;
            tbl.remove(key.as_str())?.is_some()
        };
        txn.commit()?;
        Ok(removed)
    }

    /// Delete every push notification config belonging to a task — called
    /// when the task reaches a terminal state (Completed / Failed /
    /// Canceled) so configs don't linger forever after delivery is done.
    /// Returns the number of configs removed.
    pub fn delete_push_configs_for_task(&self, task_id: &str) -> Result<usize> {
        let prefix = format!("{task_id}:");
        // Collect keys to delete in a read txn, then delete them in a
        // write txn. redb doesn't allow holding a read iter while writing.
        let keys: Vec<String> = {
            let txn = self.db.begin_read()?;
            let tbl = txn.open_table(PUSH_CONFIGS)?;
            let mut out = Vec::new();
            for entry in tbl.range(prefix.as_str()..)? {
                let (k, _) = entry?;
                let s = k.value();
                if !s.starts_with(&prefix) {
                    break;
                }
                out.push(s.to_owned());
            }
            out
        };
        if keys.is_empty() {
            return Ok(0);
        }
        let txn = self.db.begin_write()?;
        let n = {
            let mut tbl = txn.open_table(PUSH_CONFIGS)?;
            let mut count = 0;
            for k in &keys {
                if tbl.remove(k.as_str())?.is_some() {
                    count += 1;
                }
            }
            count
        };
        txn.commit()?;
        Ok(n)
    }

    /// Atomically admits an operation, its Task -> Attempt -> Work records, and
    /// the first durable task event. A matching operation replay returns the
    /// original record; a digest mismatch is rejected without mutation.
    pub fn create_execution(
        &self,
        operation: &OperationRecord,
        task: &DurableTaskRecord,
        attempt: &AttemptRecord,
        work: &WorkRecord,
        initial_event_payload: &str,
    ) -> std::result::Result<OperationAdmission, DurableStoreError> {
        if attempt.task_id != task.task_id
            || work.task_id != task.task_id
            || work.attempt_id != attempt.attempt_id
            || operation.task_id != task.task_id
            || operation.attempt_id != attempt.attempt_id
            || operation.work_id != work.work_id
            || work.operation_id != operation.key.operation_id
            || task.current_attempt_id.as_ref() != Some(&attempt.attempt_id)
            || work.lease.task_id != work.task_id
            || work.lease.attempt_id != work.attempt_id
            || work.lease.work_id != work.work_id
            || work.lease.agent_id != work.agent_id
            || work.lease.assigned_machine_id != work.assigned_machine_id
        {
            return Err(DurableStoreError::StateConflict(
                "invalid task, attempt, work, operation, or lease relationship",
            ));
        }
        if operation.key.actor.is_empty()
            || operation.key.actor.len() > MAX_OPERATION_ACTOR_BYTES
            || operation.key.kind.is_empty()
            || operation.key.kind.len() > MAX_OPERATION_KIND_BYTES
            || !valid_request_digest(&operation.request_digest)
        {
            return Err(DurableStoreError::StateConflict(
                "invalid operation identity or request digest",
            ));
        }
        if work.lease.lease_epoch == 0
            || work.lease.expires_at.is_empty()
            || work.lease.lease_token.is_empty()
            || work.delivery_state != rsclaw_a2a_types::types::DeliveryState::NotDelivered
            || work.outbound_dispatch.is_some()
            || work.receipt.is_some()
        {
            return Err(DurableStoreError::StateConflict(
                "new work must have a valid unacknowledged lease",
            ));
        }
        ensure_unexpired_lease(&work.lease.expires_at)?;
        if initial_event_payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(DurableStoreError::LimitExceeded(
                "initial task event payload is too large",
            ));
        }

        let initial_event = TaskEvent {
            task_id: task.task_id.clone(),
            event_seq: 1,
            payload: initial_event_payload.to_owned(),
        };
        let operation_json = serde_json::to_string(operation).map_err(anyhow::Error::from)?;
        let task_json = serde_json::to_string(task).map_err(anyhow::Error::from)?;
        let attempt_json = serde_json::to_string(attempt).map_err(anyhow::Error::from)?;
        let work_json = serde_json::to_string(work).map_err(anyhow::Error::from)?;
        let initial_event_json =
            serde_json::to_string(&initial_event).map_err(anyhow::Error::from)?;
        let event_state_json = serde_json::to_string(&TaskEventState {
            high_water: 1,
            replay_floor: 0,
        })
        .map_err(anyhow::Error::from)?;
        let key = operation.key.storage_key();
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        let admission = {
            let mut operations = txn.open_table(OPERATIONS).map_err(anyhow::Error::from)?;
            if let Some(previous) = operations.get(key.as_str()).map_err(anyhow::Error::from)? {
                let previous: OperationRecord =
                    serde_json::from_str(previous.value()).map_err(anyhow::Error::from)?;
                if previous.request_digest != operation.request_digest {
                    return Err(DurableStoreError::IdempotencyConflict);
                }
                OperationAdmission::Existing(previous)
            } else {
                let mut tasks = txn.open_table(DURABLE_TASKS).map_err(anyhow::Error::from)?;
                let mut attempts = txn.open_table(ATTEMPTS).map_err(anyhow::Error::from)?;
                let mut works = txn.open_table(WORKS).map_err(anyhow::Error::from)?;
                if tasks
                    .get(task.task_id.as_str())
                    .map_err(anyhow::Error::from)?
                    .is_some()
                    || attempts
                        .get(attempt.attempt_id.as_str())
                        .map_err(anyhow::Error::from)?
                        .is_some()
                    || works
                        .get(work.work_id.as_str())
                        .map_err(anyhow::Error::from)?
                        .is_some()
                {
                    return Err(DurableStoreError::StateConflict(
                        "durable ID already exists",
                    ));
                }
                tasks
                    .insert(task.task_id.as_str(), task_json.as_str())
                    .map_err(anyhow::Error::from)?;
                attempts
                    .insert(attempt.attempt_id.as_str(), attempt_json.as_str())
                    .map_err(anyhow::Error::from)?;
                works
                    .insert(work.work_id.as_str(), work_json.as_str())
                    .map_err(anyhow::Error::from)?;
                txn.open_table(TASK_EVENTS)
                    .map_err(anyhow::Error::from)?
                    .insert(
                        event_key(task.task_id.as_str(), 1).as_str(),
                        initial_event_json.as_str(),
                    )
                    .map_err(anyhow::Error::from)?;
                txn.open_table(TASK_EVENT_STATES)
                    .map_err(anyhow::Error::from)?
                    .insert(task.task_id.as_str(), event_state_json.as_str())
                    .map_err(anyhow::Error::from)?;
                operations
                    .insert(key.as_str(), operation_json.as_str())
                    .map_err(anyhow::Error::from)?;
                OperationAdmission::Applied(operation.clone())
            }
        };
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(admission)
    }

    /// Returns a durable work record by identity.
    pub fn durable_work(&self, work_id: &str) -> Result<Option<WorkRecord>> {
        let txn = self.db.begin_read()?;
        let works = txn.open_table(WORKS)?;
        works
            .get(work_id)?
            .map(|value| serde_json::from_str(value.value()).map_err(Into::into))
            .transpose()
    }

    /// Allocates and appends one immutable event in the same transaction.
    pub fn append_task_event(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
        payload: &str,
    ) -> std::result::Result<TaskEvent, DurableStoreError> {
        if payload.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(DurableStoreError::LimitExceeded(
                "task event payload is too large",
            ));
        }
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        let event = {
            if txn
                .open_table(DURABLE_TASKS)
                .map_err(anyhow::Error::from)?
                .get(task_id.as_str())
                .map_err(anyhow::Error::from)?
                .is_none()
            {
                return Err(DurableStoreError::StateConflict("task not found"));
            }
            let mut states = txn
                .open_table(TASK_EVENT_STATES)
                .map_err(anyhow::Error::from)?;
            let mut state: TaskEventState = states
                .get(task_id.as_str())
                .map_err(anyhow::Error::from)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict(
                    "task event state not found",
                ))?;
            state.high_water = state
                .high_water
                .checked_add(1)
                .ok_or(DurableStoreError::StateConflict("event sequence exhausted"))?;
            let event = TaskEvent {
                task_id: task_id.clone(),
                event_seq: state.high_water,
                payload: payload.to_owned(),
            };
            let event_json = serde_json::to_string(&event).map_err(anyhow::Error::from)?;
            txn.open_table(TASK_EVENTS)
                .map_err(anyhow::Error::from)?
                .insert(
                    event_key(task_id.as_str(), event.event_seq).as_str(),
                    event_json.as_str(),
                )
                .map_err(anyhow::Error::from)?;
            let state_json = serde_json::to_string(&state).map_err(anyhow::Error::from)?;
            states
                .insert(task_id.as_str(), state_json.as_str())
                .map_err(anyhow::Error::from)?;
            event
        };
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(event)
    }

    /// Returns no more than 1,000 ordered events strictly after `after`, or
    /// resync metadata for a compacted gap.
    pub fn events_after(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
        after: u64,
        limit: usize,
    ) -> std::result::Result<TaskEventReplay, DurableStoreError> {
        let txn = self.db.begin_read().map_err(anyhow::Error::from)?;
        let states = txn
            .open_table(TASK_EVENT_STATES)
            .map_err(anyhow::Error::from)?;
        let state: TaskEventState = states
            .get(task_id.as_str())
            .map_err(anyhow::Error::from)?
            .map(|v| serde_json::from_str(v.value()))
            .transpose()
            .map_err(anyhow::Error::from)?
            .ok_or(DurableStoreError::StateConflict("task not found"))?;
        if after < state.replay_floor {
            return Ok(TaskEventReplay::ResyncRequired {
                replay_floor: state.replay_floor,
                high_water: state.high_water,
            });
        }
        if limit > MAX_EVENT_PAGE {
            return Err(DurableStoreError::LimitExceeded(
                "task event replay page exceeds 1,000 entries",
            ));
        }
        let Some(first_seq) = after.checked_add(1) else {
            return Ok(TaskEventReplay::Events(TaskEventPage {
                events: Vec::new(),
                high_water: state.high_water,
            }));
        };
        let prefix = format!("{}:", task_id.as_str());
        let start = event_key(task_id.as_str(), first_seq);
        let events_table = txn.open_table(TASK_EVENTS).map_err(anyhow::Error::from)?;
        let mut events = Vec::with_capacity(limit);
        for entry in events_table
            .range(start.as_str()..)
            .map_err(anyhow::Error::from)?
        {
            let (key, value) = entry.map_err(anyhow::Error::from)?;
            if !key.value().starts_with(&prefix) || events.len() == limit {
                break;
            }
            events.push(serde_json::from_str(value.value()).map_err(anyhow::Error::from)?);
        }
        Ok(TaskEventReplay::Events(TaskEventPage {
            events,
            high_water: state.high_water,
        }))
    }

    /// Advances a consumer's contiguous cursor, rejecting decreases and values
    /// above the task high-water mark.
    pub fn acknowledge_events(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
        consumer: &str,
        cursor: u64,
    ) -> std::result::Result<(), DurableStoreError> {
        if consumer.is_empty() || consumer.len() > MAX_OPERATION_ACTOR_BYTES {
            return Err(DurableStoreError::CursorConflict(
                "consumer identity is empty or too large",
            ));
        }
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        {
            let state: TaskEventState = txn
                .open_table(TASK_EVENT_STATES)
                .map_err(anyhow::Error::from)?
                .get(task_id.as_str())
                .map_err(anyhow::Error::from)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict("task not found"))?;
            if cursor > state.high_water {
                return Err(DurableStoreError::CursorConflict(
                    "cursor exceeds high-water",
                ));
            }
            if cursor < state.replay_floor {
                return Err(DurableStoreError::CursorConflict(
                    "cursor precedes replay floor",
                ));
            }
            let key = composite_key(&[task_id.as_str(), consumer]);
            let mut cursors = txn
                .open_table(TASK_EVENT_CURSORS)
                .map_err(anyhow::Error::from)?;
            if let Some(previous) = cursors.get(key.as_str()).map_err(anyhow::Error::from)? {
                if cursor
                    < previous
                        .value()
                        .parse::<u64>()
                        .map_err(anyhow::Error::from)?
                {
                    return Err(DurableStoreError::CursorConflict("cursor moved backwards"));
                }
            }
            let value = cursor.to_string();
            cursors
                .insert(key.as_str(), value.as_str())
                .map_err(anyhow::Error::from)?;
        }
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Returns the durable cursor for one task consumer.
    pub fn event_cursor(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
        consumer: &str,
    ) -> std::result::Result<Option<u64>, DurableStoreError> {
        let key = composite_key(&[task_id.as_str(), consumer]);
        let txn = self.db.begin_read().map_err(anyhow::Error::from)?;
        let cursors = txn
            .open_table(TASK_EVENT_CURSORS)
            .map_err(anyhow::Error::from)?;
        cursors
            .get(key.as_str())
            .map_err(anyhow::Error::from)?
            .map(|value| value.value().parse::<u64>().map_err(anyhow::Error::from))
            .transpose()
            .map_err(DurableStoreError::Storage)
    }

    /// Returns the durable event high-water mark and replay floor for a task.
    pub fn task_event_state(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
    ) -> std::result::Result<Option<TaskEventState>, DurableStoreError> {
        let txn = self.db.begin_read().map_err(anyhow::Error::from)?;
        let states = txn
            .open_table(TASK_EVENT_STATES)
            .map_err(anyhow::Error::from)?;
        states
            .get(task_id.as_str())
            .map_err(anyhow::Error::from)?
            .map(|value| serde_json::from_str(value.value()).map_err(anyhow::Error::from))
            .transpose()
            .map_err(DurableStoreError::Storage)
    }

    /// Advances the logical replay floor; callers must compact payloads
    /// separately and never move it backwards.
    pub fn advance_replay_floor(
        &self,
        task_id: &rsclaw_a2a_types::types::TaskId,
        replay_floor: u64,
    ) -> std::result::Result<(), DurableStoreError> {
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        {
            let cursor_prefix = composite_key(&[task_id.as_str()]);
            let cursors = txn
                .open_table(TASK_EVENT_CURSORS)
                .map_err(anyhow::Error::from)?;
            for entry in cursors
                .range(cursor_prefix.as_str()..)
                .map_err(anyhow::Error::from)?
            {
                let (key, value) = entry.map_err(anyhow::Error::from)?;
                if !key.value().starts_with(&cursor_prefix) {
                    break;
                }
                let cursor = value.value().parse::<u64>().map_err(anyhow::Error::from)?;
                if cursor < replay_floor {
                    return Err(DurableStoreError::CursorConflict(
                        "replay floor exceeds an active consumer cursor",
                    ));
                }
            }
            drop(cursors);
            let mut states = txn
                .open_table(TASK_EVENT_STATES)
                .map_err(anyhow::Error::from)?;
            let mut state: TaskEventState = states
                .get(task_id.as_str())
                .map_err(anyhow::Error::from)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict("task not found"))?;
            if replay_floor < state.replay_floor || replay_floor > state.high_water {
                return Err(DurableStoreError::StateConflict("invalid replay floor"));
            }
            state.replay_floor = replay_floor;
            let json = serde_json::to_string(&state).map_err(anyhow::Error::from)?;
            states
                .insert(task_id.as_str(), json.as_str())
                .map_err(anyhow::Error::from)?;
        }
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }

    /// Persists that a dispatch entered the outbound log and is
    /// delivery-unknown until receipt. Identical repeats are idempotent;
    /// conflicting payloads are rejected.
    pub fn record_outbound_dispatch(
        &self,
        work_id: &str,
        dispatch: &str,
    ) -> std::result::Result<(), DurableStoreError> {
        if dispatch.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(DurableStoreError::LimitExceeded(
                "outbound dispatch is too large",
            ));
        }
        self.update_durable_work(work_id, |work| {
            if let Some(existing) = &work.outbound_dispatch {
                if existing != dispatch {
                    return Err(DurableStoreError::StateConflict(
                        "conflicting outbound dispatch",
                    ));
                }
                return Ok(());
            }
            if work.delivery_state != rsclaw_a2a_types::types::DeliveryState::NotDelivered {
                return Err(DurableStoreError::StateConflict(
                    "dispatch state changed without an outbound record",
                ));
            }
            work.outbound_dispatch = Some(dispatch.to_owned());
            work.delivery_state = rsclaw_a2a_types::types::DeliveryState::DeliveryUnknown;
            Ok(())
        })
    }

    /// Persists a receipt only when its typed task, attempt, work, agent,
    /// machine, and epoch binding matches the current unexpired lease.
    pub fn record_receipt(
        &self,
        receipt: &WorkReceipt,
    ) -> std::result::Result<(), DurableStoreError> {
        self.update_durable_work(receipt.work_id.as_str(), |work| {
            if receipt.task_id != work.task_id
                || receipt.attempt_id != work.attempt_id
                || receipt.work_id != work.work_id
                || receipt.agent_id != work.agent_id
                || receipt.machine_id != work.assigned_machine_id
            {
                return Err(DurableStoreError::StateConflict(
                    "receipt binding does not match work",
                ));
            }
            if receipt.lease_epoch != work.lease.lease_epoch {
                return Err(DurableStoreError::Fenced {
                    current_epoch: work.lease.lease_epoch,
                });
            }
            ensure_unexpired_lease(&work.lease.expires_at)?;
            if work.outbound_dispatch.is_none() {
                return Err(DurableStoreError::StateConflict(
                    "receipt arrived before outbound dispatch",
                ));
            }
            if let Some(existing) = &work.receipt {
                if existing != receipt {
                    return Err(DurableStoreError::StateConflict("conflicting receipt"));
                }
                return Ok(());
            }
            work.receipt = Some(receipt.clone());
            work.delivery_state = rsclaw_a2a_types::types::DeliveryState::Delivered;
            Ok(())
        })
    }

    /// Records a durable relay dispatch only after validating its exact work
    /// assignment. The serialized durable frame is the outbound log entry; a
    /// successful insert deliberately changes delivery to `DeliveryUnknown`.
    pub fn record_relay_dispatch(
        &self,
        frame: &rsclaw_a2a_types::durable_relay::RelayFrame,
    ) -> std::result::Result<(), DurableStoreError> {
        use rsclaw_a2a_types::durable_relay::{RelayBody, RelayKind};
        frame
            .validate()
            .map_err(|_| DurableStoreError::StateConflict("invalid relay frame"))?;
        let RelayBody::DispatchWork(dispatch) = &frame.body else {
            return Err(DurableStoreError::StateConflict(
                "relay frame is not DispatchWork",
            ));
        };
        if frame.kind != RelayKind::DispatchWork {
            return Err(DurableStoreError::StateConflict(
                "relay kind is not DispatchWork",
            ));
        }
        let work = self
            .durable_work(dispatch.work_id.as_str())
            .map_err(DurableStoreError::Storage)?
            .ok_or(DurableStoreError::StateConflict("work not found"))?;
        if work.task_id != dispatch.task_id
            || work.attempt_id != dispatch.attempt_id
            || work.agent_id != dispatch.agent_id
            || work.assigned_repo_id != dispatch.repo_id
            || work.assigned_workspace_id != dispatch.workspace_id
            || work.operation_id != dispatch.operation_id
            || work.lease.lease_epoch != dispatch.lease.lease_epoch
            || work.lease.expires_at != dispatch.lease.expires_at
            || work.lease.lease_token
                != rsclaw_a2a_types::types::LeaseToken::new(dispatch.lease.lease_token.clone())
        {
            return Err(DurableStoreError::StateConflict(
                "dispatch binding does not match work",
            ));
        }
        let serialized = serde_json::to_string(frame).map_err(anyhow::Error::from)?;
        self.record_outbound_dispatch(work.work_id.as_str(), &serialized)
    }

    /// Authenticates a durable relay Receipt against the machine that owns the
    /// current lease, then persists its exact work binding. A receipt cannot
    /// convert a `NotDelivered` dispatch or an older lease epoch to Delivered.
    pub fn record_relay_receipt(
        &self,
        machine_id: &rsclaw_a2a_types::types::MachineId,
        receipt: &rsclaw_a2a_types::durable_relay::ReceiptBody,
        serialized_receipt: &str,
    ) -> std::result::Result<(), DurableStoreError> {
        let work = self
            .durable_work(receipt.work_id.as_str())
            .map_err(DurableStoreError::Storage)?
            .ok_or(DurableStoreError::StateConflict("work not found"))?;
        let outbound =
            work.outbound_dispatch
                .as_deref()
                .ok_or(DurableStoreError::StateConflict(
                    "receipt arrived before outbound dispatch",
                ))?;
        let dispatch: rsclaw_a2a_types::durable_relay::RelayFrame = serde_json::from_str(outbound)
            .map_err(|_| DurableStoreError::StateConflict("stored outbound dispatch is invalid"))?;
        if dispatch.frame_id != receipt.frame_id {
            return Err(DurableStoreError::StateConflict(
                "receipt frame binding does not match dispatch",
            ));
        }
        self.record_receipt(&WorkReceipt {
            task_id: work.task_id,
            attempt_id: receipt.attempt_id.clone(),
            work_id: receipt.work_id.clone(),
            agent_id: receipt.agent_id.clone(),
            machine_id: machine_id.clone(),
            lease_epoch: receipt.lease_epoch,
            receipt: serialized_receipt.to_owned(),
        })
    }

    /// Persists a worker terminal report and its replayable task event before
    /// the hub forwards it. The authenticated source machine and every fenced
    /// binding must match the currently delivered lease; stale terminals are
    /// rejected rather than changing a newer attempt's state.
    pub fn record_relay_terminal(
        &self,
        machine_id: &rsclaw_a2a_types::types::MachineId,
        terminal: &rsclaw_a2a_types::durable_relay::WorkTerminalBody,
        serialized_terminal: &str,
    ) -> std::result::Result<TaskEvent, DurableStoreError> {
        use rsclaw_a2a_types::types::{AttemptState, DeliveryState, WorkState};

        if serialized_terminal.len() > MAX_EVENT_PAYLOAD_BYTES {
            return Err(DurableStoreError::LimitExceeded(
                "terminal event is too large",
            ));
        }
        let terminal_state = match terminal.outcome.as_str() {
            "Succeeded" => (WorkState::Succeeded, AttemptState::Succeeded),
            "Failed" => (WorkState::Failed, AttemptState::Failed),
            "Canceled" => (WorkState::Canceled, AttemptState::Canceled),
            _ => return Err(DurableStoreError::StateConflict("invalid terminal outcome")),
        };
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        let event = {
            let mut works = txn.open_table(WORKS).map_err(anyhow::Error::from)?;
            let work_json = works
                .get(terminal.binding.work_id.as_str())
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict("work not found"))?
                .value()
                .to_owned();
            let mut work: WorkRecord =
                serde_json::from_str(&work_json).map_err(anyhow::Error::from)?;
            if &work.assigned_machine_id != machine_id
                || work.work_id != terminal.binding.work_id
                || work.attempt_id != terminal.binding.attempt_id
                || work.agent_id != terminal.binding.agent_id
            {
                return Err(DurableStoreError::StateConflict(
                    "terminal binding does not match work",
                ));
            }
            if work.lease.lease_epoch != terminal.binding.lease_epoch {
                return Err(DurableStoreError::Fenced {
                    current_epoch: work.lease.lease_epoch,
                });
            }
            ensure_unexpired_lease(&work.lease.expires_at)?;
            if work.delivery_state != DeliveryState::Delivered {
                return Err(DurableStoreError::StateConflict(
                    "terminal arrived before receipt",
                ));
            }
            if work.state.is_terminal() {
                return Err(DurableStoreError::StateConflict("work is already terminal"));
            }

            let mut attempts = txn.open_table(ATTEMPTS).map_err(anyhow::Error::from)?;
            let attempt_json = attempts
                .get(work.attempt_id.as_str())
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict("attempt not found"))?
                .value()
                .to_owned();
            let mut attempt: AttemptRecord =
                serde_json::from_str(&attempt_json).map_err(anyhow::Error::from)?;
            if attempt.task_id != work.task_id || attempt.state != AttemptState::Active {
                return Err(DurableStoreError::StateConflict(
                    "attempt is not active for work",
                ));
            }

            let mut states = txn
                .open_table(TASK_EVENT_STATES)
                .map_err(anyhow::Error::from)?;
            let mut event_state: TaskEventState = states
                .get(work.task_id.as_str())
                .map_err(anyhow::Error::from)?
                .map(|v| serde_json::from_str(v.value()))
                .transpose()
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict(
                    "task event state not found",
                ))?;
            event_state.high_water = event_state
                .high_water
                .checked_add(1)
                .ok_or(DurableStoreError::StateConflict("event sequence exhausted"))?;
            let event = TaskEvent {
                task_id: work.task_id.clone(),
                event_seq: event_state.high_water,
                payload: serialized_terminal.to_owned(),
            };

            work.state = terminal_state.0;
            attempt.state = terminal_state.1;
            let work_json = serde_json::to_string(&work).map_err(anyhow::Error::from)?;
            let attempt_json = serde_json::to_string(&attempt).map_err(anyhow::Error::from)?;
            let event_json = serde_json::to_string(&event).map_err(anyhow::Error::from)?;
            let state_json = serde_json::to_string(&event_state).map_err(anyhow::Error::from)?;
            works
                .insert(work.work_id.as_str(), work_json.as_str())
                .map_err(anyhow::Error::from)?;
            attempts
                .insert(attempt.attempt_id.as_str(), attempt_json.as_str())
                .map_err(anyhow::Error::from)?;
            txn.open_table(TASK_EVENTS)
                .map_err(anyhow::Error::from)?
                .insert(
                    event_key(event.task_id.as_str(), event.event_seq).as_str(),
                    event_json.as_str(),
                )
                .map_err(anyhow::Error::from)?;
            states
                .insert(event.task_id.as_str(), state_json.as_str())
                .map_err(anyhow::Error::from)?;
            event
        };
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(event)
    }

    /// Advances a work fence only with a lease bound to the same work
    /// assignment.
    pub fn advance_lease_epoch(
        &self,
        work_id: &str,
        expected_epoch: u64,
        lease: WorkLease,
    ) -> std::result::Result<(), DurableStoreError> {
        self.update_durable_work(work_id, |work| {
            if expected_epoch != work.lease.lease_epoch || lease.lease_epoch <= expected_epoch {
                return Err(DurableStoreError::Fenced {
                    current_epoch: work.lease.lease_epoch,
                });
            }
            if lease.task_id != work.task_id
                || lease.attempt_id != work.attempt_id
                || lease.work_id != work.work_id
                || lease.agent_id != work.agent_id
                || lease.assigned_machine_id != work.assigned_machine_id
                || lease.lease_token.is_empty()
            {
                return Err(DurableStoreError::StateConflict(
                    "lease binding does not match work",
                ));
            }
            ensure_unexpired_lease(&lease.expires_at)?;
            work.lease = lease;
            work.state = rsclaw_a2a_types::types::WorkState::Recovering;
            work.delivery_state = rsclaw_a2a_types::types::DeliveryState::NotDelivered;
            work.outbound_dispatch = None;
            work.receipt = None;
            Ok(())
        })
    }

    fn update_durable_work(
        &self,
        work_id: &str,
        update: impl FnOnce(&mut WorkRecord) -> std::result::Result<(), DurableStoreError>,
    ) -> std::result::Result<(), DurableStoreError> {
        let txn = self.db.begin_write().map_err(anyhow::Error::from)?;
        {
            let mut works = txn.open_table(WORKS).map_err(anyhow::Error::from)?;
            let work_json = works
                .get(work_id)
                .map_err(anyhow::Error::from)?
                .ok_or(DurableStoreError::StateConflict("work not found"))?
                .value()
                .to_owned();
            let mut work: WorkRecord =
                serde_json::from_str(&work_json).map_err(anyhow::Error::from)?;
            update(&mut work)?;
            let json = serde_json::to_string(&work).map_err(anyhow::Error::from)?;
            works
                .insert(work_id, json.as_str())
                .map_err(anyhow::Error::from)?;
        }
        txn.commit().map_err(anyhow::Error::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rsclaw_a2a_types::types::A2aTaskStatus;

    use super::*;

    fn task(id: &str, context_id: &str) -> A2aTask {
        A2aTask {
            id: id.to_owned(),
            context_id: Some(context_id.to_owned()),
            status: A2aTaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            history: Vec::new(),
            artifacts: Vec::new(),
            metadata: None,
        }
    }

    #[test]
    fn create_task_atomically_preserves_original_task_and_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(&tmp.path().join("tasks.redb")).unwrap();

        assert!(
            store
                .create_task(&task("shared", "original"), Some("alice"))
                .unwrap()
        );
        assert!(
            !store
                .create_task(&task("shared", "replacement"), Some("bob"))
                .unwrap()
        );

        assert_eq!(store.get_owner("shared").unwrap().as_deref(), Some("alice"));
        assert_eq!(
            store.get("shared").unwrap().unwrap().context_id.as_deref(),
            Some("original")
        );
    }

    #[test]
    fn create_task_rejects_orphaned_owner_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let store = TaskStore::open(&tmp.path().join("tasks.redb")).unwrap();
        store.put_owner("reserved", "alice").unwrap();

        assert!(
            !store
                .create_task(&task("reserved", "replacement"), Some("bob"))
                .unwrap()
        );
        assert!(store.get("reserved").unwrap().is_none());
        assert_eq!(
            store.get_owner("reserved").unwrap().as_deref(),
            Some("alice")
        );
    }

    use rsclaw_a2a_types::types::{
        AgentId, AttemptId, AttemptState, DeliveryState, FleetTeamId, FrameId, LeaseToken,
        MachineId, OperationId, OperationKey, RepoId, TaskEventReplay, TaskId, WorkId, WorkReceipt,
        WorkState, WorkspaceId,
    };

    fn future_timestamp(minutes: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    fn sample_execution() -> (
        DurableTaskRecord,
        AttemptRecord,
        WorkRecord,
        OperationRecord,
    ) {
        let task_id = TaskId::new();
        let attempt_id = AttemptId::new();
        let work_id = WorkId::new();
        let operation_id = OperationId::new();
        let agent_id = AgentId::new();
        let machine_id = MachineId::new();
        let task = DurableTaskRecord {
            task_id: task_id.clone(),
            fleet_team_id: FleetTeamId::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            current_attempt_id: Some(attempt_id.clone()),
        };
        let attempt = AttemptRecord {
            attempt_id: attempt_id.clone(),
            task_id: task_id.clone(),
            state: AttemptState::Active,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let work = WorkRecord {
            work_id: work_id.clone(),
            operation_id: operation_id.clone(),
            attempt_id: attempt_id.clone(),
            task_id: task_id.clone(),
            agent_id: agent_id.clone(),
            assigned_machine_id: machine_id.clone(),
            assigned_repo_id: RepoId::new(),
            assigned_workspace_id: WorkspaceId::new(),
            state: WorkState::Leased,
            delivery_state: DeliveryState::NotDelivered,
            lease: WorkLease {
                task_id: task_id.clone(),
                attempt_id: attempt_id.clone(),
                work_id: work_id.clone(),
                agent_id: agent_id.clone(),
                assigned_machine_id: machine_id,
                lease_epoch: 1,
                expires_at: future_timestamp(10),
                lease_token: LeaseToken::new("lease-1"),
            },
            outbound_dispatch: None,
            receipt: None,
        };
        let operation = OperationRecord {
            key: OperationKey {
                actor: "alice".to_owned(),
                kind: "CreateTask".to_owned(),
                operation_id,
            },
            task_id,
            attempt_id,
            work_id,
            request_digest: "a".repeat(SHA256_HEX_BYTES),
            result: "accepted".to_owned(),
        };
        (task, attempt, work, operation)
    }

    fn receipt_for(work: &WorkRecord, value: &str) -> WorkReceipt {
        WorkReceipt {
            task_id: work.task_id.clone(),
            attempt_id: work.attempt_id.clone(),
            work_id: work.work_id.clone(),
            agent_id: work.agent_id.clone(),
            machine_id: work.assigned_machine_id.clone(),
            lease_epoch: work.lease.lease_epoch,
            receipt: value.to_owned(),
        }
    }

    fn relay_dispatch(
        task: &DurableTaskRecord,
        work: &WorkRecord,
        operation: &OperationRecord,
    ) -> rsclaw_a2a_types::durable_relay::RelayFrame {
        use rsclaw_a2a_types::durable_relay::{
            DispatchWorkBody, LeaseBody, RelayBody, RelayFrame, RelayKind,
        };

        RelayFrame {
            frame_id: FrameId::new(),
            fleet_team_id: task.fleet_team_id.clone(),
            machine_id: MachineId::new(),
            sent_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind: RelayKind::DispatchWork,
            seq: 1,
            ack: 0,
            cursor: 0,
            route: Vec::new(),
            body: RelayBody::DispatchWork(DispatchWorkBody {
                task_id: work.task_id.clone(),
                attempt_id: work.attempt_id.clone(),
                work_id: work.work_id.clone(),
                agent_id: work.agent_id.clone(),
                repo_id: work.assigned_repo_id.clone(),
                workspace_id: work.assigned_workspace_id.clone(),
                lease: LeaseBody {
                    lease_epoch: work.lease.lease_epoch,
                    expires_at: work.lease.expires_at.clone(),
                    lease_token: "lease-1".to_owned(),
                },
                operation_id: operation.key.operation_id.clone(),
                hops_remaining: 4,
                payload: serde_json::json!({"text": "hello"}),
            }),
        }
    }

    #[test]
    fn durable_execution_is_idempotent_fenced_and_recovered_after_reopen() {
        let tmp = tempfile::tempdir().expect("create temp directory");
        let path = tmp.path().join("tasks.redb");
        let (task, attempt, work, operation) = sample_execution();
        let store = TaskStore::open(&path).expect("open durable task store");

        assert!(matches!(
            store
                .create_execution(&operation, &task, &attempt, &work, "submitted")
                .expect("admit first operation"),
            OperationAdmission::Applied(_)
        ));
        assert!(matches!(
            store
                .create_execution(&operation, &task, &attempt, &work, "ignored duplicate")
                .expect("deduplicate operation"),
            OperationAdmission::Existing(_)
        ));
        let mut conflict = operation.clone();
        conflict.request_digest = "b".repeat(SHA256_HEX_BYTES);
        assert!(matches!(
            store.create_execution(&conflict, &task, &attempt, &work, "conflict"),
            Err(DurableStoreError::IdempotencyConflict)
        ));

        let receipt = receipt_for(&work, "receipt-1");
        assert!(matches!(
            store.record_receipt(&receipt),
            Err(DurableStoreError::StateConflict(
                "receipt arrived before outbound dispatch"
            ))
        ));
        store
            .record_outbound_dispatch(work.work_id.as_str(), "dispatch-1")
            .expect("persist outbound dispatch");
        store
            .record_outbound_dispatch(work.work_id.as_str(), "dispatch-1")
            .expect("deduplicate outbound dispatch");
        assert!(matches!(
            store.record_outbound_dispatch(work.work_id.as_str(), "dispatch-2"),
            Err(DurableStoreError::StateConflict(
                "conflicting outbound dispatch"
            ))
        ));
        let mut stale_receipt = receipt.clone();
        stale_receipt.lease_epoch = 0;
        assert!(matches!(
            store.record_receipt(&stale_receipt),
            Err(DurableStoreError::Fenced { current_epoch: 1 })
        ));
        store
            .record_receipt(&receipt)
            .expect("persist matching receipt");
        store
            .record_receipt(&receipt)
            .expect("deduplicate matching receipt");
        let mut conflicting_receipt = receipt.clone();
        conflicting_receipt.receipt = "receipt-2".to_owned();
        assert!(matches!(
            store.record_receipt(&conflicting_receipt),
            Err(DurableStoreError::StateConflict("conflicting receipt"))
        ));
        drop(store);

        let reopened = TaskStore::open(&path).expect("reopen durable task store");
        let recovered = reopened
            .durable_work(work.work_id.as_str())
            .expect("load work")
            .expect("work exists");
        assert_eq!(recovered.delivery_state, DeliveryState::Delivered);
        assert_eq!(recovered.receipt.as_ref(), Some(&receipt));

        let mut renewed = work.lease.clone();
        renewed.lease_epoch = 2;
        renewed.expires_at = future_timestamp(20);
        renewed.lease_token = LeaseToken::new("lease-2");
        assert!(matches!(
            reopened.advance_lease_epoch(work.work_id.as_str(), 0, renewed.clone()),
            Err(DurableStoreError::Fenced { current_epoch: 1 })
        ));
        reopened
            .advance_lease_epoch(work.work_id.as_str(), 1, renewed)
            .expect("advance work fence");
        let fenced = reopened
            .durable_work(work.work_id.as_str())
            .expect("load fenced work")
            .expect("fenced work exists");
        assert_eq!(fenced.lease.lease_epoch, 2);
        assert_eq!(fenced.state, WorkState::Recovering);
        assert_eq!(fenced.delivery_state, DeliveryState::NotDelivered);
        assert!(fenced.outbound_dispatch.is_none());
        assert!(fenced.receipt.is_none());
    }

    #[test]
    fn durable_relay_dispatch_and_receipt_require_exact_binding() {
        use rsclaw_a2a_types::durable_relay::{ReceiptBody, RelayBody};

        let tmp = tempfile::tempdir().expect("create temp directory");
        let path = tmp.path().join("tasks.redb");
        let (task, attempt, work, operation) = sample_execution();
        let store = TaskStore::open(&path).expect("open durable task store");
        store
            .create_execution(&operation, &task, &attempt, &work, "submitted")
            .expect("admit execution");
        let dispatch = relay_dispatch(&task, &work, &operation);
        let mut wrong_operation = dispatch.clone();
        let RelayBody::DispatchWork(body) = &mut wrong_operation.body else {
            panic!("expected dispatch body");
        };
        body.operation_id = OperationId::new();
        assert!(matches!(
            store.record_relay_dispatch(&wrong_operation),
            Err(DurableStoreError::StateConflict(
                "dispatch binding does not match work"
            ))
        ));
        store
            .record_relay_dispatch(&dispatch)
            .expect("persist durable relay dispatch");

        let mut receipt = ReceiptBody {
            work_id: work.work_id.clone(),
            attempt_id: work.attempt_id.clone(),
            agent_id: work.agent_id.clone(),
            lease_epoch: work.lease.lease_epoch,
            frame_id: FrameId::new(),
        };
        assert!(matches!(
            store.record_relay_receipt(&work.assigned_machine_id, &receipt, "wrong-frame-receipt"),
            Err(DurableStoreError::StateConflict(
                "receipt frame binding does not match dispatch"
            ))
        ));
        receipt.frame_id = dispatch.frame_id.clone();
        assert!(matches!(
            store.record_relay_receipt(&MachineId::new(), &receipt, "wrong-machine-receipt"),
            Err(DurableStoreError::StateConflict(
                "receipt binding does not match work"
            ))
        ));
        let receipt_json = serde_json::to_string(&RelayBody::Receipt(receipt.clone()))
            .expect("serialize durable receipt");
        store
            .record_relay_receipt(&work.assigned_machine_id, &receipt, &receipt_json)
            .expect("persist exact receipt binding");
        let persisted = store
            .durable_work(work.work_id.as_str())
            .expect("load durable work")
            .expect("durable work exists");
        assert_eq!(persisted.delivery_state, DeliveryState::Delivered);
        assert_eq!(
            persisted
                .receipt
                .as_ref()
                .map(|value| value.receipt.as_str()),
            Some(receipt_json.as_str())
        );
    }

    #[test]
    fn durable_terminal_is_fenced_and_appended_before_forwarding() {
        use rsclaw_a2a_types::durable_relay::{RelayBody, WorkTerminalBody, WorkerBinding};

        let tmp = tempfile::tempdir().expect("create temp directory");
        let (task, attempt, work, operation) = sample_execution();
        let store = TaskStore::open(&tmp.path().join("tasks.redb")).expect("open store");
        store
            .create_execution(&operation, &task, &attempt, &work, "submitted")
            .expect("admit");
        let dispatch = relay_dispatch(&task, &work, &operation);
        store
            .record_relay_dispatch(&dispatch)
            .expect("persist dispatch");
        let receipt = rsclaw_a2a_types::durable_relay::ReceiptBody {
            work_id: work.work_id.clone(),
            attempt_id: work.attempt_id.clone(),
            agent_id: work.agent_id.clone(),
            lease_epoch: work.lease.lease_epoch,
            frame_id: dispatch.frame_id.clone(),
        };
        store
            .record_relay_receipt(&work.assigned_machine_id, &receipt, "receipt")
            .expect("receipt");
        let terminal = WorkTerminalBody {
            binding: WorkerBinding {
                work_id: work.work_id.clone(),
                attempt_id: work.attempt_id.clone(),
                agent_id: work.agent_id.clone(),
                lease_epoch: work.lease.lease_epoch,
            },
            outcome: "Succeeded".to_owned(),
            result: Some(serde_json::json!({"ok": true})),
            failure: None,
        };
        let terminal_json = serde_json::to_string(&RelayBody::WorkTerminal(terminal.clone()))
            .expect("serialize terminal");
        let event = store
            .record_relay_terminal(&work.assigned_machine_id, &terminal, &terminal_json)
            .expect("persist terminal");
        assert_eq!(event.event_seq, 2);
        assert_eq!(event.payload, terminal_json);
        assert_eq!(
            store
                .durable_work(work.work_id.as_str())
                .expect("work")
                .expect("exists")
                .state,
            WorkState::Succeeded
        );

        let mut stale = terminal;
        stale.binding.lease_epoch += 1;
        assert!(matches!(
            store.record_relay_terminal(&work.assigned_machine_id, &stale, "stale"),
            Err(DurableStoreError::Fenced { current_epoch: 1 })
        ));
    }

    #[test]
    fn durable_event_replay_cursor_and_resync_survive_reopen() {
        let tmp = tempfile::tempdir().expect("create temp directory");
        let path = tmp.path().join("tasks.redb");
        let (task, attempt, work, operation) = sample_execution();
        let store = TaskStore::open(&path).expect("open durable task store");
        store
            .create_execution(&operation, &task, &attempt, &work, "submitted")
            .expect("admit execution");
        let second = store
            .append_task_event(&task.task_id, "working")
            .expect("append second event");
        let third = store
            .append_task_event(&task.task_id, "completed")
            .expect("append third event");
        assert_eq!((second.event_seq, third.event_seq), (2, 3));

        let replay = store
            .events_after(&task.task_id, 1, 1)
            .expect("replay bounded suffix");
        let TaskEventReplay::Events(page) = replay else {
            panic!("expected replay page");
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].event_seq, 2);
        assert_eq!(page.high_water, 3);
        assert!(matches!(
            store.events_after(&task.task_id, 0, MAX_EVENT_PAGE + 1),
            Err(DurableStoreError::LimitExceeded(_))
        ));

        store
            .acknowledge_events(&task.task_id, "consumer-a", 2)
            .expect("advance cursor");
        assert!(matches!(
            store.acknowledge_events(&task.task_id, "consumer-a", 1),
            Err(DurableStoreError::CursorConflict("cursor moved backwards"))
        ));
        assert!(matches!(
            store.acknowledge_events(&task.task_id, "consumer-a", 4),
            Err(DurableStoreError::CursorConflict(
                "cursor exceeds high-water"
            ))
        ));
        assert!(matches!(
            store.advance_replay_floor(&task.task_id, 3),
            Err(DurableStoreError::CursorConflict(_))
        ));
        store
            .acknowledge_events(&task.task_id, "consumer-a", 3)
            .expect("acknowledge terminal event");
        store
            .advance_replay_floor(&task.task_id, 3)
            .expect("advance replay floor");
        assert!(matches!(
            store.events_after(&task.task_id, 2, 10),
            Ok(TaskEventReplay::ResyncRequired {
                replay_floor: 3,
                high_water: 3
            })
        ));
        drop(store);

        let reopened = TaskStore::open(&path).expect("reopen durable task store");
        assert_eq!(
            reopened
                .event_cursor(&task.task_id, "consumer-a")
                .expect("load consumer cursor"),
            Some(3)
        );
        assert_eq!(
            reopened
                .task_event_state(&task.task_id)
                .expect("load event state"),
            Some(TaskEventState {
                high_water: 3,
                replay_floor: 3
            })
        );
    }

    #[test]
    fn durable_store_refuses_to_replace_corrupt_history() {
        let tmp = tempfile::tempdir().expect("create temp directory");
        let path = tmp.path().join("tasks.redb");
        let original = b"not-a-redb-database";
        std::fs::write(&path, original).expect("write corrupt fixture");

        assert!(TaskStore::open(&path).is_err());
        assert_eq!(
            std::fs::read(&path).expect("read corrupt fixture"),
            original
        );
        assert!(!tmp.path().join("tasks.redb.broken").exists());
    }
}
