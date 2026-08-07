//! redb-backed persistence for A2A v1.0 tasks + push notification configs.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use rsclaw_a2a_types::types::{
    A2aArtifact, A2aMessage, A2aTask, PushNotificationConfig, TaskState,
};

const TASKS: TableDefinition<&str, &str> = TableDefinition::new("a2a_tasks");
/// Push configs keyed by "{task_id}:{config_id}".
const PUSH_CONFIGS: TableDefinition<&str, &str> = TableDefinition::new("a2a_push_configs");
/// Task owner index: task_id -> A2A principal id that created it. Kept out of
/// the `A2aTask` wire type so the owning principal never leaks in responses;
/// used only server-side to enforce per-caller access (A2A spec §7.5).
const TASK_OWNERS: TableDefinition<&str, &str> = TableDefinition::new("a2a_task_owners");

pub struct TaskStore {
    db: Database,
}

impl TaskStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("create a2a store dir")?;
        }
        rsclaw_store::upgrade_legacy_if_needed(path)?;
        // A2A tasks are ephemeral (completed-task records + push configs). If
        // the existing file can't be opened by the current redb — e.g. a
        // half-migrated format the legacy upgrader couldn't fix — move it aside
        // and recreate fresh rather than crashing the gateway on boot. The bad
        // file is preserved as `.broken-<ts>` for recovery, never deleted.
        //
        // BUT: transient I/O errors (permission, disk full, temporary lock)
        // should NOT trigger a reset. We retry once after a short delay; only
        // if the second attempt also fails do we move aside + recreate.
        let builder = Database::builder();
        let db = match rsclaw_store::create_with_lock_retry(&builder, path) {
            Ok(db) => db,
            // Still locked after the full retry window — a second gateway is
            // genuinely running against this base dir. Do NOT fall through to
            // the "move aside + recreate" path below: that resets task history.
            // Surface the conflict instead so the operator can stop the dupe.
            Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                anyhow::bail!(
                    "a2a task store at {} is locked by another process after retry; \
                     refusing to reset (is a second gateway running on this base dir?)",
                    path.display()
                );
            }
            Err(first_err) => {
                // Retry once after 500ms — the error may be transient (disk
                // hiccup, OS file lock release race).
                tracing::warn!(
                    path = %path.display(),
                    error = %first_err,
                    "a2a task store open failed, retrying in 500ms"
                );
                std::thread::sleep(std::time::Duration::from_millis(500));
                match rsclaw_store::create_with_lock_retry(&builder, path) {
                    Ok(db) => db,
                    Err(_second_err) => {
                        // Two consecutive failures — treat as corruption and
                        // move aside rather than crashing the gateway.
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let aside = path.with_extension(format!("redb.broken-{ts}"));
                        tracing::error!(
                            path = %path.display(),
                            moved_to = %aside.display(),
                            first_error = %first_err,
                            "a2a task store unopenable after retry; moving aside and recreating (task history reset)"
                        );
                        if path.exists() {
                            std::fs::rename(path, &aside)
                                .context("move aside unopenable a2a task store")?;
                        }
                        Database::create(path).context("recreate a2a task redb")?
                    }
                }
            }
        };
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(TASKS)?;
            let _ = txn.open_table(PUSH_CONFIGS)?;
            let _ = txn.open_table(TASK_OWNERS)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    // -----------------------------------------------------------------------
    // Tasks
    // -----------------------------------------------------------------------

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
}
