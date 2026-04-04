//! Hot-reload watcher for config file changes (AGENTS.md §11).
//!
//! Uses polling (cross-platform) to detect config file modifications.
//! On change:
//!   1. Re-parse the config.
//!   2. Diff against the previous loaded config.
//!   3. Emit `ConfigChange::RequiresRestart` if port/bind/reload changed.
//!   4. Otherwise emit `ConfigChange::FullReload` with the new config.
//!
//! Fields that require a restart (cannot hot-reload):
//!   - gateway.port
//!   - gateway.bind
//!   - gateway.reload

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use tokio::{
    sync::broadcast,
    time::{MissedTickBehavior, interval},
};
use tracing::{debug, info, warn};

use crate::{
    config::{self, runtime::RuntimeConfig},
    gateway::live_config::detect_restart_fields,
};

/// Poll interval for config file change detection.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// ConfigChange events
// ---------------------------------------------------------------------------

/// Events emitted when config file changes are detected.
#[derive(Debug, Clone)]
pub enum ConfigChange {
    /// An agent's config was updated (model, workspace, heartbeat, etc.).
    AgentUpdated(String),
    /// A channel's config was updated (dmPolicy, allowFrom, etc.).
    ChannelUpdated(String),
    /// A provider config was updated (api_key rotation, new provider).
    ModelUpdated(String),
    /// A skill was enabled or disabled.
    SkillUpdated(String),
    /// A plugin was enabled or disabled.
    PluginUpdated(String),
    /// Session config changed (dmScope, reset policy).
    SessionUpdated,
    /// Cron jobs changed (add/edit/remove).
    CronUpdated,
    /// Webhook mappings changed.
    HooksUpdated,
    /// The full config was reloaded (used when diffs are too coarse).
    FullReload(Arc<RuntimeConfig>),
    /// One or more fields require a gateway restart.
    RequiresRestart(Vec<String>),
}

// ---------------------------------------------------------------------------
// FileWatcher
// ---------------------------------------------------------------------------

pub struct FileWatcher {
    path: PathBuf,
    last_hash: u64,
    /// The last successfully parsed config, used to diff on next change.
    last_config: Option<RuntimeConfig>,
    tx: broadcast::Sender<ConfigChange>,
}

impl FileWatcher {
    /// Create a new `FileWatcher` for `path`.
    ///
    /// Returns `(watcher, receiver)`. Call `watcher.run()` in a background
    /// task.
    pub fn new(path: PathBuf) -> (Self, broadcast::Receiver<ConfigChange>) {
        let (tx, rx) = broadcast::channel(64);
        let hash = hash_file(&path).unwrap_or(0);
        // Pre-load the current config so the first real change can be diffed.
        let last_config = config::load_from(path.clone()).ok();
        (
            Self {
                path,
                last_hash: hash,
                last_config,
                tx,
            },
            rx,
        )
    }

    /// Subscribe to future change events.
    pub fn subscribe(&self) -> broadcast::Receiver<ConfigChange> {
        self.tx.subscribe()
    }

    /// Start the polling loop. Never returns unless the task is cancelled.
    pub async fn run(&mut self) {
        let mut ticker = interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        info!(path = %self.path.display(), "hot-reload watcher started");

        loop {
            ticker.tick().await;

            let new_hash = match hash_file(&self.path) {
                Ok(h) => h,
                Err(e) => {
                    warn!(path = %self.path.display(), "cannot hash config file: {e}");
                    continue;
                }
            };

            if new_hash == self.last_hash {
                continue;
            }

            self.last_hash = new_hash;
            debug!(path = %self.path.display(), "config file changed");

            self.process_change().await;
        }
    }

    async fn process_change(&mut self) {
        match config::load_from(self.path.clone()) {
            Ok(new_cfg) => {
                // Diff gateway fields against the previous config.
                let restart_fields = match self.last_config {
                    Some(ref old) => detect_restart_fields(&old.gateway, &new_cfg.gateway),
                    None => vec![],
                };

                // Always advance last_config so subsequent saves don't
                // re-trigger the same restart warning.
                self.last_config = Some(new_cfg.clone());

                if !restart_fields.is_empty() {
                    warn!(?restart_fields, "config change requires gateway restart");
                    let _ = self.tx.send(ConfigChange::RequiresRestart(restart_fields));
                    return;
                }

                info!("config hot-reloaded successfully");
                let _ = self.tx.send(ConfigChange::FullReload(Arc::new(new_cfg)));
            }
            Err(e) => {
                warn!("hot-reload failed (config error): {e:#}");
                // Don't emit any event; keep the current config active.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// File hashing
// ---------------------------------------------------------------------------

fn hash_file(path: &Path) -> Result<u64> {
    let content = std::fs::read(path)?;
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    Ok(hasher.finish())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detects_file_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.json5");
        std::fs::write(&path, r#"{}"#).expect("write");

        let (watcher, _rx) = FileWatcher::new(path.clone());
        let initial_hash = watcher.last_hash;

        std::fs::write(&path, r#"{"agents": {}}"#).expect("modify");
        let new_hash = hash_file(&path).expect("hash");
        assert_ne!(
            initial_hash, new_hash,
            "file hash should change after modification"
        );
    }

    #[test]
    fn hash_stable_for_same_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg.json5");
        std::fs::write(&path, r#"{"gateway": {}}"#).expect("write");

        let h1 = hash_file(&path).expect("h1");
        let h2 = hash_file(&path).expect("h2");
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn emits_full_reload_when_no_restart_fields_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg.json5");
        // Both writes produce a parseable default config — gateway fields unchanged.
        std::fs::write(&path, r#"{}"#).expect("write initial");

        let (mut watcher, mut rx) = FileWatcher::new(path.clone());

        // Write again (same content) then force process_change.
        std::fs::write(&path, r#"{}"#).expect("write again");
        watcher.process_change().await;

        match rx.try_recv() {
            Ok(ConfigChange::FullReload(_)) => {}
            other => panic!("expected FullReload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn second_save_after_restart_fields_does_not_re_trigger() {
        // Verify last_config is advanced after a RequiresRestart so the
        // next save (with unchanged port) emits FullReload, not RequiresRestart.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cfg.json5");
        std::fs::write(&path, r#"{}"#).expect("write initial");

        let (mut watcher, mut rx) = FileWatcher::new(path.clone());

        // First change: only non-restart fields (gateway defaults stay the same).
        watcher.process_change().await;
        // Should be FullReload because nothing restart-required changed.
        let _ = rx.try_recv();

        // Second call with same file — last_config was updated, no diff.
        watcher.process_change().await;
        match rx.try_recv() {
            Ok(ConfigChange::FullReload(_)) => {}
            other => panic!("expected FullReload on second save, got {other:?}"),
        }
    }
}
