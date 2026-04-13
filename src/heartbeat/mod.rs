pub mod schedule;
pub mod state;

use anyhow::{anyhow, bail, Result};
use chrono::NaiveTime;
use chrono_tz::Tz;
use std::time::Duration;
use crate::agent::registry::{AgentMessage, AgentRegistry};
use crate::config::loader::base_dir;
use state::{HeartbeatState, HeartbeatStore};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Parsed representation of a HEARTBEAT.md file.
#[derive(Debug, Clone)]
pub struct HeartbeatSpec {
    pub every: Duration,
    pub active_hours: Option<(NaiveTime, NaiveTime)>,
    pub timezone: Tz,
    pub content: String,
}

/// Parse a HEARTBEAT.md string (frontmatter + body) into a [`HeartbeatSpec`].
pub fn parse_heartbeat_md(raw: &str) -> Result<HeartbeatSpec> {
    // Must start with "---"
    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| anyhow!("HEARTBEAT.md must begin with a '---' frontmatter block"))?;

    // The first character after "---" must be a newline (or the line ends immediately)
    let rest = if rest.starts_with('\n') {
        &rest[1..]
    } else if rest.starts_with("\r\n") {
        &rest[2..]
    } else {
        bail!("HEARTBEAT.md must begin with a '---' frontmatter block");
    };

    // Find the closing "---"
    let closing = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("HEARTBEAT.md frontmatter is not closed with '---'"))?;

    let fm_text = &rest[..closing];
    let after_closing = &rest[closing + 4..]; // skip "\n---"
    let content = if after_closing.starts_with('\n') {
        after_closing[1..].to_string()
    } else if after_closing.starts_with("\r\n") {
        after_closing[2..].to_string()
    } else {
        after_closing.to_string()
    };

    // Parse frontmatter key-value pairs (simple "key: value" lines)
    let mut every_raw: Option<String> = None;
    let mut active_hours_raw: Option<String> = None;
    let mut timezone_raw: Option<String> = None;

    for line in fm_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim().to_string();
            match key {
                "every" => every_raw = Some(val),
                "active_hours" => active_hours_raw = Some(val),
                "timezone" | "tz" => timezone_raw = Some(val),
                _ => {} // ignore unknown keys
            }
        }
    }

    let every_str = every_raw
        .ok_or_else(|| anyhow!("HEARTBEAT.md frontmatter is missing required field 'every'"))?;
    let every = parse_duration(&every_str);

    let active_hours = active_hours_raw
        .as_deref()
        .map(parse_time_range)
        .transpose()?;

    let timezone: Tz = match timezone_raw.as_deref() {
        Some(tz_str) => tz_str
            .parse()
            .map_err(|_| anyhow!("Unknown timezone: '{}'", tz_str))?,
        None => chrono_tz::Asia::Shanghai,
    };

    Ok(HeartbeatSpec {
        every,
        active_hours,
        timezone,
        content,
    })
}

/// Parse a human-readable duration string into [`std::time::Duration`].
///
/// Supported forms: `"5m"`, `"30m"`, `"1h"`, `"30s"`, bare integer (treated as minutes).
fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    if let Some(mins) = s.strip_suffix('m') {
        if let Ok(n) = mins.parse::<u64>() {
            return Duration::from_secs(n * 60);
        }
    }
    if let Some(hours) = s.strip_suffix('h') {
        if let Ok(n) = hours.parse::<u64>() {
            return Duration::from_secs(n * 3600);
        }
    }
    if let Some(secs) = s.strip_suffix('s') {
        if let Ok(n) = secs.parse::<u64>() {
            return Duration::from_secs(n);
        }
    }
    // Bare number → minutes
    if let Ok(n) = s.parse::<u64>() {
        return Duration::from_secs(n * 60);
    }
    // Fallback: zero (shouldn't happen in practice)
    Duration::ZERO
}

/// Parse a time range string of the form `"HH:MM-HH:MM"`.
fn parse_time_range(s: &str) -> Result<(NaiveTime, NaiveTime)> {
    let (start_str, end_str) = s
        .split_once('-')
        .ok_or_else(|| anyhow!("active_hours must be in 'HH:MM-HH:MM' format, got '{}'", s))?;

    let start = NaiveTime::parse_from_str(start_str.trim(), "%H:%M")
        .map_err(|e| anyhow!("Invalid start time '{}': {}", start_str.trim(), e))?;
    let end = NaiveTime::parse_from_str(end_str.trim(), "%H:%M")
        .map_err(|e| anyhow!("Invalid end time '{}': {}", end_str.trim(), e))?;

    Ok((start, end))
}

/// Heartbeat runner — scans agent workspaces and spawns per-agent heartbeat loops.
/// Periodically rescans to discover new HEARTBEAT.md files from dynamically created agents.
pub struct HeartbeatRunner {
    registry: Arc<AgentRegistry>,
    store: Arc<HeartbeatStore>,
    /// Tracks which agents already have a running heartbeat loop.
    active: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl HeartbeatRunner {
    pub fn new(
        registry: Arc<AgentRegistry>,
        data_dir: &Path,
    ) -> Self {
        let state_path = data_dir.join("heartbeat").join("state.json");
        Self {
            registry,
            store: Arc::new(HeartbeatStore::new(state_path)),
            active: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Start heartbeat loops for existing agents and spawn a rescan task
    /// to discover new HEARTBEAT.md files from dynamically created agents.
    pub fn run(self: Arc<Self>) {
        self.scan_and_spawn();

        // Rescan every 60 seconds for new HEARTBEAT.md files.
        let runner = Arc::clone(&self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                runner.scan_and_spawn();
            }
        });
    }

    /// Scan all workspace directories for HEARTBEAT*.md and spawn loops for new ones.
    fn scan_and_spawn(self: &Arc<Self>) {
        let base = base_dir();
        // Collect workspace dirs: "workspace" + "workspace-*"
        let mut dirs: Vec<(String, PathBuf)> = vec![
            ("main".to_string(), base.join("workspace")),
        ];
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(agent_id) = name.strip_prefix("workspace-") {
                    dirs.push((agent_id.to_string(), entry.path()));
                }
            }
        }

        let mut active = self.active.lock().unwrap();
        for (agent_id, workspace) in &dirs {
            // Find all HEARTBEAT*.md files in the workspace.
            let heartbeat_files = Self::find_heartbeat_files(workspace);
            for hb_path in heartbeat_files {
                let filename = hb_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let key = format!("{agent_id}:{filename}");
                if active.contains(&key) {
                    continue;
                }

                active.insert(key);
                let runner = Arc::clone(self);
                let agent_id = agent_id.clone();

                info!(agent_id = %agent_id, file = %filename, "heartbeat loop started");

                tokio::spawn(async move {
                    runner.agent_loop(&agent_id, &hb_path).await;
                });
            }
        }
    }

    /// Find all HEARTBEAT*.md files in a workspace directory.
    fn find_heartbeat_files(workspace: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(workspace) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with("HEARTBEAT") && name.ends_with(".md") {
                    files.push(entry.path());
                }
            }
        }
        files.sort();
        files
    }

    /// Per-agent heartbeat loop.
    async fn agent_loop(&self, agent_id: &str, heartbeat_path: &Path) {
        let filename = heartbeat_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let state_key = format!("{agent_id}:{filename}");

        // Load persisted state for startup delay.
        let mut hb_state = self.store.load(&state_key).unwrap_or_else(|e| {
            warn!(agent_id, "failed to load heartbeat state: {e:#}");
            HeartbeatState::new(&state_key)
        });

        // Initial spec read for startup delay calculation.
        let spec = match self.read_spec(&heartbeat_path) {
            Some(s) => s,
            None => return,
        };
        let delay = schedule::startup_delay(spec.every, hb_state.last_run_at);
        info!(agent_id, ?delay, "heartbeat waiting for first tick");
        tokio::time::sleep(delay).await;

        loop {
            // Re-read HEARTBEAT.md each tick (auto hot-reload).
            let spec = match self.read_spec(&heartbeat_path) {
                Some(s) => s,
                None => {
                    info!(agent_id, "HEARTBEAT.md removed, stopping heartbeat");
                    return;
                }
            };

            // Check active hours — sleep until window if outside.
            if let Some(sleep_dur) = schedule::check_active_hours(spec.active_hours, spec.timezone) {
                info!(agent_id, secs = sleep_dur.as_secs(), "outside active_hours, sleeping");
                tokio::time::sleep(sleep_dur).await;
                continue;
            }

            // Send heartbeat message to agent.
            match self.send_heartbeat(agent_id, &spec.content).await {
                Ok(()) => {
                    hb_state.record_success();
                }
                Err(e) => {
                    warn!(agent_id, "heartbeat failed: {e:#}");
                    hb_state.record_failure(&e.to_string());
                }
            }

            // Persist state (best-effort).
            if let Err(e) = self.store.save(hb_state.clone()) {
                warn!(agent_id, "failed to save heartbeat state: {e:#}");
            }

            // Sleep with backoff.
            let interval = schedule::backoff_interval(spec.every, hb_state.consecutive_failures);
            tokio::time::sleep(interval).await;
        }
    }

    /// Read and parse HEARTBEAT.md. Returns None if file missing or unparseable.
    fn read_spec(&self, path: &Path) -> Option<HeartbeatSpec> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return None,
        };
        match parse_heartbeat_md(&raw) {
            Ok(spec) => Some(spec),
            Err(e) => {
                warn!(path = %path.display(), "failed to parse HEARTBEAT.md: {e:#}");
                None
            }
        }
    }

    /// Send a heartbeat message to the agent and wait for reply.
    async fn send_heartbeat(&self, agent_id: &str, content: &str) -> Result<()> {
        let handle = self.registry.get(agent_id)
            .map_err(|e| anyhow!("agent not found: {e:#}"))?;

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let msg = AgentMessage {
            session_key: format!("heartbeat:{agent_id}"),
            text: content.to_owned(),
            channel: "heartbeat".to_owned(),
            peer_id: "heartbeat".to_owned(),
            chat_id: String::new(),
            reply_tx,
            extra_tools: vec![],
            images: vec![],
            files: vec![],
        };

        handle
            .tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("heartbeat send failed: agent channel closed"))?;

        // Wait for reply with timeout (5 minutes).
        match tokio::time::timeout(Duration::from_secs(300), reply_rx).await {
            Ok(Ok(_reply)) => Ok(()),
            Ok(Err(_)) => Ok(()), // reply_tx dropped — agent finished without explicit reply
            Err(_) => bail!("heartbeat timed out after 300s"),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn parse_basic_frontmatter() {
        let input = "---\nevery: 30m\n---\nHello world\n";
        let spec = parse_heartbeat_md(input).unwrap();
        assert_eq!(spec.every, Duration::from_secs(30 * 60));
        assert!(spec.active_hours.is_none());
        assert_eq!(spec.timezone, chrono_tz::Asia::Shanghai);
        assert_eq!(spec.content.trim(), "Hello world");
    }

    #[test]
    fn parse_with_active_hours() {
        let input = "---\nevery: 1h\nactive_hours: 09:15-15:05\ntimezone: Asia/Tokyo\n---\nBody text\n";
        let spec = parse_heartbeat_md(input).unwrap();
        assert_eq!(spec.every, Duration::from_secs(3600));
        let (s, e) = spec.active_hours.unwrap();
        assert_eq!(s, NaiveTime::from_hms_opt(9, 15, 0).unwrap());
        assert_eq!(e, NaiveTime::from_hms_opt(15, 5, 0).unwrap());
        assert_eq!(spec.timezone, chrono_tz::Asia::Tokyo);
        assert_eq!(spec.content.trim(), "Body text");
    }

    #[test]
    fn parse_missing_every_fails() {
        let input = "---\nactive_hours: 09:00-17:00\n---\ncontent\n";
        let err = parse_heartbeat_md(input).unwrap_err();
        assert!(err.to_string().contains("every"));
    }

    #[test]
    fn parse_missing_frontmatter_fails() {
        let input = "No frontmatter here\n";
        let err = parse_heartbeat_md(input).unwrap_err();
        assert!(err.to_string().contains("---"));
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("5m"), Duration::from_secs(5 * 60));
        assert_eq!(parse_duration("1h"), Duration::from_secs(3600));
        assert_eq!(parse_duration("30s"), Duration::from_secs(30));
        assert_eq!(parse_duration("30"), Duration::from_secs(30 * 60));
    }

    #[test]
    fn parse_time_range_valid() {
        let (s, e) = parse_time_range("09:00-17:30").unwrap();
        assert_eq!(s, NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        assert_eq!(e, NaiveTime::from_hms_opt(17, 30, 0).unwrap());
    }

    #[test]
    fn parse_time_range_invalid() {
        assert!(parse_time_range("not-a-time").is_err());
        assert!(parse_time_range("25:00-26:00").is_err());
        assert!(parse_time_range("09:00").is_err());
    }
}
