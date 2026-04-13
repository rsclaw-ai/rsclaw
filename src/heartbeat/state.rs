use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Per-agent heartbeat run state — one entry per agent_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatState {
    pub agent_id: String,
    pub last_run_at: Option<DateTime<Utc>>,
    /// "ok" or "error"
    pub last_status: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl HeartbeatState {
    pub fn new(agent_id: impl Into<String>) -> Self {
        Self {
            agent_id: agent_id.into(),
            last_run_at: None,
            last_status: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }

    pub fn record_success(&mut self) {
        self.last_run_at = Some(Utc::now());
        self.last_status = Some("ok".to_string());
        self.last_error = None;
        self.consecutive_failures = 0;
    }

    pub fn record_failure(&mut self, error: impl Into<String>) {
        self.last_run_at = Some(Utc::now());
        self.last_status = Some("error".to_string());
        self.last_error = Some(error.into());
        self.consecutive_failures += 1;
    }
}

/// Lightweight JSON-file-backed store for heartbeat states.
/// The file contains a JSON array of [`HeartbeatState`] objects, one per agent.
pub struct HeartbeatStore {
    path: PathBuf,
}

impl HeartbeatStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load all states from the file.  Returns an empty vec if the file does
    /// not exist yet.
    pub fn load_all(&self) -> Result<Vec<HeartbeatState>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("reading heartbeat state file {}", self.path.display()))?;
        let states: Vec<HeartbeatState> = serde_json::from_str(&data)
            .with_context(|| format!("parsing heartbeat state file {}", self.path.display()))?;
        Ok(states)
    }

    /// Load a single agent's state.  Returns a fresh default if not found.
    pub fn load(&self, agent_id: &str) -> Result<HeartbeatState> {
        let all = self.load_all()?;
        Ok(all
            .into_iter()
            .find(|s| s.agent_id == agent_id)
            .unwrap_or_else(|| HeartbeatState::new(agent_id)))
    }

    /// Upsert `state` into the JSON file.
    pub fn save(&self, state: HeartbeatState) -> Result<()> {
        let mut all = self.load_all()?;
        match all.iter_mut().find(|s| s.agent_id == state.agent_id) {
            Some(existing) => *existing = state,
            None => all.push(state),
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("creating heartbeat state dir {}", parent.display())
            })?;
        }
        let data = serde_json::to_string_pretty(&all)
            .context("serializing heartbeat states")?;
        std::fs::write(&self.path, data)
            .with_context(|| format!("writing heartbeat state file {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn state_record_success_resets_failures() {
        let mut s = HeartbeatState::new("agent-1");
        s.record_failure("timeout");
        s.record_failure("timeout");
        assert_eq!(s.consecutive_failures, 2);

        s.record_success();
        assert_eq!(s.consecutive_failures, 0);
        assert_eq!(s.last_status.as_deref(), Some("ok"));
        assert!(s.last_error.is_none());
        assert!(s.last_run_at.is_some());
    }

    #[test]
    fn state_record_failure_increments() {
        let mut s = HeartbeatState::new("agent-2");
        s.record_failure("connect refused");
        assert_eq!(s.consecutive_failures, 1);
        assert_eq!(s.last_status.as_deref(), Some("error"));
        assert_eq!(s.last_error.as_deref(), Some("connect refused"));

        s.record_failure("timeout");
        assert_eq!(s.consecutive_failures, 2);
        assert_eq!(s.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn store_load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let store = HeartbeatStore::new(dir.path().join("state.json"));
        let s = store.load("unknown-agent").unwrap();
        assert_eq!(s.agent_id, "unknown-agent");
        assert_eq!(s.consecutive_failures, 0);
        assert!(s.last_run_at.is_none());
    }

    #[test]
    fn store_save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let store = HeartbeatStore::new(dir.path().join("state.json"));

        let mut s = HeartbeatState::new("agent-rt");
        s.record_success();
        store.save(s).unwrap();

        let loaded = store.load("agent-rt").unwrap();
        assert_eq!(loaded.agent_id, "agent-rt");
        assert_eq!(loaded.last_status.as_deref(), Some("ok"));
        assert_eq!(loaded.consecutive_failures, 0);
        assert!(loaded.last_run_at.is_some());
    }

    #[test]
    fn store_upserts_existing_agent() {
        let dir = tempdir().unwrap();
        let store = HeartbeatStore::new(dir.path().join("state.json"));

        let mut s = HeartbeatState::new("agent-up");
        s.record_success();
        store.save(s.clone()).unwrap();

        // Save again with a failure — should overwrite, not duplicate.
        let mut s2 = store.load("agent-up").unwrap();
        s2.record_failure("disk full");
        store.save(s2).unwrap();

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 1, "upsert must not duplicate the entry");

        let loaded = store.load("agent-up").unwrap();
        assert_eq!(loaded.last_status.as_deref(), Some("error"));
        assert_eq!(loaded.consecutive_failures, 1);
    }
}
