//! Path confinement helpers.
//!
//! Use [`resolve_within`] whenever a path comes from an LLM, a plugin, a
//! remote peer or an inbound payload and must stay inside a root directory
//! (workspace, uploads dir, plugin dir, ...). String blacklists (`..`,
//! leading `/`) are not sufficient: `~`, symlinks, Windows drive prefixes and
//! forward-slash drive paths all slip past them.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

/// Resolve `user_path` against `root` and guarantee the result stays inside
/// `root`, following symlinks of every existing ancestor.
///
/// - Relative paths are joined onto `root`.
/// - Absolute paths are accepted only when they already point inside `root`.
/// - `~` is NOT expanded (a leading `~` is treated as a literal directory
///   name under `root`).
/// - The target itself need not exist; the deepest existing ancestor is
///   canonicalized and the remaining components must be plain names.
pub fn resolve_within(root: &Path, user_path: &str) -> Result<PathBuf> {
    let root_canon = canonical_or_self(root);
    let candidate = Path::new(user_path);
    let joined = if candidate.is_absolute() || has_prefix(candidate) {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let normalized = lexical_normalize(&joined)?;
    let resolved = canonicalize_existing_prefix(&normalized);
    if !resolved.starts_with(&root_canon) {
        bail!(
            "path `{user_path}` escapes the allowed directory {}",
            root.display()
        );
    }
    Ok(resolved)
}

/// Like [`resolve_within`] but tries each root in order and returns the first
/// match.
pub fn resolve_within_any(roots: &[&Path], user_path: &str) -> Result<PathBuf> {
    for r in roots {
        if let Ok(p) = resolve_within(r, user_path) {
            return Ok(p);
        }
    }
    bail!("path `{user_path}` is outside every allowed directory")
}

/// Reduce an untrusted file name (from a chat attachment, HTTP header,
/// archive entry, ...) to a single safe path component. Directory parts,
/// control characters and reserved names are removed; an empty result
/// becomes `fallback`.
pub fn sanitize_filename(name: &str, fallback: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    let mut out = trimmed.to_owned();
    if out.len() > 200 {
        let mut end = 200;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

/// True when `s` is a safe single identifier (`[A-Za-z0-9_-]`, 1..=64 chars,
/// not starting with `-`). Use for agent ids, plugin names, skill slugs.
pub fn is_safe_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s != "."
        && s != ".."
        && !s.contains("..")
}

fn has_prefix(p: &Path) -> bool {
    matches!(p.components().next(), Some(Component::Prefix(_)))
}

fn canonical_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve `.` and `..` lexically; error if `..` climbs above the start.
fn lexical_normalize(p: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("path climbs above filesystem root");
                }
            }
            Component::Normal(n) => out.push(n),
        }
    }
    Ok(out)
}

/// Canonicalize the deepest existing ancestor of `p` (resolving symlinks) and
/// re-append the non-existing tail.
fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = std::fs::canonicalize(&existing) {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (existing.file_name().map(|s| s.to_os_string()), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escapes() {
        let dir = std::env::temp_dir().join(format!("rsclaw-fsguard-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("mkdir");
        assert!(resolve_within(&dir, "sub/a.txt").is_ok());
        assert!(resolve_within(&dir, "new/deeper/a.txt").is_ok());
        assert!(resolve_within(&dir, "../x").is_err());
        assert!(resolve_within(&dir, "sub/../../x").is_err());
        assert!(resolve_within(&dir, "/etc/passwd").is_err());
        let inside_abs = dir.join("sub/b.txt");
        assert!(resolve_within(&dir, inside_abs.to_str().expect("utf8")).is_ok());
        #[cfg(unix)]
        {
            let link = dir.join("escape");
            let _ignored = std::fs::remove_file(&link);
            std::os::unix::fs::symlink("/", &link).expect("symlink");
            assert!(resolve_within(&dir, "escape/etc/passwd").is_err());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filenames_and_slugs() {
        assert_eq!(sanitize_filename("../../.ssh/authorized_keys", "f"), "authorized_keys");
        assert_eq!(sanitize_filename("..", "f"), "f");
        assert_eq!(sanitize_filename("C:\\x\\报告.pdf", "f"), "报告.pdf");
        assert!(is_safe_slug("coder-1"));
        assert!(!is_safe_slug("x/../../y"));
        assert!(!is_safe_slug(".."));
        assert!(!is_safe_slug(""));
    }
}
