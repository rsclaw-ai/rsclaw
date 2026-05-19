//! `.env` file reader / writer for `$RSCLAW_BASE_DIR/.env`.
//!
//! Format: one `KEY=VAL` per line. `#` starts a comment. Blank lines
//! ignored. Values are not quoted / escaped — this is a tightly-scoped
//! file we control end-to-end (auto-managed, written by us), so we
//! don't carry the dotenv crate's full grammar. Values containing
//! newlines are skipped on write with a warning marker so a manually
//! pasted multi-line cert doesn't silently corrupt the file.
//!
//! Keys are written sorted (BTreeMap iteration order) for stable,
//! diff-friendly file content. Atomic rename + mode 0600 on Unix —
//! the file holds secrets.
//!
//! See `env_resolution.rs` for the reconcile pipeline that drives
//! writes; this module is pure file IO.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result};

/// Read `.env` file into a sorted key→value map. Missing file → empty
/// map. Malformed lines are skipped with a warn-level log (don't crash
/// gateway startup over a stray paste).
pub fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    if !path.exists() {
        return Ok(out);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((k, v)) = trimmed.split_once('=') else {
            tracing::warn!(line_num = i + 1, line, "malformed line in .env, skipping");
            continue;
        };
        let k = k.trim();
        if !is_valid_key(k) {
            tracing::warn!(
                line_num = i + 1,
                key = k,
                "invalid env var name in .env, skipping"
            );
            continue;
        }
        out.insert(k.to_owned(), v.to_owned());
    }
    Ok(out)
}

/// Atomically write `vars` to `path` with mode 0600 on Unix. Creates
/// parent dir if missing. Replaces an existing file via tmp+rename so
/// concurrent gateway processes never see a half-written file.
pub fn write(path: &Path, vars: &BTreeMap<String, String>) -> Result<()> {
    let parent = path
        .parent()
        .context("env file has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create dir {}", parent.display()))?;

    // Unique tmp name per process avoids races between concurrent
    // gateway startups (e.g. supervisor restarts during a hot-reload).
    let tmp = parent.join(format!(".env.tmp.{}", std::process::id()));

    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;

        writeln!(
            f,
            "# Auto-managed by rsclaw. Vars synced from your shell on first run."
        )?;
        writeln!(
            f,
            "# Edit by hand to override a value; the next startup respects your edit"
        )?;
        writeln!(f, "# unless that var is also exported in your shell with a different")?;
        writeln!(f, "# value (shell wins on diff — see docs/env.md).")?;
        writeln!(f)?;

        for (k, v) in vars {
            if v.contains('\n') {
                writeln!(
                    f,
                    "# SKIPPED: {k} value contained newline (not supported in .env)"
                )?;
                continue;
            }
            writeln!(f, "{k}={v}")?;
        }
        f.flush()?;
        // fsync before rename so the rename can't outpace the data.
        f.sync_all()?;
    }

    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn is_valid_key(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_values() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join(".env");
        let mut vars = BTreeMap::new();
        vars.insert("FOO".to_owned(), "bar".to_owned());
        vars.insert("RSCLAW_API_KEY".to_owned(), "sk-abc=def/ghi+jkl".to_owned());
        write(&path, &vars).expect("write");

        let got = read(&path).expect("read");
        assert_eq!(got, vars);
    }

    #[test]
    fn read_skips_malformed_lines() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join(".env");
        std::fs::write(&path, "# comment\nGOOD=ok\nno_equals_sign\n=missing_key\nBAD KEY=x\nFOO=bar\n")
            .expect("write");

        let got = read(&path).expect("read");
        assert_eq!(got.get("GOOD").map(String::as_str), Some("ok"));
        assert_eq!(got.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(got.len(), 2, "expected only valid keys");
    }

    #[test]
    fn read_returns_empty_when_missing() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let got = read(&tmp.path().join("does-not-exist.env")).expect("read");
        assert!(got.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn write_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join(".env");
        let mut vars = BTreeMap::new();
        vars.insert("KEY".to_owned(), "val".to_owned());
        write(&path, &vars).expect("write");

        let meta = std::fs::metadata(&path).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");
    }

    #[test]
    fn write_skips_values_with_newlines() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let path = tmp.path().join(".env");
        let mut vars = BTreeMap::new();
        vars.insert("MULTILINE".to_owned(), "line1\nline2".to_owned());
        vars.insert("GOOD".to_owned(), "fine".to_owned());
        write(&path, &vars).expect("write");

        let got = read(&path).expect("read");
        assert!(!got.contains_key("MULTILINE"));
        assert_eq!(got.get("GOOD").map(String::as_str), Some("fine"));
    }
}
