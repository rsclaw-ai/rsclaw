//! Atomic file primitives for the content store.
//!
//! - `write_if_new` — truly atomic no-clobber write via
//!   `NamedTempFile::persist_noclobber` (link-based on Unix).
//! - `overwrite_atomic` — tempfile + rename for the explicit "I really want to
//!   replace" path (used by compactor / GC, not by the ingest path).
//! - `sha256_hex` — hex-encoded sha256 for content addressing.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

/// Atomic, no-clobber file write. Returns `Ok(true)` if the file was
/// created, `Ok(false)` if a file already existed at `path`. Truly
/// atomic: uses `NamedTempFile::persist_noclobber`, which goes through
/// `link(2)` + `unlink(2)` on Unix (so a concurrent racing writer
/// cannot overwrite) and the equivalent no-replace move on Windows.
///
/// Why this is necessary: a naive `if path.exists() { return Ok(false) }`
/// + `rename(tmp, path)` is a TOCTOU race AND `rename(2)` on Unix
/// overwrites the destination — so two concurrent ingests of the same
/// path could each see `!exists()` and then clobber each other.
pub fn write_if_new(path: &Path, bytes: &[u8]) -> Result<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;

    let mut tmp = NamedTempFile::new_in(parent)
        .with_context(|| format!("create tempfile in {}", parent.display()))?;
    tmp.as_file_mut().write_all(bytes)?;
    tmp.as_file().sync_all()?;

    match tmp.persist_noclobber(path) {
        Ok(_) => {
            #[cfg(unix)]
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all(); // best-effort fsync of dir entry
            }
            Ok(true)
        }
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => {
            Err(anyhow::Error::from(e.error)
                .context(format!("persist_noclobber → {}", path.display())))
        }
    }
}

/// Atomic overwrite. Replaces the file if present. Use only from
/// paths that explicitly want to clobber (compactor / GC); the ingest
/// path goes through `write_if_new`.
pub fn overwrite_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp.{}",
        path.extension().and_then(|e| e.to_str()).unwrap_or("kb"),
        ulid::Ulid::new()
    ));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn write_if_new_creates() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a/b/c.md");
        assert!(write_if_new(&p, b"hi").unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), b"hi");
    }

    #[test]
    fn write_if_new_skips_existing() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        write_if_new(&p, b"first").unwrap();
        assert!(!write_if_new(&p, b"second").unwrap());
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
    }

    #[test]
    fn write_if_new_concurrent_no_clobber() {
        // Regression: TOCTOU race where two threads each see !exists()
        // and then rename(tmp, path) over each other. With
        // persist_noclobber, exactly one wins and the other gets
        // Ok(false); the file always holds the winner's bytes.
        use std::{sync::Arc, thread};

        for _ in 0..20 {
            let tmp = TempDir::new().unwrap();
            let p = Arc::new(tmp.path().join("race.md"));
            let p1 = p.clone();
            let p2 = p.clone();
            let h1 = thread::spawn(move || write_if_new(&p1, b"first").unwrap());
            let h2 = thread::spawn(move || write_if_new(&p2, b"second").unwrap());
            let r1 = h1.join().unwrap();
            let r2 = h2.join().unwrap();
            assert_ne!(r1, r2, "both calls reported the same result");
            assert!(r1 || r2);
            let body = std::fs::read(&*p).unwrap();
            assert!(body == b"first" || body == b"second");
        }
    }

    #[test]
    fn overwrite_replaces() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        write_if_new(&p, b"first").unwrap();
        overwrite_atomic(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }
}
