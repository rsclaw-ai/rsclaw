//! Workspace file loading (AGENTS.md §14 + §20).
//!
//! Loads the standard workspace markdown files into a `WorkspaceContext`
//! struct that is merged into the system prompt.
//!
//! File loading rules:
//!   - Missing files inject a `[<FILE>.md not found — using defaults]` marker.
//!   - Per-file character limit: `bootstrap_max_chars` (default 20 000).
//!   - Total character limit:    `bootstrap_total_max_chars` (default 150 000).
//!   - Load order follows AGENTS.md §20 "System Prompt assembly".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::Local;
use tracing::debug;

/// Default per-file character limit.
pub const DEFAULT_MAX_CHARS_PER_FILE: usize = 20_000;
/// Default total character limit across all workspace files.
pub const DEFAULT_TOTAL_MAX_CHARS: usize = 150_000;

// ---------------------------------------------------------------------------
// SessionType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionType {
    /// Regular user DM or group message.
    Normal,
    /// Scheduled heartbeat run.
    Heartbeat,
    /// First run after gateway restart.
    Boot,
    /// First ever run on a brand-new workspace (BOOTSTRAP.md present).
    Bootstrap,
}

// ---------------------------------------------------------------------------
// WorkspaceContext
// ---------------------------------------------------------------------------

/// Contents of the workspace markdown files, ready to be injected into
/// the system prompt.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceContext {
    /// AGENTS.md — loaded every session.
    pub agents_md: Option<String>,
    /// SOUL.md — loaded every session.
    pub soul_md: Option<String>,
    /// USER.md — loaded every session.
    pub user_md: Option<String>,
    /// IDENTITY.md — loaded every session.
    pub identity_md: Option<String>,
    /// TOOLS.md — loaded every session.
    pub tools_md: Option<String>,
    /// HEARTBEAT.md — loaded only during heartbeat runs.
    pub heartbeat_md: Option<String>,
    /// BOOT.md — loaded only after gateway restart.
    pub boot_md: Option<String>,
    /// BOOTSTRAP.md — loaded only on a brand-new workspace.
    pub bootstrap_md: Option<String>,
    /// MEMORY.md — loaded only in private (DM) sessions.
    pub memory_long: Option<String>,
    /// `memory/YYYY-MM-DD.md` — today's daily memory log.
    pub memory_today: Option<String>,
    /// `memory/YYYY-MM-DD.md` — yesterday's daily memory log.
    pub memory_yesterday: Option<String>,
    /// Workspace root directory.
    pub workspace_dir: PathBuf,
}

impl WorkspaceContext {
    /// Load workspace files from `workspace`.
    ///
    /// `session_type` controls which optional files are loaded.
    /// `is_private` controls whether `MEMORY.md` is loaded.
    ///
    /// If a file is missing from the agent's workspace, it falls back to
    /// the default workspace (inheritance). Pass `None` for default agents.
    pub fn load(
        workspace: &Path,
        session_type: SessionType,
        is_private: bool,
        max_chars_per_file: usize,
        total_max_chars: usize,
    ) -> Self {
        // Determine fallback workspace: the default "workspace" directory.
        // Only used if `workspace` is an agent-specific directory (workspace-xxx).
        let fallback = {
            let default_ws = crate::config::loader::base_dir().join("workspace");
            if workspace != default_ws && default_ws.exists() {
                Some(default_ws)
            } else {
                None
            }
        };

        let mut ctx = WorkspaceContext {
            workspace_dir: workspace.to_path_buf(),
            ..Default::default()
        };

        let mut total_chars: usize = 0;

        // Helper macro: read a file, apply per-file limit, accumulate total.
        // Falls back to the default workspace if not found in the agent's workspace.
        macro_rules! load_file {
            ($field:ident, $filename:expr) => {{
                if total_chars < total_max_chars {
                    let content = read_workspace_file(
                        workspace,
                        $filename,
                        max_chars_per_file,
                        &mut total_chars,
                        total_max_chars,
                    );
                    let content = if content.is_empty() {
                        if let Some(ref fb) = fallback {
                            read_workspace_file(
                                fb,
                                $filename,
                                max_chars_per_file,
                                &mut total_chars,
                                total_max_chars,
                            )
                        } else {
                            content
                        }
                    } else {
                        content
                    };
                    if !content.is_empty() {
                        ctx.$field = Some(content);
                    }
                }
            }};
        }

        // Always-loaded files.
        load_file!(agents_md, "AGENTS.md");
        load_file!(soul_md, "SOUL.md");
        load_file!(user_md, "USER.md");
        load_file!(identity_md, "IDENTITY.md");
        load_file!(tools_md, "TOOLS.md");

        // Conditional files.
        if session_type == SessionType::Heartbeat {
            load_file!(heartbeat_md, "HEARTBEAT.md");
        }
        if session_type == SessionType::Boot {
            load_file!(boot_md, "BOOT.md");
        }
        if session_type == SessionType::Bootstrap {
            load_file!(bootstrap_md, "BOOTSTRAP.md");
        }
        if is_private {
            load_file!(memory_long, "MEMORY.md");
        }

        // Daily memory logs.
        let today = Local::now().format("%Y-%m-%d").to_string();
        let yesterday = (Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        ctx.memory_today = read_optional_file(
            &workspace.join("memory").join(format!("{today}.md")),
            max_chars_per_file,
            &mut total_chars,
            total_max_chars,
        );
        ctx.memory_yesterday = read_optional_file(
            &workspace.join("memory").join(format!("{yesterday}.md")),
            max_chars_per_file,
            &mut total_chars,
            total_max_chars,
        );

        debug!(
            workspace = %workspace.display(),
            total_chars,
            "workspace context loaded"
        );

        ctx
    }

    /// Build the system-prompt segment from loaded workspace files.
    /// Each non-None file is wrapped with a header.
    pub fn to_prompt_segment(&self) -> String {
        let mut parts: Vec<String> = Vec::new();

        macro_rules! append {
            ($field:expr, $label:expr) => {
                if let Some(content) = &$field {
                    parts.push(format!("## {}\n\n{content}", $label));
                }
            };
        }

        append!(self.agents_md, "AGENTS.md");
        append!(self.soul_md, "SOUL.md");
        append!(self.identity_md, "IDENTITY.md");
        append!(self.user_md, "USER.md");
        append!(self.tools_md, "TOOLS.md");
        append!(self.heartbeat_md, "HEARTBEAT.md");
        append!(self.boot_md, "BOOT.md");
        append!(self.bootstrap_md, "BOOTSTRAP.md");
        append!(self.memory_long, "MEMORY.md");
        append!(self.memory_today, "Memory (today)");
        append!(self.memory_yesterday, "Memory (yesterday)");

        parts.join("\n\n---\n\n")
    }
}

// ---------------------------------------------------------------------------
// Workspace cache (P1 context optimisation)
// ---------------------------------------------------------------------------

/// In-memory cache for workspace files. Re-reads only files whose mtime
/// has changed since the last load, avoiding redundant disk IO every turn.
#[derive(Debug)]
pub struct WorkspaceCache {
    /// Cached file contents keyed by filename.
    entries: HashMap<String, CacheEntry>,
    /// Last fully-assembled `WorkspaceContext`.
    cached_ctx: Option<WorkspaceContext>,
    /// Workspace root.
    workspace: PathBuf,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    content: String,
    mtime: Option<SystemTime>,
}

impl WorkspaceCache {
    pub fn new(workspace: &Path) -> Self {
        Self {
            entries: HashMap::new(),
            cached_ctx: None,
            workspace: workspace.to_path_buf(),
        }
    }

    /// Return a `WorkspaceContext`, reloading only files whose mtime changed.
    pub fn load(
        &mut self,
        session_type: SessionType,
        is_private: bool,
        max_chars_per_file: usize,
        total_max_chars: usize,
    ) -> WorkspaceContext {
        let mut total_chars: usize = 0;
        let mut ctx = WorkspaceContext {
            workspace_dir: self.workspace.clone(),
            ..Default::default()
        };

        macro_rules! load_cached {
            ($field:ident, $filename:expr) => {{
                if total_chars < total_max_chars {
                    let content = self.read_cached(
                        $filename,
                        max_chars_per_file,
                        &mut total_chars,
                        total_max_chars,
                    );
                    if !content.is_empty() {
                        ctx.$field = Some(content);
                    }
                }
            }};
        }

        load_cached!(agents_md, "AGENTS.md");
        load_cached!(soul_md, "SOUL.md");
        load_cached!(user_md, "USER.md");
        load_cached!(identity_md, "IDENTITY.md");
        load_cached!(tools_md, "TOOLS.md");

        if session_type == SessionType::Heartbeat {
            load_cached!(heartbeat_md, "HEARTBEAT.md");
        }
        if session_type == SessionType::Boot {
            load_cached!(boot_md, "BOOT.md");
        }
        if session_type == SessionType::Bootstrap {
            load_cached!(bootstrap_md, "BOOTSTRAP.md");
        }
        if is_private {
            load_cached!(memory_long, "MEMORY.md");
        }

        // Daily memory logs (always re-read since date changes).
        let today = Local::now().format("%Y-%m-%d").to_string();
        let yesterday = (Local::now() - chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();

        ctx.memory_today = read_optional_file(
            &self.workspace.join("memory").join(format!("{today}.md")),
            max_chars_per_file,
            &mut total_chars,
            total_max_chars,
        );
        ctx.memory_yesterday = read_optional_file(
            &self
                .workspace
                .join("memory")
                .join(format!("{yesterday}.md")),
            max_chars_per_file,
            &mut total_chars,
            total_max_chars,
        );

        debug!(
            workspace = %self.workspace.display(),
            total_chars,
            "workspace context loaded (cached)"
        );

        self.cached_ctx = Some(ctx.clone());
        ctx
    }

    /// Read a file, returning cached content if mtime hasn't changed.
    fn read_cached(
        &mut self,
        filename: &str,
        max_chars: usize,
        total_chars: &mut usize,
        total_max: usize,
    ) -> String {
        let path = self.workspace.join(filename);
        let current_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok());

        // Check if cache is still valid.
        if let Some(entry) = self.entries.get(filename) {
            if entry.mtime == current_mtime && current_mtime.is_some() {
                let remaining = total_max.saturating_sub(*total_chars);
                let limit = max_chars.min(remaining);
                let truncated = truncate_chars(&entry.content, limit);
                *total_chars += truncated.len();
                return truncated.to_owned();
            }
        }

        // Cache miss or mtime changed -- re-read from disk.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => String::new(),
        };

        self.entries.insert(
            filename.to_owned(),
            CacheEntry {
                content: content.clone(),
                mtime: current_mtime,
            },
        );

        let remaining = total_max.saturating_sub(*total_chars);
        let limit = max_chars.min(remaining);
        let truncated = truncate_chars(&content, limit);
        *total_chars += truncated.len();
        truncated.to_owned()
    }
}

// ---------------------------------------------------------------------------
// File reading helpers
// ---------------------------------------------------------------------------

/// Read a workspace file, truncating to `max_chars`. If missing, return
/// the standard placeholder string.
fn read_workspace_file(
    workspace: &Path,
    filename: &str,
    max_chars: usize,
    total_chars: &mut usize,
    total_max: usize,
) -> String {
    let path = workspace.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let remaining = total_max.saturating_sub(*total_chars);
            let limit = max_chars.min(remaining);
            let truncated = truncate_chars(&content, limit);
            *total_chars += truncated.len();
            truncated.to_owned()
        }
        Err(_) => String::new(),
    }
}

/// Read an optional file (daily memory). Returns `None` if the file does
/// not exist or the total budget is exhausted.
fn read_optional_file(
    path: &Path,
    max_chars: usize,
    total_chars: &mut usize,
    total_max: usize,
) -> Option<String> {
    if !path.exists() || *total_chars >= total_max {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let remaining = total_max.saturating_sub(*total_chars);
    let limit = max_chars.min(remaining);
    let truncated = truncate_chars(&content, limit).to_owned();
    *total_chars += truncated.len();
    Some(truncated)
}

/// Truncate a string to at most `max_chars` Unicode characters.
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        return s;
    }
    // Find the byte offset of the `max_chars`-th char boundary.
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn setup_workspace(dir: &Path) {
        fs::write(dir.join("AGENTS.md"), "# Agents\nDo stuff.").expect("agents");
        fs::write(dir.join("SOUL.md"), "# Soul\nBe helpful.").expect("soul");
        fs::write(dir.join("USER.md"), "# User\nJane.").expect("user");
        fs::write(dir.join("IDENTITY.md"), "# Identity\nBot.").expect("identity");
        fs::write(dir.join("TOOLS.md"), "# Tools\nNone.").expect("tools");
    }

    #[test]
    fn loads_standard_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        setup_workspace(tmp.path());

        let ctx = WorkspaceContext::load(
            tmp.path(),
            SessionType::Normal,
            false,
            DEFAULT_MAX_CHARS_PER_FILE,
            DEFAULT_TOTAL_MAX_CHARS,
        );

        assert!(ctx.agents_md.as_deref().unwrap_or("").contains("Agents"));
        assert!(ctx.soul_md.as_deref().unwrap_or("").contains("Soul"));
        assert!(
            ctx.heartbeat_md.is_none(),
            "heartbeat not loaded in Normal mode"
        );
    }

    #[test]
    fn missing_file_gets_placeholder() {
        // Use a "workspace" sub-dir inside tempdir so that base_dir()
        // fallback won't find the real ~/.rsclaw/workspace/SOUL.md.
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = tmp.path().join("workspace");
        fs::create_dir_all(&ws).expect("mkdir");
        // Write only AGENTS.md; others should get placeholders.
        fs::write(ws.join("AGENTS.md"), "# Agents").expect("agents");

        // Temporarily override RSCLAW_BASE_DIR so base_dir() resolves inside
        // the temp directory, preventing fallback to the real workspace.
        let orig = std::env::var("RSCLAW_BASE_DIR").ok();
        // SAFETY: test is single-threaded for this env var access.
        unsafe { std::env::set_var("RSCLAW_BASE_DIR", tmp.path()); }

        let ctx = WorkspaceContext::load(
            &ws,
            SessionType::Normal,
            false,
            DEFAULT_MAX_CHARS_PER_FILE,
            DEFAULT_TOTAL_MAX_CHARS,
        );

        // Restore env.
        // SAFETY: test is single-threaded for this env var access.
        unsafe {
            match orig {
                Some(v) => std::env::set_var("RSCLAW_BASE_DIR", v),
                None => std::env::remove_var("RSCLAW_BASE_DIR"),
            }
        }

        // Missing file → field is None (no fallback available).
        assert!(
            ctx.soul_md.is_none(),
            "expected None for missing SOUL.md, got: {:?}",
            ctx.soul_md
        );
    }

    #[test]
    fn heartbeat_loads_heartbeat_md() {
        let tmp = tempfile::tempdir().expect("tempdir");
        setup_workspace(tmp.path());
        fs::write(tmp.path().join("HEARTBEAT.md"), "# Heartbeat\nchecklist").expect("hb");

        let ctx = WorkspaceContext::load(
            tmp.path(),
            SessionType::Heartbeat,
            false,
            DEFAULT_MAX_CHARS_PER_FILE,
            DEFAULT_TOTAL_MAX_CHARS,
        );

        assert!(
            ctx.heartbeat_md
                .as_deref()
                .unwrap_or("")
                .contains("checklist")
        );
    }

    #[test]
    fn per_file_char_limit_applied() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a 100-char AGENTS.md but limit to 50.
        fs::write(tmp.path().join("AGENTS.md"), "x".repeat(100)).expect("agents");

        let ctx = WorkspaceContext::load(
            tmp.path(),
            SessionType::Normal,
            false,
            50,
            DEFAULT_TOTAL_MAX_CHARS,
        );

        let content = ctx.agents_md.as_deref().unwrap_or("");
        assert!(content.len() <= 50, "got {} chars", content.len());
    }
}
