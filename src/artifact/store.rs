//! Filesystem-backed artifact storage.
//!
//! Layout: `~/.rsclaw/artifacts/<session_key>/<id>.txt`. Session-scoped so
//! cleanup is cheap on session end. Global housekeeping pass enforces a
//! 7-day TTL across all sessions.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

pub const ARTIFACT_SUBDIR: &str = "artifacts";

/// Per-session cap on number of artifact files. When exceeded, oldest files
/// are pruned first so writes never fail.
const SESSION_FILE_CAP: usize = 100;

/// Global TTL — artifacts older than this are eligible for deletion by
/// [`ArtifactStore::housekeep`]. 7 days matches typical agent session
/// recency windows.
const GLOBAL_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Opaque id used in tool_result envelopes. Format: `tr_<8 hex>`.
/// Stable, URL-safe, easy to grep in logs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(pub String);

impl ArtifactId {
    pub fn new() -> Self {
        // 32-bit randomness is enough — collisions within a session are
        // recoverable (write overwrites). Avoid pulling uuid into this hot path.
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        let mix = nanos.wrapping_mul(pid.wrapping_add(2654435761));
        Self(format!("tr_{:08x}", mix))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Strict id validation — must be `tr_` + at least 4 alphanumeric chars.
    /// Rejects anything containing path separators or `..` so a malicious
    /// LLM-supplied id can't escape the artifact dir.
    pub fn parse(s: &str) -> Result<Self> {
        if !s.starts_with("tr_") {
            return Err(anyhow!("artifact id must start with 'tr_': {s}"));
        }
        let rest = &s[3..];
        if rest.len() < 4 || rest.len() > 32 {
            return Err(anyhow!("artifact id has invalid length: {s}"));
        }
        if !rest.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(anyhow!("artifact id has invalid chars: {s}"));
        }
        Ok(Self(s.to_owned()))
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Default store under `~/.rsclaw/artifacts/`.
    pub fn default_store() -> Self {
        let root = crate::config::loader::base_dir().join(ARTIFACT_SUBDIR);
        Self { root }
    }

    /// Custom root — useful for tests.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, session_key: &str) -> PathBuf {
        // Sanitize session_key so a key with '/' or '..' can't escape.
        let safe: String = session_key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        self.root.join(safe)
    }

    fn path_for(&self, session_key: &str, id: &ArtifactId) -> PathBuf {
        self.session_dir(session_key).join(format!("{}.txt", id.as_str()))
    }

    /// Write `text` to a freshly-generated artifact under `session_key`.
    /// Enforces the per-session file cap by pruning oldest entries first.
    pub fn write(&self, session_key: &str, text: &str) -> Result<ArtifactId> {
        let dir = self.session_dir(session_key);
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        self.prune_if_over_cap(&dir).ok();

        let id = ArtifactId::new();
        let path = dir.join(format!("{}.txt", id.as_str()));
        let mut f = fs::File::create(&path)
            .with_context(|| format!("create artifact {}", path.display()))?;
        f.write_all(text.as_bytes())
            .with_context(|| format!("write artifact {}", path.display()))?;
        Ok(id)
    }

    /// Read full artifact text. Returns `Err` if the id is malformed or the
    /// file is missing (e.g. session GC'd).
    pub fn read(&self, session_key: &str, id: &ArtifactId) -> Result<String> {
        let path = self.path_for(session_key, id);
        let mut f = fs::File::open(&path)
            .with_context(|| format!("artifact not found: {}", path.display()))?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        Ok(buf)
    }

    /// Delete all artifacts for a session (called on session end).
    pub fn gc_session(&self, session_key: &str) -> io::Result<()> {
        let dir = self.session_dir(session_key);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Walk every session dir and delete artifacts older than [`GLOBAL_TTL`].
    /// Best-effort — errors are logged and skipped, never propagated.
    pub fn housekeep(&self) {
        let Ok(sessions) = fs::read_dir(&self.root) else { return };
        let cutoff = SystemTime::now() - GLOBAL_TTL;
        for s_entry in sessions.flatten() {
            let s_path = s_entry.path();
            if !s_path.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&s_path) else { continue };
            let mut any_left = false;
            for f_entry in files.flatten() {
                let f_path = f_entry.path();
                let too_old = fs::metadata(&f_path)
                    .and_then(|m| m.modified())
                    .map(|t| t < cutoff)
                    .unwrap_or(false);
                if too_old {
                    let _ = fs::remove_file(&f_path);
                } else {
                    any_left = true;
                }
            }
            if !any_left {
                let _ = fs::remove_dir(&s_path);
            }
        }
    }

    /// If `dir` holds more than `SESSION_FILE_CAP` artifacts, delete the
    /// oldest ones until back under the cap. Called from `write`.
    fn prune_if_over_cap(&self, dir: &Path) -> io::Result<()> {
        let entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "txt").unwrap_or(false))
            .collect();
        if entries.len() < SESSION_FILE_CAP {
            return Ok(());
        }
        let mut with_mtime: Vec<(PathBuf, SystemTime)> = entries
            .iter()
            .filter_map(|e| {
                let mtime = fs::metadata(e.path()).and_then(|m| m.modified()).ok()?;
                Some((e.path(), mtime))
            })
            .collect();
        with_mtime.sort_by_key(|(_, m)| *m);
        let to_delete = with_mtime.len().saturating_sub(SESSION_FILE_CAP - 1);
        for (p, _) in with_mtime.into_iter().take(to_delete) {
            let _ = fs::remove_file(p);
        }
        Ok(())
    }
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self::default_store()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = tempdir().unwrap();
        let store = ArtifactStore::at(tmp.path().to_path_buf());
        let id = store.write("sess-1", "hello world").unwrap();
        let got = store.read("sess-1", &id).unwrap();
        assert_eq!(got, "hello world");
    }

    #[test]
    fn gc_session_removes_all() {
        let tmp = tempdir().unwrap();
        let store = ArtifactStore::at(tmp.path().to_path_buf());
        let id = store.write("sess-x", "data").unwrap();
        store.gc_session("sess-x").unwrap();
        assert!(store.read("sess-x", &id).is_err());
    }

    #[test]
    fn id_parse_rejects_traversal() {
        assert!(ArtifactId::parse("../etc/passwd").is_err());
        assert!(ArtifactId::parse("tr_../foo").is_err());
        assert!(ArtifactId::parse("tr_abc/xyz").is_err());
        assert!(ArtifactId::parse("tr_abc12345").is_ok());
    }

    #[test]
    fn session_key_with_slash_is_sanitized() {
        let tmp = tempdir().unwrap();
        let store = ArtifactStore::at(tmp.path().to_path_buf());
        let id = store.write("sess/../escape", "x").unwrap();
        // Sanitization should have stripped slashes; read with same key works.
        assert_eq!(store.read("sess/../escape", &id).unwrap(), "x");
        // The dir is under the root, not above it.
        assert!(store.session_dir("sess/../escape").starts_with(tmp.path()));
    }

    #[test]
    fn prune_keeps_under_cap() {
        let tmp = tempdir().unwrap();
        let store = ArtifactStore::at(tmp.path().to_path_buf());
        for _ in 0..(SESSION_FILE_CAP + 20) {
            store.write("sess-cap", "x").unwrap();
            // Distinct mtimes so prune order is deterministic.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let dir = store.session_dir("sess-cap");
        let count = fs::read_dir(&dir).unwrap().count();
        assert!(count <= SESSION_FILE_CAP, "got {count} files, cap is {SESSION_FILE_CAP}");
    }
}
