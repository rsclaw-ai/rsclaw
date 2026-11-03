//! On-disk cache of OCR readings.
//!
//! Reading a scanned bundle again (a retried turn, "continue", a second
//! question about the same file) used to send every page to the OCR model
//! again and pay for it again. Every OCR request goes through
//! [`crate::OcrClient`], so the cache sits there and covers the `ocr` and
//! `pdf` tools, `read_file` on PDFs, KB image ingest, the CLI and plugins.
//!
//! The key is a SHA-256 over the exact image bytes sent (after rendering,
//! cropping, strip cutting and JPEG encoding, so any change to that
//! preprocessing changes the key), the model, the prompt, the language and
//! the output cap. Only inline `data:` images are cached: a URL can change
//! behind the same string. Entries are plain text files under
//! `var/cache/ocr/`, kept to a total size cap by evicting the least
//! recently used.

use std::{
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

use sha2::{Digest, Sha256};

/// Bumped when the meaning of a cached reading changes.
const KEY_VERSION: &str = "rsclaw-ocr-cache/1";
/// Total size the cache is kept under. A page reads to a few KB of text,
/// so this holds tens of thousands of pages.
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
/// Stores between two size checks of the cache directory.
const EVICT_EVERY: u32 = 32;
const ENTRY_EXT: &str = "txt";

/// A directory of OCR readings keyed by [`OcrCache::key`].
pub struct OcrCache {
    dir: PathBuf,
    max_bytes: u64,
    puts: AtomicU32,
}

impl OcrCache {
    /// A cache in `dir`, kept under `max_bytes`.
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            puts: AtomicU32::new(0),
        }
    }

    /// The process-wide cache in the profile's `var/cache/ocr`.
    pub fn global() -> &'static OcrCache {
        static CACHE: OnceLock<OcrCache> = OnceLock::new();
        CACHE.get_or_init(|| {
            OcrCache::new(
                rsclaw_config::loader::cache_dir().join("ocr"),
                MAX_CACHE_BYTES,
            )
        })
    }

    /// The cache key for one OCR request, or `None` when the image is not
    /// an inline `data:` URI.
    pub fn key(
        image: &str,
        model: Option<&str>,
        prompt: Option<&str>,
        lang: Option<&str>,
        max_tokens: Option<u32>,
    ) -> Option<String> {
        if !image.starts_with("data:") {
            return None;
        }
        let max_tokens = max_tokens.map(|n| n.to_string());
        let mut h = Sha256::new();
        for part in [
            Some(KEY_VERSION),
            model,
            prompt,
            lang,
            max_tokens.as_deref(),
            Some(image),
        ] {
            // Length-prefixed, with absence distinct from an empty string.
            match part {
                Some(s) => {
                    h.update(b"1");
                    h.update((s.len() as u64).to_le_bytes());
                    h.update(s.as_bytes());
                }
                None => h.update(b"0"),
            }
        }
        Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{ENTRY_EXT}"))
    }

    /// The cached reading for `key`, marking it recently used.
    pub fn get(&self, key: &str) -> Option<String> {
        let path = self.path(key);
        let text = std::fs::read_to_string(&path).ok()?;
        if let Err(e) = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(std::time::SystemTime::now()))
        {
            tracing::warn!(error = %e, "ocr cache: cannot mark an entry as used");
        }
        Some(text)
    }

    /// Stores a reading. Failures are logged, never returned: the cache is
    /// an optimisation.
    pub fn put(&self, key: &str, text: &str) {
        if text.len() as u64 > self.max_bytes / 8 {
            return;
        }
        if let Err(e) = self.write(key, text) {
            tracing::warn!(error = %e, dir = %self.dir.display(), "ocr cache: cannot store a reading");
            return;
        }
        if self.puts.fetch_add(1, Ordering::Relaxed) % EVICT_EVERY == 0 {
            if let Err(e) = self.evict() {
                tracing::warn!(error = %e, "ocr cache: cannot trim the cache");
            }
        }
    }

    fn write(&self, key: &str, text: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        // Write aside and rename, so a concurrent reader never sees half
        // an entry.
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        std::io::Write::write_all(&mut tmp, text.as_bytes())?;
        tmp.persist(self.path(key)).map_err(|e| e.error)?;
        Ok(())
    }

    /// Removes the least recently used entries until the cache is at three
    /// quarters of its cap, when it is over the cap.
    fn evict(&self) -> std::io::Result<()> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(ENTRY_EXT) {
                continue;
            }
            let meta = entry.metadata()?;
            total += meta.len();
            let used = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            entries.push((used, meta.len(), path));
        }
        if total <= self.max_bytes {
            return Ok(());
        }
        entries.sort_by_key(|(used, _, _)| *used);
        let target = self.max_bytes / 4 * 3;
        for (_, len, path) in entries {
            if total <= target {
                break;
            }
            remove(&path);
            total = total.saturating_sub(len);
        }
        Ok(())
    }
}

fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::warn!(error = %e, "ocr cache: cannot remove an old entry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: &str = "data:image/jpeg;base64,AAAA";

    #[test]
    fn ocr_cache_key_covers_every_input() {
        let base = OcrCache::key(IMG, Some("rsclaw-ocr-v1"), Some("OCR:"), None, Some(2048));
        assert!(base.is_some());
        let variants = [
            OcrCache::key(
                "data:image/jpeg;base64,AAAB",
                Some("rsclaw-ocr-v1"),
                Some("OCR:"),
                None,
                Some(2048),
            ),
            OcrCache::key(IMG, Some("rsclaw-ocr-v2"), Some("OCR:"), None, Some(2048)),
            OcrCache::key(IMG, Some("rsclaw-ocr-v1"), Some("Table:"), None, Some(2048)),
            OcrCache::key(IMG, Some("rsclaw-ocr-v1"), None, None, Some(2048)),
            OcrCache::key(
                IMG,
                Some("rsclaw-ocr-v1"),
                Some("OCR:"),
                Some("zh"),
                Some(2048),
            ),
            OcrCache::key(IMG, Some("rsclaw-ocr-v1"), Some("OCR:"), None, None),
        ];
        for v in variants {
            assert_ne!(v, base);
        }
        assert_eq!(
            base,
            OcrCache::key(IMG, Some("rsclaw-ocr-v1"), Some("OCR:"), None, Some(2048))
        );
        // Absent and empty are different requests.
        assert_ne!(
            OcrCache::key(IMG, None, None, None, None),
            OcrCache::key(IMG, None, Some(""), None, None)
        );
    }

    #[test]
    fn ocr_cache_skips_urls() {
        assert!(OcrCache::key("https://example.com/a.png", None, None, None, None).is_none());
    }

    #[test]
    fn ocr_cache_stores_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let cache = OcrCache::new(dir.path().join("ocr"), 1024 * 1024);
        let key = OcrCache::key(IMG, Some("m"), None, None, None).unwrap();
        assert!(cache.get(&key).is_none());
        cache.put(&key, "第一页 合同金额 120000 元");
        assert_eq!(
            cache.get(&key).as_deref(),
            Some("第一页 合同金额 120000 元")
        );
    }

    #[test]
    fn ocr_cache_evicts_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        // Room for five 80-byte entries. `put` trims only every
        // `EVICT_EVERY` stores, so the test calls `evict` itself.
        let cache = OcrCache::new(dir.path().to_path_buf(), 400);
        let text = "x".repeat(40);
        let keys: Vec<String> = (0..10)
            .map(|i| OcrCache::key(&format!("data:,{i}"), None, None, None, None).unwrap())
            .collect();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (i, key) in keys.iter().enumerate() {
            cache.write(key, &text.repeat(2)).unwrap();
            // Oldest first, a second apart, so the order is unambiguous.
            let at = old + std::time::Duration::from_secs(i as u64);
            std::fs::File::options()
                .write(true)
                .open(cache.path(key))
                .unwrap()
                .set_modified(at)
                .unwrap();
        }
        // Reading the oldest entry makes it the most recently used.
        assert!(cache.get(&keys[0]).is_some());
        cache.evict().unwrap();
        let kept: Vec<usize> = (0..10).filter(|&i| cache.path(&keys[i]).exists()).collect();
        let total: u64 = kept.len() as u64 * 80;
        assert!(
            total <= 300,
            "trimmed to three quarters of the cap: {kept:?}"
        );
        assert!(kept.contains(&0), "the entry just read survives: {kept:?}");
        assert!(kept.contains(&9), "the newest entry survives: {kept:?}");
        assert!(
            !kept.contains(&1),
            "the least recently used goes first: {kept:?}"
        );
    }
}
