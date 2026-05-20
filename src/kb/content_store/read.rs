//! Lazy readers for on-disk doc files. Chunk retrieval reads only
//! the byte range it needs via `read_doc_range`, avoiding loading
//! entire documents into memory just to serve one chunk's body.

use crate::kb::content_store::atomic::sha256_hex;
use crate::kb::content_store::compose::parse_doc_file;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub fn read_doc_body(abs: &Path) -> Result<String> {
    let s = std::fs::read_to_string(abs)
        .with_context(|| format!("read {}", abs.display()))?;
    Ok(parse_doc_file(&s)?.body)
}

pub fn read_doc_range(abs: &Path, start: u64, end_excl: u64) -> Result<String> {
    let s = std::fs::read_to_string(abs)
        .with_context(|| format!("read {}", abs.display()))?;
    let parsed = parse_doc_file(&s)?;
    let bytes = parsed.body.as_bytes();
    let (s_, e_) = (start as usize, end_excl as usize);
    if e_ > bytes.len() || s_ > e_ {
        return Err(anyhow!(
            "range {s_}..{e_} oob (body len {})",
            bytes.len()
        ));
    }
    Ok(std::str::from_utf8(&bytes[s_..e_])?.to_string())
}

pub fn verify_doc_sha(abs: &Path, expected: &str) -> Result<()> {
    let body = read_doc_body(abs)?;
    let actual = sha256_hex(body.as_bytes());
    if actual != expected {
        return Err(anyhow!(
            "sha mismatch for {}: expected {expected} got {actual}",
            abs.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::content_store::atomic::write_if_new;
    use crate::kb::content_store::compose::{compose_doc_file, FrontMatter};
    use tempfile::TempDir;

    fn fm() -> FrontMatter {
        FrontMatter {
            title: "T".into(),
            source_kind: "doc".into(),
            logical_source_id: "x".into(),
            created_at: "2026-05-19".into(),
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    fn stage(tmp: &TempDir, body: &str) -> std::path::PathBuf {
        let p = tmp.path().join("x.md");
        let s = compose_doc_file(&fm(), body).unwrap();
        write_if_new(&p, s.as_bytes()).unwrap();
        p
    }

    #[test]
    fn read_body_strips_fm() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "BODY");
        assert_eq!(read_doc_body(&p).unwrap(), "BODY");
    }

    #[test]
    fn read_range() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "0123456789");
        assert_eq!(read_doc_range(&p, 2, 5).unwrap(), "234");
    }

    #[test]
    fn read_range_oob_errors() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "short");
        assert!(read_doc_range(&p, 0, 999).is_err());
    }

    #[test]
    fn read_range_inverted_errors() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "hello");
        assert!(read_doc_range(&p, 4, 1).is_err());
    }

    #[test]
    fn verify_sha_ok() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "X");
        verify_doc_sha(&p, &sha256_hex(b"X")).unwrap();
    }

    #[test]
    fn verify_sha_mismatch() {
        let tmp = TempDir::new().unwrap();
        let p = stage(&tmp, "X");
        assert!(verify_doc_sha(&p, "bad").is_err());
    }
}
