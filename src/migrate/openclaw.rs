//! OpenClaw JSONL data reader and converter.
//!
//! Reads OpenClaw's native JSONL session files and sessions.json indices,
//! converting them into rsclaw's internal message format for import.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// OpenClaw data structures (deserialized from JSONL / JSON)
// ---------------------------------------------------------------------------

/// Top-level sessions.json: maps session keys to session descriptors.
pub type SessionsIndex = HashMap<String, SessionDescriptor>;

/// A single entry in sessions.json.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDescriptor {
    pub session_id: String,
    #[serde(default)]
    pub updated_at: Option<u64>,
    #[serde(default)]
    pub chat_type: Option<String>,
    #[serde(default)]
    pub last_channel: Option<String>,
    #[serde(default)]
    pub compaction_count: Option<u32>,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub origin: Option<SessionOrigin>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOrigin {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub chat_type: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
}

/// A single JSONL event line from a session file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<String>,
    // For type=message events
    #[serde(default)]
    pub message: Option<MessagePayload>,
    // For type=model_change events
    #[serde(default)]
    pub model_id: Option<String>,
    // For type=custom events
    #[serde(default)]
    pub custom_type: Option<String>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    // For type=session events
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub version: Option<u32>,
}

/// The message payload within a type=message event.
#[derive(Debug, Clone, Deserialize)]
pub struct MessagePayload {
    pub role: String,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Scan results
// ---------------------------------------------------------------------------

/// Summary of what was found in an OpenClaw data directory.
#[derive(Debug, Default)]
pub struct OpenClawScanResult {
    /// Agent IDs found under agents/.
    pub agent_ids: Vec<String>,
    /// Total number of sessions across all agents.
    pub total_sessions: usize,
    /// Total number of JSONL files found.
    pub total_jsonl_files: usize,
    /// Memory entries (custom type=memory_put) found in JSONL.
    pub total_memories: usize,
    /// MEMORY.md files found in workspaces.
    pub total_memory_md_files: usize,
    /// Memory SQLite databases found.
    pub total_memory_dbs: usize,
    /// Workspace directories found.
    pub total_workspaces: usize,
    /// Installed skills found.
    pub total_skills: usize,
    /// Per-agent session counts.
    pub sessions_per_agent: HashMap<String, usize>,
    /// Whether openclaw.json config exists.
    pub has_config: bool,
    /// Number of cron jobs found.
    pub total_cron_jobs: usize,
}

/// A converted message ready for rsclaw import.
#[derive(Debug, Clone)]
pub struct ConvertedMessage {
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub timestamp: Option<String>,
}

/// A converted memory entry ready for rsclaw import.
#[derive(Debug, Clone)]
pub struct ConvertedMemory {
    pub key: String,
    pub value: String,
    pub agent_id: String,
}

/// Per-session import data.
#[derive(Debug)]
pub struct ConvertedSession {
    pub session_key: String,
    pub openclaw_session_id: String,
    pub agent_id: String,
    pub messages: Vec<ConvertedMessage>,
    pub updated_at: Option<u64>,
}

/// Statistics from an import operation.
#[derive(Debug, Default)]
pub struct ImportStats {
    pub sessions: usize,
    pub messages: usize,
    pub memories: usize,
    pub workspace_files: usize,
    pub skills: usize,
    pub aliases: usize,
    pub errors: usize,
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

/// Scan an OpenClaw data directory and report what data exists.
pub fn scan_openclaw(dir: &Path) -> Result<OpenClawScanResult> {
    let mut result = OpenClawScanResult::default();
    result.has_config = dir.join("openclaw.json").is_file();

    let agents_dir = dir.join("agents");
    if !agents_dir.is_dir() {
        info!(path = %dir.display(), "no agents/ directory found");
        return Ok(result);
    }

    let entries = fs::read_dir(&agents_dir)
        .with_context(|| format!("read agents dir: {}", agents_dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let agent_id = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if agent_id.is_empty() {
            continue;
        }

        result.agent_ids.push(agent_id.clone());

        let sessions_dir = path.join("sessions");
        if !sessions_dir.is_dir() {
            continue;
        }

        // Count sessions from sessions.json.
        let index_path = sessions_dir.join("sessions.json");
        if index_path.is_file() {
            match read_sessions_index(&index_path) {
                Ok(index) => {
                    let count = index.len();
                    result.total_sessions += count;
                    result.sessions_per_agent.insert(agent_id.clone(), count);
                }
                Err(e) => {
                    warn!(
                        agent = %agent_id,
                        error = %e,
                        "failed to parse sessions.json"
                    );
                }
            }
        }

        // Count JSONL files and scan for memories.
        let jsonl_entries = fs::read_dir(&sessions_dir);
        if let Ok(entries) = jsonl_entries {
            for file_entry in entries.flatten() {
                let fpath = file_entry.path();
                if fpath.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    result.total_jsonl_files += 1;
                    // Quick scan for memory_put events.
                    if let Ok(memories) = read_custom_memories(&fpath) {
                        result.total_memories += memories.len();
                    }
                }
            }
        }
    }

    // Scan workspace dirs for MEMORY.md and skills.
    for ws_entry in fs::read_dir(dir)?.flatten() {
        let ws_path = ws_entry.path();
        let name = ws_path.file_name().unwrap_or_default().to_string_lossy();
        if ws_path.is_dir() && name.starts_with("workspace") {
            result.total_workspaces += 1;
            if ws_path.join("MEMORY.md").is_file() {
                result.total_memory_md_files += 1;
            }
            // Count memory/*.md files too.
            let mem_dir = ws_path.join("memory");
            if mem_dir.is_dir() {
                if let Ok(md_entries) = fs::read_dir(&mem_dir) {
                    for md_entry in md_entries.flatten() {
                        if md_entry.path().extension().and_then(|e| e.to_str()) == Some("md") {
                            result.total_memory_md_files += 1;
                        }
                    }
                }
            }
            // Count skills.
            let skills_dir = ws_path.join("skills");
            if skills_dir.is_dir() {
                if let Ok(skill_entries) = fs::read_dir(&skills_dir) {
                    result.total_skills += skill_entries
                        .flatten()
                        .filter(|e| e.path().is_dir())
                        .count();
                }
            }
        }
    }

    // Scan memory SQLite databases (*.sqlite, *.db, brain.db in workspaces).
    let memory_dir = dir.join("memory");
    if memory_dir.is_dir() {
        if let Ok(mem_entries) = fs::read_dir(&memory_dir) {
            result.total_memory_dbs += mem_entries
                .flatten()
                .filter(|e| {
                    let p = e.path();
                    let ext = p.extension().and_then(|ext| ext.to_str()).unwrap_or("");
                    ext == "sqlite" || ext == "db"
                })
                .count();
        }
    }
    // Check workspace*/memory/brain.db.
    for ws_entry in fs::read_dir(dir)?.flatten() {
        let ws_path = ws_entry.path();
        let name = ws_path.file_name().unwrap_or_default().to_string_lossy();
        if ws_path.is_dir() && name.starts_with("workspace") {
            if ws_path.join("memory").join("brain.db").is_file() {
                result.total_memory_dbs += 1;
            }
        }
    }

    // Scan cron jobs.
    let cron_path = dir.join("cron/jobs.json");
    if cron_path.is_file() {
        if let Ok(data) = fs::read_to_string(&cron_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data) {
                result.total_cron_jobs = val["jobs"]
                    .as_array()
                    .map(|a| a.len())
                    .unwrap_or(0);
            }
        }
    }

    debug!(
        agents = result.agent_ids.len(),
        sessions = result.total_sessions,
        jsonl_files = result.total_jsonl_files,
        memories = result.total_memories,
        memory_md = result.total_memory_md_files,
        memory_dbs = result.total_memory_dbs,
        workspaces = result.total_workspaces,
        skills = result.total_skills,
        cron_jobs = result.total_cron_jobs,
        "OpenClaw scan complete"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Reading sessions.json
// ---------------------------------------------------------------------------

/// Parse a sessions.json file into a map of session key to descriptor.
pub fn read_sessions_index(path: &Path) -> Result<SessionsIndex> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("read sessions.json: {}", path.display()))?;
    let index: SessionsIndex = serde_json::from_str(&data)
        .with_context(|| format!("parse sessions.json: {}", path.display()))?;
    debug!(path = %path.display(), sessions = index.len(), "parsed sessions index");
    Ok(index)
}

// ---------------------------------------------------------------------------
// Reading JSONL session files
// ---------------------------------------------------------------------------

/// Parse a JSONL session file and extract message events.
/// Returns only events with type=message, converted to rsclaw format.
pub fn read_session_messages(jsonl_path: &Path) -> Result<Vec<ConvertedMessage>> {
    let file = fs::File::open(jsonl_path)
        .with_context(|| format!("open JSONL: {}", jsonl_path.display()))?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    path = %jsonl_path.display(),
                    line = line_num + 1,
                    error = %e,
                    "failed to read line"
                );
                continue;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: SessionEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                debug!(
                    path = %jsonl_path.display(),
                    line = line_num + 1,
                    error = %e,
                    "skipping unparseable JSONL line"
                );
                continue;
            }
        };

        if event.event_type != "message" {
            continue;
        }

        if let Some(msg) = event.message {
            let content = extract_text_content(&msg.content);
            if content.is_empty() {
                continue;
            }
            messages.push(ConvertedMessage {
                role: msg.role,
                content,
                model: msg.model,
                timestamp: event.timestamp,
            });
        }
    }

    debug!(
        path = %jsonl_path.display(),
        messages = messages.len(),
        "parsed session JSONL"
    );
    Ok(messages)
}

/// Extract text from the content field of a message.
///
/// Content can be:
/// - A simple string (typically for user messages)
/// - An array of content blocks (for assistant messages with text/thinking/tool_use)
fn extract_text_content(content: &Option<serde_json::Value>) -> String {
    match content {
        None => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(obj) = block.as_object() {
                    let block_type = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(text) = obj.get("text").and_then(|t| t.as_str()) {
                                parts.push(text.to_owned());
                            }
                        }
                        "thinking" => {
                            if let Some(text) = obj.get("thinking").and_then(|t| t.as_str()) {
                                parts.push(format!("[thinking] {text}"));
                            }
                        }
                        // Skip tool_use and tool_result blocks for import.
                        _ => {}
                    }
                }
            }
            parts.join("\n")
        }
        Some(_) => String::new(),
    }
}

/// Extract custom type=memory_put events from a JSONL file.
pub fn read_custom_memories(jsonl_path: &Path) -> Result<Vec<ConvertedMemory>> {
    let file = fs::File::open(jsonl_path)
        .with_context(|| format!("open JSONL for memories: {}", jsonl_path.display()))?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();

    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: SessionEvent = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if event.event_type != "custom" {
            continue;
        }
        if event.custom_type.as_deref() != Some("memory_put") {
            continue;
        }

        if let Some(data) = &event.data {
            let key = data
                .get("key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_owned();
            let value = data
                .get("value")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if !key.is_empty() && !value.is_empty() {
                memories.push(ConvertedMemory {
                    key,
                    value,
                    agent_id: String::new(), // filled in by caller
                });
            }
        }
    }

    Ok(memories)
}

// ---------------------------------------------------------------------------
// Import helpers
// ---------------------------------------------------------------------------

/// Resolve the JSONL file path for a session descriptor.
/// Tries `sessionFile` first (if it exists on disk), then falls back to
/// `<sessions_dir>/<session_id>.jsonl`. This handles cases where the
/// openclaw data was copied from another machine with different paths.
pub fn resolve_jsonl_path(sessions_dir: &Path, descriptor: &SessionDescriptor) -> PathBuf {
    // Try sessionFile as-is.
    if let Some(ref file_path) = descriptor.session_file {
        let p = PathBuf::from(file_path);
        if p.is_file() {
            return p;
        }
        // sessionFile path doesn't exist -- try just the filename in sessions_dir.
        if let Some(fname) = p.file_name() {
            let local = sessions_dir.join(fname);
            if local.is_file() {
                return local;
            }
        }
    }
    // Fallback: derive from session_id.
    sessions_dir.join(format!("{}.jsonl", descriptor.session_id))
}

/// Build a rsclaw session key from the OpenClaw session key.
///
/// OpenClaw keys vary in format:
///   "main"                         -> "agent:{agent_id}:main"
///   "agent:main:main"              -> "agent:main:main" (already rsclaw-compatible)
///   "agent:main:telegram:direct:x" -> keep as-is
///
/// The goal is to produce keys that match rsclaw's `derive_session_key` output
/// so imported history is found when the user continues chatting.
pub fn make_rsclaw_session_key(openclaw_key: &str, agent_id: &str) -> String {
    if openclaw_key.starts_with("agent:") {
        // Already in rsclaw-compatible format.
        openclaw_key.to_owned()
    } else {
        // Bare key like "main" -> wrap as agent:{id}:main scope.
        format!("agent:{agent_id}:{openclaw_key}")
    }
}

/// Generate session key aliases for migration compatibility.
///
/// OpenClaw keys include accountId: `agent:main:feishu:default:direct:ou_xxx`
/// rsclaw's default per-channel-peer omits it: `agent:main:feishu:direct:ou_xxx`
///
/// Also handles channel name remapping (e.g. openclaw-weixin -> wechat).
///
/// Returns a list of (alias_key, canonical_key) pairs where alias_key is what
/// rsclaw would generate and canonical_key is the actual stored key.
pub fn generate_session_aliases(
    sessions: &[ConvertedSession],
    channel_remap: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut aliases = Vec::new();

    for session in sessions {
        let stored_key = &session.session_key;
        let parts: Vec<&str> = stored_key.split(':').collect();

        // Only process keys with enough segments that might need aliasing.
        // Format: agent:{agentId}:{channel}:{accountId}:direct:{peerId}
        //         0      1         2         3          4      5
        // Or:     agent:{agentId}:{channel}:group:{groupId}
        //         0      1         2        3     4

        if parts.len() < 3 || parts[0] != "agent" {
            continue;
        }

        // Check for DM with accountId: 6 segments with "direct" at index 4
        if parts.len() == 6 && parts[4] == "direct" {
            let agent_id = parts[1];
            let channel = parts[2];
            let _account_id = parts[3];
            let peer_id = parts[5];

            // Alias 1: without accountId (per-channel-peer format)
            let no_account = format!("agent:{agent_id}:{channel}:direct:{peer_id}");
            if no_account != *stored_key {
                aliases.push((no_account, stored_key.clone()));
            }

            // Alias 2: with remapped channel name
            if let Some(new_channel) = channel_remap.get(channel) {
                let remapped = format!("agent:{agent_id}:{new_channel}:direct:{peer_id}");
                aliases.push((remapped, stored_key.clone()));
                // Also with accountId
                let remapped_with_acc = format!("agent:{agent_id}:{new_channel}:{_account_id}:direct:{peer_id}");
                aliases.push((remapped_with_acc, stored_key.clone()));
            }
        }

        // Check for group: 5 segments with "group" at index 3
        if parts.len() == 5 && parts[3] == "group" {
            let agent_id = parts[1];
            let channel = parts[2];
            let group_id = parts[4];

            // Only need channel remap for groups (format is identical otherwise)
            if let Some(new_channel) = channel_remap.get(channel) {
                let remapped = format!("agent:{agent_id}:{new_channel}:group:{group_id}");
                aliases.push((remapped, stored_key.clone()));
            }
        }
    }

    aliases
}

/// Read all sessions for a given agent directory and convert them.
pub fn read_agent_sessions(agent_dir: &Path, agent_id: &str) -> Result<Vec<ConvertedSession>> {
    let sessions_dir = agent_dir.join("sessions");
    let index_path = sessions_dir.join("sessions.json");

    if !index_path.is_file() {
        debug!(agent = %agent_id, "no sessions.json found");
        return Ok(Vec::new());
    }

    let index = read_sessions_index(&index_path)?;
    let mut converted = Vec::new();

    for (session_key, descriptor) in &index {
        let jsonl_path = resolve_jsonl_path(&sessions_dir, descriptor);
        if !jsonl_path.is_file() {
            debug!(
                agent = %agent_id,
                session = %descriptor.session_id,
                path = %jsonl_path.display(),
                "JSONL file not found, skipping"
            );
            continue;
        }

        match read_session_messages(&jsonl_path) {
            Ok(messages) => {
                if messages.is_empty() {
                    debug!(
                        agent = %agent_id,
                        session = %descriptor.session_id,
                        "session has no messages, skipping"
                    );
                    continue;
                }

                let rsclaw_key = make_rsclaw_session_key(session_key, agent_id);
                converted.push(ConvertedSession {
                    session_key: rsclaw_key,
                    openclaw_session_id: descriptor.session_id.clone(),
                    agent_id: agent_id.to_owned(),
                    messages,
                    updated_at: descriptor.updated_at,
                });
            }
            Err(e) => {
                warn!(
                    agent = %agent_id,
                    session = %descriptor.session_id,
                    error = %e,
                    "failed to read session JSONL"
                );
            }
        }
    }

    info!(
        agent = %agent_id,
        sessions = converted.len(),
        "read agent sessions"
    );
    Ok(converted)
}

/// Read all memory_put entries across all JSONL files for an agent.
pub fn read_agent_memories(agent_dir: &Path, agent_id: &str) -> Result<Vec<ConvertedMemory>> {
    let sessions_dir = agent_dir.join("sessions");
    if !sessions_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut all_memories = Vec::new();
    let entries = fs::read_dir(&sessions_dir)?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        match read_custom_memories(&path) {
            Ok(mut memories) => {
                for m in &mut memories {
                    m.agent_id = agent_id.to_owned();
                }
                all_memories.extend(memories);
            }
            Err(e) => {
                debug!(
                    path = %path.display(),
                    error = %e,
                    "failed to read memories from JSONL"
                );
            }
        }
    }

    info!(
        agent = %agent_id,
        memories = all_memories.len(),
        "read agent memories"
    );
    Ok(all_memories)
}

/// Import all data from an OpenClaw directory into rsclaw stores.
///
/// Reads JSONL session files and writes messages into rsclaw's redb store.
/// Memory entries are collected but require async MemoryStore for full import.
pub fn import_sessions_to_redb(
    openclaw_dir: &Path,
    store: &crate::store::redb_store::RedbStore,
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let mut all_sessions: Vec<ConvertedSession> = Vec::new();

    let agents_dir = openclaw_dir.join("agents");
    if !agents_dir.is_dir() {
        info!("no agents/ directory in OpenClaw dir");
        return Ok(stats);
    }

    let entries = fs::read_dir(&agents_dir)?;
    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if !agent_dir.is_dir() {
            continue;
        }
        let agent_id = agent_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Import sessions.
        match read_agent_sessions(&agent_dir, &agent_id) {
            Ok(sessions) => {
                for session in &sessions {
                    for msg in &session.messages {
                        let json_msg = serde_json::json!({
                            "role": msg.role,
                            "content": msg.content,
                            "model": msg.model,
                            "timestamp": msg.timestamp,
                            "source": "openclaw",
                        });
                        match store.append_message(&session.session_key, &json_msg) {
                            Ok(_) => stats.messages += 1,
                            Err(e) => {
                                warn!(
                                    session = %session.session_key,
                                    error = %e,
                                    "failed to import message"
                                );
                                stats.errors += 1;
                            }
                        }
                    }
                    stats.sessions += 1;
                }
                all_sessions.extend(sessions);
            }
            Err(e) => {
                warn!(agent = %agent_id, error = %e, "failed to read agent sessions");
                stats.errors += 1;
            }
        }

        // Count memories (actual insertion needs async MemoryStore).
        match read_agent_memories(&agent_dir, &agent_id) {
            Ok(memories) => {
                stats.memories += memories.len();
            }
            Err(e) => {
                warn!(agent = %agent_id, error = %e, "failed to read agent memories");
            }
        }
    }

    // Generate and store session aliases for migration compatibility.
    // Channel name remapping: openclaw channel names → rsclaw channel names.
    let mut channel_remap = std::collections::HashMap::new();
    channel_remap.insert("openclaw-weixin".to_owned(), "wechat".to_owned());
    // Add more remaps as needed.

    let aliases = generate_session_aliases(&all_sessions, &channel_remap);
    if !aliases.is_empty() {
        let alias_refs: Vec<(&str, &str)> = aliases
            .iter()
            .map(|(a, c)| (a.as_str(), c.as_str()))
            .collect();
        store.put_session_aliases(&alias_refs)?;
        info!(count = aliases.len(), "session aliases written");
        stats.aliases = aliases.len();
    }

    info!(
        sessions = stats.sessions,
        messages = stats.messages,
        memories = stats.memories,
        aliases = stats.aliases,
        errors = stats.errors,
        "OpenClaw JSONL import complete"
    );

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Workspace file migration
// ---------------------------------------------------------------------------

/// Workspace .md files to copy (MEMORY.md is also copied as-is for system prompt,
/// AND parsed into memory store for vector search).
const WORKSPACE_FILES: &[&str] = &[
    "SOUL.md",
    "IDENTITY.md",
    "AGENTS.md",
    "USER.md",
    "MEMORY.md",
];

/// Copy workspace .md files from an OpenClaw workspace dir to rsclaw workspace.
pub fn copy_workspace_files(
    src_workspace: &Path,
    dst_workspace: &Path,
) -> Result<usize> {
    if !src_workspace.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(dst_workspace)?;
    let mut count = 0;

    for filename in WORKSPACE_FILES {
        let src = src_workspace.join(filename);
        if src.is_file() {
            let dst = dst_workspace.join(filename);
            if !dst.exists() {
                std::fs::copy(&src, &dst)?;
                debug!(file = %filename, "copied workspace file");
                count += 1;
            } else {
                debug!(file = %filename, "skipped (already exists)");
            }
        }
    }
    Ok(count)
}

/// Copy installed skills directory.
pub fn copy_skills(src_workspace: &Path, dst_workspace: &Path) -> Result<usize> {
    let src_skills = src_workspace.join("skills");
    if !src_skills.is_dir() {
        return Ok(0);
    }
    let dst_skills = dst_workspace.join("skills");
    std::fs::create_dir_all(&dst_skills)?;

    let mut count = 0;
    for entry in fs::read_dir(&src_skills)?.flatten() {
        let src_path = entry.path();
        if !src_path.is_dir() {
            continue;
        }
        let name = src_path.file_name().unwrap_or_default();
        let dst_path = dst_skills.join(name);
        if !dst_path.exists() {
            copy_dir_recursive(&src_path, &dst_path)?;
            count += 1;
            debug!(skill = ?name, "copied skill");
        }
    }
    Ok(count)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Memory migration (MEMORY.md + memory/*.md → MemoryDoc entries)
// ---------------------------------------------------------------------------

/// Parsed memory entry from markdown splitting.
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// Heading text used as the memory key/title.
    pub title: String,
    /// Content under the heading.
    pub content: String,
    /// Source agent ID.
    pub agent_id: String,
    /// Source file path (for dedup/debugging).
    pub source_file: String,
}

/// Read MEMORY.md and memory/*.md from a workspace, split by ## headings.
pub fn read_workspace_memories(
    workspace_dir: &Path,
    agent_id: &str,
) -> Result<Vec<MemoryEntry>> {
    let mut entries = Vec::new();

    // 1. MEMORY.md
    let memory_md = workspace_dir.join("MEMORY.md");
    if memory_md.is_file() {
        let content = fs::read_to_string(&memory_md)?;
        let mut split = split_markdown_by_headings(&content, agent_id, "MEMORY.md");
        entries.append(&mut split);
    }

    // 2. memory/*.md
    let memory_dir = workspace_dir.join("memory");
    if memory_dir.is_dir() {
        if let Ok(dir_entries) = fs::read_dir(&memory_dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let filename = path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if let Ok(content) = fs::read_to_string(&path) {
                        let mut split = split_markdown_by_headings(
                            &content, agent_id, &filename,
                        );
                        entries.append(&mut split);
                    }
                }
            }
        }
    }

    info!(
        agent = %agent_id,
        entries = entries.len(),
        "read workspace memories"
    );
    Ok(entries)
}

/// Split markdown content by `##` headings into separate entries.
/// If no `##` headings found, treat the entire content as one entry.
fn split_markdown_by_headings(
    content: &str,
    agent_id: &str,
    source_file: &str,
) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    let mut current_title = String::new();
    let mut current_lines: Vec<&str> = Vec::new();

    for line in content.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            // Flush previous section.
            if !current_lines.is_empty() {
                let text = current_lines.join("\n").trim().to_owned();
                if !text.is_empty() {
                    entries.push(MemoryEntry {
                        title: if current_title.is_empty() {
                            source_file.to_owned()
                        } else {
                            current_title.clone()
                        },
                        content: text,
                        agent_id: agent_id.to_owned(),
                        source_file: source_file.to_owned(),
                    });
                }
            }
            current_title = heading.trim().to_owned();
            current_lines.clear();
        } else if line.starts_with("# ") && current_title.is_empty() && current_lines.is_empty() {
            // Skip top-level `# Title` header (document title, not a memory entry).
        } else {
            current_lines.push(line);
        }
    }

    // Flush last section.
    if !current_lines.is_empty() {
        let text = current_lines.join("\n").trim().to_owned();
        if !text.is_empty() {
            entries.push(MemoryEntry {
                title: if current_title.is_empty() {
                    source_file.to_owned()
                } else {
                    current_title.clone()
                },
                content: text,
                agent_id: agent_id.to_owned(),
                source_file: source_file.to_owned(),
            });
        }
    }

    entries
}

/// Read memory from an OpenClaw SQLite database.
/// Supports both new format (chunks table) and legacy format (memories table).
#[cfg(feature = "openclaw-migrate")]
pub fn read_sqlite_memories(
    db_path: &Path,
    agent_id: &str,
) -> Result<Vec<MemoryEntry>> {
    use rusqlite::Connection;

    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let mut entries = Vec::new();

    // Try new format: chunks table (path + text).
    if has_table(&conn, "chunks") {
        let mut stmt = conn.prepare(
            "SELECT path, text FROM chunks WHERE text != '' ORDER BY updated_at DESC"
        )?;
        let rows: Vec<MemoryEntry> = stmt
            .query_map([], |row| {
                let path: String = row.get(0)?;
                let text: String = row.get(1)?;
                Ok(MemoryEntry {
                    title: path,
                    content: text,
                    agent_id: agent_id.to_owned(),
                    source_file: format!("sqlite:chunks:{}", crate::config::loader::path_to_forward_slash(db_path)),
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|e| !e.content.trim().is_empty())
            .collect();
        entries.extend(rows);
    }

    // Try legacy format: memories table (key/content or key/value).
    if has_table(&conn, "memories") {
        let columns = table_columns(&conn, "memories")?;
        let key_expr = pick_column(&columns, &["key", "id", "name"])
            .unwrap_or_else(|| "CAST(rowid AS TEXT)".to_owned());
        let content_expr = pick_column(&columns, &["content", "value", "text", "memory"]);
        if let Some(content_col) = content_expr {
            let sql = format!(
                "SELECT {key_expr} AS k, {content_col} AS v FROM memories"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows: Vec<MemoryEntry> = stmt
                .query_map([], |row| {
                    let key: String = row.get(0).unwrap_or_default();
                    let content: String = row.get(1).unwrap_or_default();
                    Ok(MemoryEntry {
                        title: key,
                        content,
                        agent_id: agent_id.to_owned(),
                        source_file: format!("sqlite:memories:{}", crate::config::loader::path_to_forward_slash(db_path)),
                    })
                })?
                .filter_map(|r| r.ok())
                .filter(|e| !e.content.trim().is_empty())
                .collect();
            entries.extend(rows);
        }
    }

    // Try legacy format: brain.db in workspace/memory/
    // (handled by the same logic above if the path points to brain.db)

    info!(
        db = %db_path.display(),
        agent = %agent_id,
        entries = entries.len(),
        "read SQLite memory"
    );
    Ok(entries)
}

#[cfg(feature = "openclaw-migrate")]
fn has_table(conn: &rusqlite::Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT name FROM sqlite_master WHERE type='table' AND name=?1 LIMIT 1",
        [name],
        |_| Ok(()),
    ).is_ok()
}

#[cfg(feature = "openclaw-migrate")]
fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let cols: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    Ok(cols)
}

#[cfg(feature = "openclaw-migrate")]
fn pick_column(columns: &[String], candidates: &[&str]) -> Option<String> {
    candidates
        .iter()
        .find(|c| columns.iter().any(|col| col == *c))
        .map(|s| s.to_string())
}

/// Collect all memory entries from all sources for an OpenClaw directory.
pub fn collect_all_memories(openclaw_dir: &Path, config_json: &str) -> Result<Vec<MemoryEntry>> {
    let mut all = Vec::new();

    // Parse agent list from config to find workspace paths.
    let config: serde_json::Value = json5::from_str(config_json)
        .or_else(|_| serde_json::from_str(config_json))
        .unwrap_or_default();

    let default_workspace = openclaw_dir.join("workspace");

    // Collect from each agent's workspace.
    if let Some(agents) = config.pointer("/agents/list").and_then(|v| v.as_array()) {
        for agent in agents {
            let agent_id = agent.get("id").and_then(|v| v.as_str()).unwrap_or("main");
            let workspace_path = agent
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(|p| {
                    let expanded = if let Some(rest) = p.strip_prefix("~/") {
                        dirs_next::home_dir().unwrap_or_default().join(rest)
                    } else {
                        PathBuf::from(p)
                    };
                    // If absolute path doesn't exist, try remapping to current
                    // openclaw dir (handles configs copied from another machine).
                    if expanded.is_dir() {
                        expanded
                    } else if let Some(dirname) = expanded.file_name() {
                        let remapped = openclaw_dir.join(dirname);
                        if remapped.is_dir() {
                            info!(
                                original = %expanded.display(),
                                remapped = %remapped.display(),
                                "workspace path remapped"
                            );
                            remapped
                        } else {
                            expanded
                        }
                    } else {
                        expanded
                    }
                })
                .unwrap_or_else(|| default_workspace.clone());

            if let Ok(mut entries) = read_workspace_memories(&workspace_path, agent_id) {
                all.append(&mut entries);
            }
        }
    } else {
        // No agent list, try default workspace.
        if let Ok(mut entries) = read_workspace_memories(&default_workspace, "main") {
            all.append(&mut entries);
        }
    }

    // Fallback: scan all workspace-* dirs not covered by config.
    if let Ok(dir_entries) = fs::read_dir(openclaw_dir) {
        let known: std::collections::HashSet<String> = all.iter().map(|e| e.agent_id.clone()).collect();
        for entry in dir_entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if path.is_dir() && name.starts_with("workspace-") {
                let agent_id = name.strip_prefix("workspace-").unwrap_or(&name);
                if !known.contains(agent_id) {
                    if let Ok(mut entries) = read_workspace_memories(&path, agent_id) {
                        all.append(&mut entries);
                    }
                }
            }
        }
    }

    // Collect from SQLite memory databases.
    // Locations: memory/*.sqlite, memory/*.db, workspace*/memory/brain.db
    let mut sqlite_paths: Vec<(PathBuf, String)> = Vec::new();

    // 1. memory/ dir at root level.
    let memory_dir = openclaw_dir.join("memory");
    if memory_dir.is_dir() {
        if let Ok(dir_entries) = fs::read_dir(&memory_dir) {
            for entry in dir_entries.flatten() {
                let path = entry.path();
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if ext == "sqlite" || ext == "db" {
                    let agent_id = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    // brain.db -> "main"
                    let agent_id = if agent_id == "brain" { "main".to_owned() } else { agent_id };
                    sqlite_paths.push((path, agent_id));
                }
            }
        }
    }

    // 2. workspace*/memory/brain.db (legacy per-workspace location).
    if let Ok(dir_entries) = fs::read_dir(openclaw_dir) {
        for entry in dir_entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            if path.is_dir() && name.starts_with("workspace") {
                let brain_db = path.join("memory").join("brain.db");
                if brain_db.is_file() {
                    let agent_id = name.strip_prefix("workspace-")
                        .unwrap_or("main")
                        .to_owned();
                    sqlite_paths.push((brain_db, agent_id));
                }
            }
        }
    }

    for (db_path, agent_id) in &sqlite_paths {
        #[cfg(feature = "openclaw-migrate")]
        if let Ok(mut entries) = read_sqlite_memories(db_path, agent_id) {
            all.append(&mut entries);
        }

        #[cfg(not(feature = "openclaw-migrate"))]
        {
            warn!(
                db = %db_path.display(),
                "SQLite memory found but openclaw-migrate feature not enabled, skipping"
            );
        }
    }

    // De-duplicate by content.
    let mut seen = std::collections::HashSet::new();
    all.retain(|e| seen.insert(format!("{}:{}", e.title, e.content)));

    info!(total = all.len(), "collected all memory entries");
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_string_content() {
        let content = Some(serde_json::Value::String("hello world".to_owned()));
        assert_eq!(extract_text_content(&content), "hello world");
    }

    #[test]
    fn extract_array_content() {
        let content = Some(serde_json::json!([
            {"type": "text", "text": "Hello"},
            {"type": "thinking", "thinking": "I should greet"},
            {"type": "tool_use", "name": "bash", "input": {}}
        ]));
        let result = extract_text_content(&content);
        assert!(result.contains("Hello"));
        assert!(result.contains("[thinking]"));
        assert!(!result.contains("bash"));
    }

    #[test]
    fn extract_none_content() {
        assert_eq!(extract_text_content(&None), "");
    }

    #[test]
    fn parse_session_event_message() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2025-01-01","message":{"role":"user","content":"hello"}}"#;
        let event: SessionEvent = serde_json::from_str(line).expect("parse");
        assert_eq!(event.event_type, "message");
        assert_eq!(event.message.as_ref().expect("msg").role, "user");
    }

    #[test]
    fn parse_session_event_custom_memory() {
        let line = r#"{"type":"custom","id":"def","customType":"memory_put","data":{"key":"user_name","value":"Alice"}}"#;
        let event: SessionEvent = serde_json::from_str(line).expect("parse");
        assert_eq!(event.event_type, "custom");
        assert_eq!(event.custom_type.as_deref(), Some("memory_put"));
        let data = event.data.expect("data");
        assert_eq!(data["key"].as_str(), Some("user_name"));
        assert_eq!(data["value"].as_str(), Some("Alice"));
    }

    #[test]
    fn make_session_key_bare() {
        // Bare key "main" -> "agent:default:main"
        let key = make_rsclaw_session_key("main", "default");
        assert_eq!(key, "agent:default:main");
    }

    #[test]
    fn make_session_key_already_prefixed() {
        // Already has "agent:" prefix -> keep as-is
        let key = make_rsclaw_session_key("agent:main:main", "default");
        assert_eq!(key, "agent:main:main");
    }

    #[test]
    fn make_session_key_channel_format() {
        let key = make_rsclaw_session_key("agent:main:telegram:direct:12345", "main");
        assert_eq!(key, "agent:main:telegram:direct:12345");
    }
}
