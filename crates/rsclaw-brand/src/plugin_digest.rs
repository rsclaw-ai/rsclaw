// Content digest and version ordering for brand-bundled plugins.
//
// Shared by `build.rs` (via `include!`, to record the expected digest of each
// bundled plugin) and the crate (to verify the copy shipped in an install
// package), so it must not refer to `crate::` items and may only depend on
// `sha2` + `std`.

/// File names that are never part of a plugin's content: OS litter and the
/// marker the installer writes into an installed copy.
pub const DIGEST_IGNORED: &[&str] = &[".DS_Store", "Thumbs.db", ".rsclaw-bundled.json"];

/// `sha256:<hex>` over every file under `dir` (except [`DIGEST_IGNORED`]):
/// sorted `/`-separated relative paths, each followed by its length and
/// bytes, so renames, additions and edits all change the digest.
pub fn tree_digest(dir: &std::path::Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};

    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, std::path::PathBuf)>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if DIGEST_IGNORED.contains(&name.as_ref()) {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(root, &path, out)?;
            } else if kind.is_file() {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, path));
            } else {
                return Err(std::io::Error::other(format!(
                    "{} is neither a file nor a directory",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    walk(dir, dir, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = Sha256::new();
    for (rel, path) in files {
        let bytes = std::fs::read(&path)?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sha256:{hex}"))
}

/// Order dotted versions numerically (`0.10.0` > `0.9.3`); a non-numeric
/// part compares as 0 and a missing part as 0 (`1.2` == `1.2.0`).
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(v: &str) -> Vec<u64> {
        v.trim()
            .trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(a), parts(b));
    let len = a.len().max(b.len());
    for i in 0..len {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}
