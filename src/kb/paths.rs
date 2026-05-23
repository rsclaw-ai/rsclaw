//! Resolves the on-disk layout `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/`.
//!
//! See spec §1 Layout. The KB owns its own subtree of `~/.rsclaw/`;
//! `KbPaths::ensure_layout()` is idempotent so callers can run it
//! freely at startup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct KbPaths {
    pub root: PathBuf,
}

impl KbPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn md_dir(&self) -> PathBuf {
        self.root.join("md")
    }
    pub fn raw_dir(&self) -> PathBuf {
        self.root.join("raw")
    }
    pub fn db_dir(&self) -> PathBuf {
        self.root.join("db")
    }
    pub fn idx_dir(&self) -> PathBuf {
        self.root.join("idx")
    }
    pub fn hnsw_dir(&self) -> PathBuf {
        self.root.join("hnsw")
    }
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }
    pub fn redb_file(&self) -> PathBuf {
        self.db_dir().join("kb.redb")
    }

    /// Create all directories. Idempotent. Week 1 only provisions
    /// `md/doc` and `md/url`; v2 adds `md/{chat,img,mail}` when
    /// those source kinds ship.
    pub fn ensure_layout(&self) -> Result<()> {
        for d in [
            &self.md_dir(),
            &self.raw_dir(),
            &self.db_dir(),
            &self.idx_dir(),
            &self.hnsw_dir(),
            &self.state_dir(),
        ] {
            ensure_dir(d)?;
        }
        for sub in ["doc", "url"] {
            ensure_dir(&self.md_dir().join(sub))?;
        }
        Ok(())
    }
}

fn ensure_dir(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("mkdir -p {}", p.display()))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn ensure_layout_creates_subdirs() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        for d in [
            "md", "raw", "db", "idx", "hnsw", "state", "md/doc", "md/url",
        ] {
            assert!(tmp.path().join(d).is_dir(), "missing {d}");
        }
    }

    #[test]
    fn ensure_layout_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        p.ensure_layout().unwrap();
    }

    #[test]
    fn helpers_compose_under_root() {
        let p = KbPaths::new("/tmp/kbroot");
        assert_eq!(p.md_dir(), PathBuf::from("/tmp/kbroot/md"));
        assert_eq!(p.raw_dir(), PathBuf::from("/tmp/kbroot/raw"));
        assert_eq!(p.db_dir(), PathBuf::from("/tmp/kbroot/db"));
        assert_eq!(p.idx_dir(), PathBuf::from("/tmp/kbroot/idx"));
        assert_eq!(p.hnsw_dir(), PathBuf::from("/tmp/kbroot/hnsw"));
        assert_eq!(p.state_dir(), PathBuf::from("/tmp/kbroot/state"));
        assert_eq!(p.redb_file(), PathBuf::from("/tmp/kbroot/db/kb.redb"));
    }
}
