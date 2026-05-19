# KB MVP Week 1 — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the foundation layer of the rsclaw Knowledge Base — model types (with `logical_source_id` + `KbVisibility`), on-disk content store, redb schema covering all 12 tables (incl. `kb_ledger`, `kb_jobs_*`, `kb_seen_items`, `kb_doc_latest_version`), source canonicalizers (text / md / html / text-layer PDF), URL canonicalization, and the chunker (deterministic chunk_id from `logical_source_id`). By end of Week 1, the system can canonicalize a file/URL into Markdown, stage it on disk, compute its `logical_source_id`, and produce chunks — **but no DB writes, no embedding, no FTS indexing, no retrieval**. Those are Week 2/3.

**Architecture:** New `src/kb/` module on top of existing `redb` / `tantivy` / `hnsw_rs` deps. **No SQL pretense** — jobs queue and seen_items are modeled as redb-native KV tables with explicit indices. **logical_source_id is the idempotency key**: same content from same source produces same chunk_ids no matter how many times re-ingested.

**Tech Stack:** Rust 2024, tokio, redb 2.x (existing), tantivy 0.22 (existing, schema only this week), hnsw_rs 0.3 (existing, schema only), sha2, ulid, serde, serde_json, serde_yaml, lol-html (existing), pdf-extract (new), jieba-rs (new — Week 2 use, added now), url (existing or new), chrono (existing), once_cell.

**Spec reference:** `docs/specs/2026-05-19-knowledge-base.md` (especially §1 model, §I SourceIdentity, §J IngestLedger/Outbox, §K PermissionScope, §2 canonicalize+chunker).

---

## What this plan delivers

By end of Week 1, the engineer can run:

```bash
cargo test -p rsclaw --lib kb::
cargo test --test kb_week1_e2e
```

…and have the full integration test pass: given a sample `.md` / `.html` / `.txt` / `.pdf` file or a URL, the system:

1. Detects mime
2. Canonicalizes → CanonicalizedSource { markdown, metadata }
3. Computes `logical_source_id` (`file:sha256:...` or `url:<normalized>`)
4. Stages `.md` file to `~/.rsclaw/kb/md/<kind>/<slug>.md` (atomic write, body + YAML front-matter)
5. Stages raw bytes to `~/.rsclaw/kb/raw/<doc_id>.<ext>` if `keep_raw=true`
6. Chunks the canonical markdown into ≤512-token chunks with `heading_path` prefix and SimHash
7. Produces deterministic chunk_ids that are **identical across runs**

Week 2 will add: redb writes of KbDoc/KbChunk/Ledger/Job in a single transaction, embedder, worker pool, crash-recovery tests. Week 3: tantivy/HNSW indexing, retrieval, tools. Week 4: syncers, CLI, compactor, release.

---

## File structure (this week)

```
src/kb/
  mod.rs                 # public façade
  paths.rs               # KbPaths
  model/
    mod.rs
    source.rs            # KbSource + KbSourceKind + MailSource + LogicalSourceId
    doc.rs               # KbDoc + KbStatus + KbVisibility + CallerScope
    chunk.rs             # KbChunk + ChunkStatus + chunk_id() (uses logical_source_id)
    locator.rs           # KbLocator
    entity.rs            # KbEntity + KbEntityIndex + EntityKind (types only this week)
    simhash.rs           # simhash64 + hamming64
    version.rs           # VersionPointer struct
  content_store/
    mod.rs               # stage_doc public API
    atomic.rs            # write_if_new + overwrite_atomic + sha256_hex
    paths.rs             # markdown_rel_path + raw_rel_path + slugify
    compose.rs           # YAML front-matter + body compose/parse
    read.rs              # read_doc_body + read_doc_range + verify_doc_sha
  store/
    mod.rs               # facade + open_kb_store
    schema.rs            # ALL 12 redb table definitions
  canonicalize/
    mod.rs               # Canonicalizer trait + CanonicalizedSource + CanonicalMetadata
    text.rs
    md.rs                # passthrough + heading_path scan + heading_path_at
    html.rs              # lol-html → markdown
    pdf.rs               # pdf-extract text layer (no OCR)
    mime.rs              # detect_mime + canonicalize_by_mime
    url_canon.rs         # URL canonicalization (strip utm/fbclid, sort params, etc.)
  chunker/
    mod.rs               # chunk_markdown(input) -> Vec<KbChunk>
    splitter.rs          # paragraph + sentence splitters (CJK aware)
    tokens.rs            # approx_token_count
  ledger/
    mod.rs               # re-exports
    types.rs             # IngestLedgerEntry + LedgerOp + LedgerStatus
  jobs/
    mod.rs               # re-exports
    types.rs             # Job + JobKind + JobStatus + ClaimToken + NewJob
  util/
    mod.rs
    redact.rs            # redact(input) -> first 8 hex of sha256

tests/
  fixtures/kb/
    sample.md
    sample.html
    sample.txt
  kb_week1_e2e.rs        # end-to-end: canonicalize → stage → chunk → verify

Cargo.toml               # add: pdf-extract, jieba-rs, serde_yaml, url, arc-swap (Week 2 use)
src/lib.rs               # pub mod kb
```

---

## Conventions

- **TDD**: every task is "write test → run (fails) → implement → run (passes) → commit"
- **One commit per task** with `feat(kb): ...` / `test(kb): ...` / `chore(kb): ...`
- **Test placement**: unit tests in `#[cfg(test)] mod tests` at end of source file; integration tests at `tests/kb_*.rs`
- **`cargo test -p rsclaw --lib kb::...`** for unit; `cargo test --test kb_week1_e2e` for integration
- **No `unwrap()` / `expect()` in non-test code** — use `anyhow::Result`
- **No `println!()` in non-test code** — use `log::` macros, content goes through `redact()`
- **All public types** `Serialize + Deserialize`

---

## Task 0: Bootstrap — Cargo deps + module skeleton

**Files:** Modify `Cargo.toml`, `src/lib.rs`; create empty `src/kb/mod.rs` and subdir stubs.

- [ ] **Step 1: Add Cargo deps**

Edit `Cargo.toml` `[dependencies]`:

```toml
pdf-extract = "0.7"
jieba-rs = "0.7"
serde_yaml = "0.9"
url = "2"
arc-swap = "1.7"
```

Existing assumed: `tokio`, `serde`, `serde_json`, `sha2`, `ulid`, `anyhow`, `log`, `tantivy`, `redb`, `hnsw_rs`, `lol_html`, `chrono`, `once_cell`, `tempfile` (dev).

- [ ] **Step 2: Create module skeleton (empty stubs)**

```bash
mkdir -p src/kb/{model,content_store,store,canonicalize,chunker,ledger,jobs,util}
for d in model content_store store canonicalize chunker ledger jobs util; do
  : > "src/kb/$d/mod.rs"
done
: > src/kb/paths.rs
: > src/kb/mod.rs
```

`src/kb/mod.rs`:

```rust
//! Knowledge base module. See docs/specs/2026-05-19-knowledge-base.md.

pub mod paths;
pub mod model;
pub mod content_store;
pub mod store;
pub mod canonicalize;
pub mod chunker;
pub mod ledger;
pub mod jobs;
pub mod util;
```

- [ ] **Step 3: Register in lib.rs**

Edit `src/lib.rs`, add `pub mod kb;` in appropriate spot.

- [ ] **Step 4: Verify compile**

```bash
cargo check
```

Expected: passes (empty modules are fine).

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/kb/
git commit -m "chore(kb): bootstrap module skeleton + deps (pdf-extract, jieba, serde_yaml, url, arc-swap)"
```

---

## Task 1: `paths.rs` — KB root + layout resolver

**Files:** `src/kb/paths.rs`

- [ ] **Step 1: Write test + impl**

```rust
//! Resolves the on-disk layout `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct KbPaths {
    pub root: PathBuf,
}

impl KbPaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn md_dir(&self) -> PathBuf { self.root.join("md") }
    pub fn raw_dir(&self) -> PathBuf { self.root.join("raw") }
    pub fn db_dir(&self) -> PathBuf { self.root.join("db") }
    pub fn idx_dir(&self) -> PathBuf { self.root.join("idx") }
    pub fn hnsw_dir(&self) -> PathBuf { self.root.join("hnsw") }
    pub fn state_dir(&self) -> PathBuf { self.root.join("state") }
    pub fn redb_file(&self) -> PathBuf { self.db_dir().join("kb.redb") }

    pub fn ensure_layout(&self) -> Result<()> {
        for d in [&self.md_dir(), &self.raw_dir(), &self.db_dir(),
                  &self.idx_dir(), &self.hnsw_dir(), &self.state_dir()] {
            std::fs::create_dir_all(d)
                .with_context(|| format!("create {}", d.display()))?;
        }
        // md/ subdirs — Week 1 only doc/url; v2 add chat, img, mail
        for sub in ["doc", "url"] {
            std::fs::create_dir_all(self.md_dir().join(sub))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_layout_creates_subdirs() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        for d in ["md", "raw", "db", "idx", "hnsw", "state", "md/doc", "md/url"] {
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
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::paths
git add src/kb/paths.rs
git commit -m "feat(kb): KbPaths resolves ~/.rsclaw/kb/ layout (md/doc + md/url only this week)"
```

---

## Task 2: `util/redact.rs` — PII redaction

**Files:** `src/kb/util/redact.rs`, modify `src/kb/util/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! PII redaction for log messages.

use sha2::{Digest, Sha256};

/// First 8 hex chars of sha256(input). Stable, non-reversible.
pub fn redact(input: impl AsRef<str>) -> String {
    let mut h = Sha256::new();
    h.update(input.as_ref().as_bytes());
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s.truncate(8);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() { assert_eq!(redact("x"), redact("x")); }

    #[test]
    fn differs() { assert_ne!(redact("a"), redact("b")); }

    #[test]
    fn eight_chars() { assert_eq!(redact("foo").len(), 8); }
}
```

`src/kb/util/mod.rs`:
```rust
pub mod redact;
pub use redact::redact;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::util::redact
git add src/kb/util/
git commit -m "feat(kb): util::redact for PII-safe logging"
```

---

## Task 3: `model/source.rs` — KbSourceKind + KbSource + LogicalSourceId

**Files:** `src/kb/model/source.rs`, `src/kb/model/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbSourceKind {
    Doc,
    Chat,   // v2
    Url,
    Img,    // v2
    Mail,   // v2
}

impl KbSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc", Self::Chat => "chat", Self::Url => "url",
            Self::Img => "img", Self::Mail => "mail",
        }
    }
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "doc" => Ok(Self::Doc), "chat" => Ok(Self::Chat),
            "url" => Ok(Self::Url), "img" => Ok(Self::Img),
            "mail" => Ok(Self::Mail),
            o => Err(format!("unknown KbSourceKind: {o}")),
        }
    }
    pub fn all() -> &'static [Self] {
        &[Self::Doc, Self::Chat, Self::Url, Self::Img, Self::Mail]
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbSource {
    Doc  { path: PathBuf },
    Url  { url: String, fetched_at: i64 },
    Chat { channel: String, range: (i64, i64) },
    Img  { path: PathBuf },
    Mail { source: MailSource },
}

impl KbSource {
    pub fn kind(&self) -> KbSourceKind {
        match self {
            Self::Doc {..} => KbSourceKind::Doc,
            Self::Url {..} => KbSourceKind::Url,
            Self::Chat {..} => KbSourceKind::Chat,
            Self::Img {..} => KbSourceKind::Img,
            Self::Mail {..} => KbSourceKind::Mail,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailSource {
    EmlFile  { path: PathBuf },
    MboxFile { path: PathBuf },
    Imap     { account: String, folder: String, uid: u64 },
    Gmail    { account: String, thread_id: String, msg_id: String },
}

/// Idempotency key: same content/source → same id, no matter how
/// many times re-ingested. Decoupled from KbDoc.id (ULID instance).
/// See spec §I SourceIdentity.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct LogicalSourceId(pub String);

impl LogicalSourceId {
    pub fn for_file(sha256_hex: &str) -> Self {
        Self(format!("file:sha256:{sha256_hex}"))
    }
    pub fn for_url(normalized_url: &str) -> Self {
        Self(format!("url:{normalized_url}"))
    }
    pub fn for_chat_bucket(channel: &str, window_start_unix: i64) -> Self {
        Self(format!("chat:{channel}:{window_start_unix}"))
    }
    pub fn for_mail(message_id: &str) -> Self {
        Self(format!("mail:{message_id}"))
    }
    pub fn as_str(&self) -> &str { &self.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_roundtrip() {
        for k in KbSourceKind::all() {
            assert_eq!(KbSourceKind::parse(k.as_str()).unwrap(), *k);
        }
    }

    #[test]
    fn source_to_kind() {
        assert_eq!(KbSource::Doc { path: "/x".into() }.kind(), KbSourceKind::Doc);
        assert_eq!(
            KbSource::Mail { source: MailSource::EmlFile { path: "/x.eml".into() }}.kind(),
            KbSourceKind::Mail
        );
    }

    #[test]
    fn logical_source_id_namespaces() {
        assert_eq!(LogicalSourceId::for_file("abc").as_str(), "file:sha256:abc");
        assert_eq!(LogicalSourceId::for_url("https://x").as_str(), "url:https://x");
        assert_eq!(
            LogicalSourceId::for_chat_bucket("feishu:pm", 1234567890).as_str(),
            "chat:feishu:pm:1234567890"
        );
    }

    #[test]
    fn logical_source_id_distinguishes_namespaces() {
        assert_ne!(LogicalSourceId::for_file("x"), LogicalSourceId::for_url("x"));
    }
}
```

`src/kb/model/mod.rs`:
```rust
pub mod source;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model::source
git add src/kb/model/
git commit -m "feat(kb): KbSourceKind + KbSource + LogicalSourceId (idempotency key)"
```

---

## Task 4: `model/locator.rs`

**Files:** `src/kb/model/locator.rs`, update `src/kb/model/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbLocator {
    PdfPage   { page: u32, bbox: Option<(f32, f32, f32, f32)> },
    MdSection { heading_path: Vec<String> },
    UrlAnchor { fragment: Option<String> },
    ChatMsgs  { first_ts: i64, last_ts: i64 },
    Image     { bbox: Option<(f32, f32, f32, f32)> },
    Offset    { start: usize, end: usize },
}

impl KbLocator {
    pub fn human(&self) -> String {
        match self {
            Self::PdfPage { page, .. } => format!("p.{page}"),
            Self::MdSection { heading_path } => {
                if heading_path.is_empty() { String::from("§") }
                else { format!("§{}", heading_path.join(" > ")) }
            }
            Self::UrlAnchor { fragment } => fragment
                .as_deref().map(|f| format!("#{f}")).unwrap_or_default(),
            Self::ChatMsgs { first_ts, last_ts } => format!("{first_ts}..{last_ts}"),
            Self::Image { .. } => String::from("image"),
            Self::Offset { start, end } => format!("bytes {start}..{end}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_format() {
        assert_eq!(KbLocator::PdfPage { page: 12, bbox: None }.human(), "p.12");
        assert_eq!(
            KbLocator::MdSection { heading_path: vec!["A".into(), "B".into()] }.human(),
            "§A > B"
        );
        assert_eq!(
            KbLocator::UrlAnchor { fragment: Some("s1".into()) }.human(), "#s1"
        );
        assert_eq!(KbLocator::Image { bbox: None }.human(), "image");
        assert_eq!(KbLocator::Offset { start: 0, end: 5 }.human(), "bytes 0..5");
    }

    #[test]
    fn serde_roundtrip() {
        let l = KbLocator::PdfPage { page: 7, bbox: Some((1.0, 2.0, 3.0, 4.0)) };
        let s = serde_json::to_string(&l).unwrap();
        let back: KbLocator = serde_json::from_str(&s).unwrap();
        assert_eq!(l, back);
    }
}
```

Update `model/mod.rs`:
```rust
pub mod source;
pub mod locator;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
pub use locator::KbLocator;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model::locator
git add src/kb/model/
git commit -m "feat(kb): KbLocator enum + human() formatter"
```

---

## Task 5: `model/simhash.rs` — SimHash-64 + Hamming

**Files:** `src/kb/model/simhash.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use sha2::{Digest, Sha256};

pub fn simhash64(text: &str) -> u64 {
    let mut accum = [0i32; 64];
    let mut seen = std::collections::HashSet::new();
    for tok in text.split_whitespace() {
        if !seen.insert(tok) { continue; }
        let mut h = Sha256::new();
        h.update(tok.as_bytes());
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&h.finalize()[..8]);
        let bits = u64::from_be_bytes(bytes);
        for i in 0..64 {
            if (bits >> i) & 1 == 1 { accum[i] += 1; } else { accum[i] -= 1; }
        }
    }
    let mut out: u64 = 0;
    for i in 0..64 {
        if accum[i] >= 0 { out |= 1u64 << i; }
    }
    out
}

pub fn hamming64(a: u64, b: u64) -> u32 { (a ^ b).count_ones() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_same_hash() {
        assert_eq!(simhash64("hello world"), simhash64("hello world"));
    }

    #[test]
    fn similar_close_hash() {
        let a = simhash64("the quick brown fox jumps over the lazy dog");
        let b = simhash64("the quick brown fox jumps over a lazy dog");
        assert!(hamming64(a, b) < 16);
    }

    #[test]
    fn different_far_hash() {
        let a = simhash64("the quick brown fox");
        let b = simhash64("completely unrelated content here");
        assert!(hamming64(a, b) > 16);
    }

    #[test]
    fn hamming_basic() {
        assert_eq!(hamming64(0, 0), 0);
        assert_eq!(hamming64(0, 0xFF), 8);
    }
}
```

Update `model/mod.rs`:
```rust
pub mod source;
pub mod locator;
pub mod simhash;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
pub use locator::KbLocator;
pub use simhash::{hamming64, simhash64};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model::simhash
git add src/kb/model/
git commit -m "feat(kb): simhash64 + hamming64 for chunk-level near-dup detection"
```

---

## Task 6: `model/chunk.rs` — deterministic chunk_id (uses logical_source_id) + KbChunk

**Files:** `src/kb/model/chunk.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use crate::kb::model::{KbLocator, LogicalSourceId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Deterministic chunk id: `sha256(logical_source_id | "\0" | seq_be | "\0" | content)`
/// truncated to 32 hex chars. Same logical_source_id + same content + same seq
/// always produces the same id → upserts are idempotent.
///
/// Crucially this is keyed on **logical_source_id**, not on doc_id or
/// source_id (which would change per-ingest). See spec §I.
pub fn chunk_id(lsid: &LogicalSourceId, seq: u32, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(lsid.as_str().as_bytes());
    h.update([0u8]);
    h.update(&seq.to_be_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    let mut hex = String::with_capacity(64);
    for b in h.finalize().iter() {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex.truncate(32);
    hex
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStatus { Active, Tombstoned }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbChunk {
    pub id: String,                       // 32-hex deterministic
    pub doc_id: String,                   // ULID, links to current KbDoc instance
    pub logical_source_id: String,        // links across versions, used for dedup
    pub doc_version: u32,                 // matches KbDoc.version
    pub seq: u32,
    pub heading_path: Vec<String>,
    pub byte_offset: (u64, u64),
    pub indexed_text: String,             // heading_path > ... \n\n body
    pub vector: Vec<f32>,                 // empty in Week 1; Week 2 embedder fills
    pub simhash: u64,
    pub locator: KbLocator,
    pub status: ChunkStatus,
    pub source_quality: f32,
    pub embedder_id: String,              // empty in Week 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_deterministic() {
        let lsid = LogicalSourceId::for_file("abc");
        let a = chunk_id(&lsid, 0, "hello world");
        let b = chunk_id(&lsid, 0, "hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn chunk_id_varies_with_seq() {
        let lsid = LogicalSourceId::for_file("abc");
        assert_ne!(chunk_id(&lsid, 0, "x"), chunk_id(&lsid, 1, "x"));
    }

    #[test]
    fn chunk_id_varies_with_content() {
        let lsid = LogicalSourceId::for_file("abc");
        assert_ne!(chunk_id(&lsid, 0, "hello"), chunk_id(&lsid, 0, "world"));
    }

    #[test]
    fn chunk_id_varies_with_logical_source() {
        let l1 = LogicalSourceId::for_file("abc");
        let l2 = LogicalSourceId::for_file("def");
        assert_ne!(chunk_id(&l1, 0, "x"), chunk_id(&l2, 0, "x"));
    }

    /// Critical guarantee for idempotency:
    /// re-ingesting the same file (= same logical_source_id) produces
    /// the same chunk_ids, regardless of doc_id.
    #[test]
    fn reingest_same_file_same_chunk_ids() {
        let lsid = LogicalSourceId::for_file("hash_of_file_contents");
        let body = "the actual canonical markdown body";
        let id_first  = chunk_id(&lsid, 0, body);
        let id_second = chunk_id(&lsid, 0, body);
        assert_eq!(id_first, id_second,
            "re-ingest must produce same chunk_id (idempotency invariant)");
    }

    #[test]
    fn struct_serde_roundtrip() {
        let lsid = LogicalSourceId::for_file("abc");
        let c = KbChunk {
            id: chunk_id(&lsid, 0, "hi"),
            doc_id: "doc1".into(),
            logical_source_id: lsid.0.clone(),
            doc_version: 1,
            seq: 0,
            heading_path: vec!["A".into()],
            byte_offset: (0, 2),
            indexed_text: "A\n\nhi".into(),
            vector: vec![],
            simhash: 0,
            locator: KbLocator::Offset { start: 0, end: 2 },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: String::new(),
        };
        let s = serde_json::to_string(&c).unwrap();
        let back: KbChunk = serde_json::from_str(&s).unwrap();
        assert_eq!(c, back);
    }
}
```

Update `model/mod.rs`:
```rust
pub mod source;
pub mod locator;
pub mod simhash;
pub mod chunk;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
pub use locator::KbLocator;
pub use simhash::{hamming64, simhash64};
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model::chunk
git add src/kb/model/
git commit -m "feat(kb): chunk_id (derived from logical_source_id) + KbChunk struct"
```

---

## Task 7: `model/doc.rs` — KbDoc + KbStatus + KbVisibility + CallerScope

**Files:** `src/kb/model/doc.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use crate::kb::model::{KbSource, KbSourceKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbStatus { Active, Tombstoned, Updating }

/// Permission boundary for a KbDoc. See spec §K.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KbVisibility {
    Global,
    Agent   { agent_id: String },
    Channel { channel_id: String },
    Private,
}

impl KbVisibility {
    /// Default visibility for a freshly-ingested doc, per source_kind.
    pub fn default_for(kind: KbSourceKind) -> Self {
        match kind {
            KbSourceKind::Doc | KbSourceKind::Url | KbSourceKind::Img => Self::Global,
            KbSourceKind::Mail => Self::Private,
            KbSourceKind::Chat => Self::Private,  // narrowed at ingest time
        }
    }

    pub fn visible_to(&self, scope: &CallerScope) -> bool {
        match self {
            Self::Global => true,
            Self::Agent { agent_id } => scope.agent_id.as_ref() == Some(agent_id),
            Self::Channel { channel_id } => scope.channel_id.as_ref() == Some(channel_id),
            Self::Private => scope.user_id.is_some(),  // any authenticated caller
        }
    }
}

/// Identity of the caller making a retrieval request. Injected by
/// agent runtime — agent code MUST NOT construct or modify this.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerScope {
    pub agent_id:   Option<String>,
    pub channel_id: Option<String>,
    pub user_id:    Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbDoc {
    pub id: String,                       // ULID, per-ingest instance
    pub logical_source_id: String,        // idempotency key (§I)
    pub source: KbSource,
    pub source_kind: KbSourceKind,
    pub title: String,
    pub mime: String,
    pub raw_sha256: String,
    pub markdown_path: String,            // relative to kb_root
    pub markdown_sha256: String,
    pub raw_path: Option<String>,
    pub owner_user_id: Option<String>,    // for Private visibility resolution
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32,                     // increments per re-ingest (§I)
    pub status: KbStatus,
    pub visibility: KbVisibility,
    pub tags: Vec<String>,
    pub meta: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_default_per_kind() {
        assert!(matches!(KbVisibility::default_for(KbSourceKind::Doc), KbVisibility::Global));
        assert!(matches!(KbVisibility::default_for(KbSourceKind::Mail), KbVisibility::Private));
        assert!(matches!(KbVisibility::default_for(KbSourceKind::Chat), KbVisibility::Private));
    }

    #[test]
    fn visibility_global_visible_to_anyone() {
        assert!(KbVisibility::Global.visible_to(&CallerScope::default()));
    }

    #[test]
    fn visibility_agent_filters_by_agent_id() {
        let v = KbVisibility::Agent { agent_id: "a1".into() };
        assert!(v.visible_to(&CallerScope { agent_id: Some("a1".into()), ..Default::default() }));
        assert!(!v.visible_to(&CallerScope { agent_id: Some("a2".into()), ..Default::default() }));
        assert!(!v.visible_to(&CallerScope::default()));
    }

    #[test]
    fn visibility_channel_filters_by_channel_id() {
        let v = KbVisibility::Channel { channel_id: "c1".into() };
        assert!(v.visible_to(&CallerScope { channel_id: Some("c1".into()), ..Default::default() }));
        assert!(!v.visible_to(&CallerScope { channel_id: Some("c2".into()), ..Default::default() }));
    }

    #[test]
    fn visibility_private_requires_user_id() {
        assert!(KbVisibility::Private.visible_to(&CallerScope { user_id: Some("u1".into()), ..Default::default() }));
        assert!(!KbVisibility::Private.visible_to(&CallerScope::default()));
    }

    #[test]
    fn doc_serde_roundtrip() {
        let d = KbDoc {
            id: "01HXY".into(),
            logical_source_id: "file:sha256:abc".into(),
            source: KbSource::Doc { path: "/tmp/x".into() },
            source_kind: KbSourceKind::Doc,
            title: "T".into(),
            mime: "text/markdown".into(),
            raw_sha256: "abc".into(),
            markdown_path: "md/doc/x.md".into(),
            markdown_sha256: "def".into(),
            raw_path: None,
            owner_user_id: None,
            created_at: 0, updated_at: 0, version: 1,
            status: KbStatus::Active,
            visibility: KbVisibility::Global,
            tags: vec![],
            meta: serde_json::Value::Null,
        };
        let s = serde_json::to_string(&d).unwrap();
        let back: KbDoc = serde_json::from_str(&s).unwrap();
        assert_eq!(d, back);
    }
}
```

Update `model/mod.rs`:
```rust
pub mod source;
pub mod locator;
pub mod simhash;
pub mod chunk;
pub mod doc;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
pub use locator::KbLocator;
pub use simhash::{hamming64, simhash64};
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
pub use doc::{CallerScope, KbDoc, KbStatus, KbVisibility};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model::doc
git add src/kb/model/
git commit -m "feat(kb): KbDoc + KbStatus + KbVisibility + CallerScope (permission scope)"
```

---

## Task 8: `model/entity.rs` + `model/version.rs`

**Files:** `src/kb/model/entity.rs`, `src/kb/model/version.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests for entity types**

```rust
// src/kb/model/entity.rs
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind { Brand, Person, Org, Email, Url, Hashtag, Other }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbEntity {
    pub canonical_id: String,
    pub surface_forms: Vec<String>,
    pub kind: EntityKind,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbEntityIndex {
    pub entity_id: String,
    pub chunk_id: String,
    pub doc_id: String,
    pub mention_count: u32,
    pub score: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serde_roundtrip() {
        let e = KbEntity {
            canonical_id: "ent_x".into(),
            surface_forms: vec!["X".into()],
            kind: EntityKind::Brand, created_at: 0,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<KbEntity>(&s).unwrap(), e);
    }
}
```

```rust
// src/kb/model/version.rs
use serde::{Deserialize, Serialize};

/// Pointer kept in `kb_doc_latest_version` table: `logical_source_id → VersionPointer`.
/// Used to find the active doc instance for a logical source. See spec §I.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct VersionPointer {
    pub doc_id: String,
    pub version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serde_roundtrip() {
        let v = VersionPointer { doc_id: "01HXY".into(), version: 3 };
        let s = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<VersionPointer>(&s).unwrap(), v);
    }
}
```

Update `model/mod.rs`:
```rust
pub mod source;
pub mod locator;
pub mod simhash;
pub mod chunk;
pub mod doc;
pub mod entity;
pub mod version;
pub use source::{KbSource, KbSourceKind, LogicalSourceId, MailSource};
pub use locator::KbLocator;
pub use simhash::{hamming64, simhash64};
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
pub use doc::{CallerScope, KbDoc, KbStatus, KbVisibility};
pub use entity::{EntityKind, KbEntity, KbEntityIndex};
pub use version::VersionPointer;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::model
git add src/kb/model/
git commit -m "feat(kb): KbEntity + KbEntityIndex + VersionPointer types"
```

---

## Task 9: `ledger/types.rs` + `jobs/types.rs` — IngestLedger and Job types

**Files:** `src/kb/ledger/types.rs`, `src/kb/jobs/types.rs`, update `mod.rs` files

- [ ] **Step 1: Write ledger types + tests**

```rust
// src/kb/ledger/types.rs
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerOp { Create, Update, Delete }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerStatus {
    Pending, IndexingComplete, CleanupPending, Done, Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IngestLedgerEntry {
    pub id: String,                  // ulid
    pub created_at: i64,
    pub updated_at: i64,
    pub doc_id: String,
    pub logical_source_id: String,
    pub op: LedgerOp,
    pub new_paths: Vec<String>,      // relative to kb_root
    pub old_paths: Vec<String>,
    pub status: LedgerStatus,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn serde_roundtrip() {
        let e = IngestLedgerEntry {
            id: "L1".into(),
            created_at: 0, updated_at: 0,
            doc_id: "D1".into(),
            logical_source_id: "file:sha256:abc".into(),
            op: LedgerOp::Create,
            new_paths: vec!["md/doc/x.md".into()],
            old_paths: vec![],
            status: LedgerStatus::Pending,
            error: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<IngestLedgerEntry>(&s).unwrap(), e);
    }
}
```

```rust
// src/kb/ledger/mod.rs
pub mod types;
pub use types::{IngestLedgerEntry, LedgerOp, LedgerStatus};
```

- [ ] **Step 2: Write job types + tests**

```rust
// src/kb/jobs/types.rs
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobKind {
    /// Chunk + embed + index a doc whose KbDoc and markdown are already
    /// in place. Week 2 worker consumes this.
    ChunkAndEmbed { doc_id: String, doc_version: u32 },
    /// Rebuild HNSW cache from redb. Background trigger.
    RebuildHnsw,
    /// Compactor work item.
    RunCompactor,
}

impl JobKind {
    /// dedupe key for the `jobs_by_dedupe_active` index. Same logical
    /// work in flight collapses to one job.
    pub fn dedupe_key(&self) -> String {
        match self {
            Self::ChunkAndEmbed { doc_id, doc_version } =>
                format!("chunk_embed:{doc_id}:{doc_version}"),
            Self::RebuildHnsw => "rebuild_hnsw:singleton".into(),
            Self::RunCompactor => "run_compactor:singleton".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus { Ready, Running, Done, Failed }

impl JobStatus {
    pub fn as_byte(self) -> u8 {
        match self {
            Self::Ready => 0, Self::Running => 1,
            Self::Done => 2, Self::Failed => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,                       // ulid
    pub kind: JobKind,
    pub status: JobStatus,
    pub priority: u8,                     // 0=highest, 255=lowest
    pub created_at: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl Job {
    pub fn new(kind: JobKind) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            kind, status: JobStatus::Ready, priority: 128,
            created_at: chrono::Utc::now().timestamp_millis(),
            attempts: 0, last_error: None,
        }
    }
}

/// Composite key for `jobs_by_status_priority` index. Order is
/// (status_byte, priority, created_at_be, job_id) so range-scan with
/// `status=Ready` prefix returns highest-priority-oldest first.
pub fn status_priority_key(j: &Job) -> Vec<u8> {
    let mut k = Vec::with_capacity(1 + 1 + 8 + j.id.len());
    k.push(j.status.as_byte());
    k.push(j.priority);
    k.extend_from_slice(&j.created_at.to_be_bytes());
    k.extend_from_slice(j.id.as_bytes());
    k
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimToken {
    pub worker_id: String,
    pub claimed_at: i64,
    pub expires_at: i64,
    pub token: String,                    // random ulid; mark_done verifies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_key_per_kind() {
        let j1 = JobKind::ChunkAndEmbed { doc_id: "d1".into(), doc_version: 1 };
        let j2 = JobKind::ChunkAndEmbed { doc_id: "d1".into(), doc_version: 2 };
        assert_ne!(j1.dedupe_key(), j2.dedupe_key());
        assert_eq!(JobKind::RebuildHnsw.dedupe_key(), "rebuild_hnsw:singleton");
    }

    #[test]
    fn status_priority_key_orders_correctly() {
        let mut j1 = Job::new(JobKind::RebuildHnsw);
        j1.priority = 50;
        let mut j2 = Job::new(JobKind::RunCompactor);
        j2.priority = 100;
        let k1 = status_priority_key(&j1);
        let k2 = status_priority_key(&j2);
        assert!(k1 < k2, "lower priority byte sorts first");
    }
}
```

```rust
// src/kb/jobs/mod.rs
pub mod types;
pub use types::{status_priority_key, ClaimToken, Job, JobKind, JobStatus};
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p rsclaw --lib kb::ledger kb::jobs
git add src/kb/ledger/ src/kb/jobs/
git commit -m "feat(kb): IngestLedgerEntry + Job/JobKind types (Week 2 will add stores)"
```

---

## Task 10: `store/schema.rs` — ALL redb tables

**Files:** `src/kb/store/schema.rs`, `src/kb/store/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! All redb table definitions for the KB store. Values are JSON-encoded
//! (compact binary encoding deferred to v2 if hot path profiling shows need).

use redb::TableDefinition;

// Core data
pub const KB_DOCS:                  TableDefinition<&str, &[u8]> = TableDefinition::new("kb_docs");
pub const KB_DOC_LATEST_VERSION:    TableDefinition<&str, &[u8]> = TableDefinition::new("kb_doc_latest_version");
pub const KB_CHUNKS:                TableDefinition<&str, &[u8]> = TableDefinition::new("kb_chunks");
pub const KB_CHUNK_BY_LOGICAL:      TableDefinition<&str, &[u8]> = TableDefinition::new("kb_chunk_by_logical");

// Entities
pub const KB_ENTITIES:              TableDefinition<&str, &[u8]> = TableDefinition::new("kb_entities");
pub const KB_ENTITY_INDEX:          TableDefinition<&str, &[u8]> = TableDefinition::new("kb_entity_index");

// Sync + dedup
pub const KB_SEEN_ITEMS:            TableDefinition<&str, &[u8]> = TableDefinition::new("kb_seen_items");
pub const KB_SYNC_STATE:            TableDefinition<&str, &[u8]> = TableDefinition::new("kb_sync_state");

// Outbox / atomicity
pub const KB_LEDGER:                TableDefinition<&str, &[u8]> = TableDefinition::new("kb_ledger");
pub const KB_JOBS_BY_ID:            TableDefinition<&str, &[u8]> = TableDefinition::new("kb_jobs_by_id");
pub const KB_JOBS_BY_DEDUPE_ACTIVE: TableDefinition<&str, &str>  = TableDefinition::new("kb_jobs_by_dedupe_active");
pub const KB_JOBS_BY_STATUS_PRIO:   TableDefinition<&[u8], &[u8]> = TableDefinition::new("kb_jobs_by_status_priority");
pub const KB_JOB_CLAIMS:            TableDefinition<&str, &[u8]> = TableDefinition::new("kb_job_claims");

pub fn open_db(path: &std::path::Path) -> anyhow::Result<redb::Database> {
    let db = redb::Database::create(path)?;
    let wtx = db.begin_write()?;
    // Open all tables to ensure they exist.
    let _ = wtx.open_table(KB_DOCS)?;
    let _ = wtx.open_table(KB_DOC_LATEST_VERSION)?;
    let _ = wtx.open_table(KB_CHUNKS)?;
    let _ = wtx.open_table(KB_CHUNK_BY_LOGICAL)?;
    let _ = wtx.open_table(KB_ENTITIES)?;
    let _ = wtx.open_table(KB_ENTITY_INDEX)?;
    let _ = wtx.open_table(KB_SEEN_ITEMS)?;
    let _ = wtx.open_table(KB_SYNC_STATE)?;
    let _ = wtx.open_table(KB_LEDGER)?;
    let _ = wtx.open_table(KB_JOBS_BY_ID)?;
    let _ = wtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE)?;
    let _ = wtx.open_table(KB_JOBS_BY_STATUS_PRIO)?;
    let _ = wtx.open_table(KB_JOB_CLAIMS)?;
    wtx.commit()?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_all_12_tables() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        let rtx = db.begin_read().unwrap();
        // If any table is missing, open_table errors.
        rtx.open_table(KB_DOCS).unwrap();
        rtx.open_table(KB_DOC_LATEST_VERSION).unwrap();
        rtx.open_table(KB_CHUNKS).unwrap();
        rtx.open_table(KB_CHUNK_BY_LOGICAL).unwrap();
        rtx.open_table(KB_ENTITIES).unwrap();
        rtx.open_table(KB_ENTITY_INDEX).unwrap();
        rtx.open_table(KB_SEEN_ITEMS).unwrap();
        rtx.open_table(KB_SYNC_STATE).unwrap();
        rtx.open_table(KB_LEDGER).unwrap();
        rtx.open_table(KB_JOBS_BY_ID).unwrap();
        rtx.open_table(KB_JOBS_BY_DEDUPE_ACTIVE).unwrap();
        rtx.open_table(KB_JOBS_BY_STATUS_PRIO).unwrap();
        rtx.open_table(KB_JOB_CLAIMS).unwrap();
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("kb.redb");
        let _a = open_db(&p).unwrap();
        let _b = open_db(&p).unwrap();
    }
}
```

`src/kb/store/mod.rs`:
```rust
pub mod schema;
pub use schema::open_db;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::store::schema
git add src/kb/store/
git commit -m "feat(kb): redb schema covering all 12 KB tables (Week 2/3 add accessors)"
```

---

## Task 11: `content_store/atomic.rs` — atomic write + SHA

**Files:** `src/kb/content_store/atomic.rs`, `src/kb/content_store/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Atomic file write that refuses to overwrite. Returns Ok(false) if
/// file already exists (caller treats as no-op).
pub fn write_if_new(path: &Path, bytes: &[u8]) -> Result<bool> {
    if path.exists() { return Ok(false); }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
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
    Ok(true)
}

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
    use super::*;
    use tempfile::TempDir;

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
    fn overwrite_replaces() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        write_if_new(&p, b"first").unwrap();
        overwrite_atomic(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn sha256_known_value() {
        assert_eq!(sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
```

`src/kb/content_store/mod.rs`:
```rust
pub mod atomic;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::content_store::atomic
git add src/kb/content_store/
git commit -m "feat(kb): atomic write_if_new + overwrite_atomic + sha256_hex"
```

---

## Task 12: `content_store/paths.rs` — slugify + rel_path helpers

**Files:** `src/kb/content_store/paths.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use crate::kb::model::KbSourceKind;

pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut prev_dash = false;
    for c in title.chars() {
        let keep = c.is_alphanumeric() || is_cjk(c);
        if keep {
            for lc in c.to_lowercase() { out.push(lc); }
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out.chars().take(80).collect()
}

fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)
       || (0x3040..=0x30FF).contains(&cp)
       || (0xAC00..=0xD7AF).contains(&cp)
}

pub fn markdown_rel_path(kind: KbSourceKind, slug: &str) -> String {
    format!("md/{}/{}.md", kind.as_str(), slug)
}

pub fn raw_rel_path(doc_id: &str, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() { format!("raw/{doc_id}") }
    else { format!("raw/{doc_id}.{ext}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_cjk() {
        assert_eq!(slugify("蒙牛 奶粉 冲泡指南"), "蒙牛-奶粉-冲泡指南");
    }

    #[test]
    fn slugify_max_len() {
        assert!(slugify(&"x".repeat(200)).chars().count() <= 80);
    }

    #[test]
    fn markdown_rel_per_kind() {
        assert_eq!(markdown_rel_path(KbSourceKind::Doc, "蒙牛"), "md/doc/蒙牛.md");
        assert_eq!(markdown_rel_path(KbSourceKind::Url, "x"), "md/url/x.md");
    }

    #[test]
    fn raw_rel_with_ext() {
        assert_eq!(raw_rel_path("01HXY", "pdf"), "raw/01HXY.pdf");
        assert_eq!(raw_rel_path("01HXY", ""), "raw/01HXY");
    }
}
```

Update `content_store/mod.rs`:
```rust
pub mod atomic;
pub mod paths;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::content_store::paths
git add src/kb/content_store/
git commit -m "feat(kb): slugify + markdown_rel_path + raw_rel_path helpers"
```

---

## Task 13: `content_store/compose.rs` — YAML front-matter

**Files:** `src/kb/content_store/compose.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrontMatter {
    pub title: String,
    pub source_kind: String,
    pub logical_source_id: String,
    pub created_at: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

pub fn compose_doc_file(fm: &FrontMatter, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(fm).context("yaml")?;
    Ok(format!("---\n{yaml}---\n\n{body}"))
}

#[derive(Debug)]
pub struct Parsed {
    pub front: FrontMatter,
    pub body: String,
    pub body_offset: usize,
}

pub fn parse_doc_file(content: &str) -> Result<Parsed> {
    let bytes = content.as_bytes();
    if !content.starts_with("---\n") {
        return Err(anyhow!("missing front-matter open"));
    }
    let after = &bytes[4..];
    let needle = b"\n---\n";
    let pos = after.windows(needle.len()).position(|w| w == needle)
        .ok_or_else(|| anyhow!("missing front-matter close"))?;
    let yaml_end = 4 + pos;
    let yaml = std::str::from_utf8(&bytes[4..yaml_end]).context("front-matter utf8")?;
    let front: FrontMatter = serde_yaml::from_str(yaml).context("yaml parse")?;
    let body_start = yaml_end + needle.len();
    let body_start = if bytes.get(body_start) == Some(&b'\n') { body_start + 1 } else { body_start };
    let body = std::str::from_utf8(&bytes[body_start..]).context("body utf8")?.to_string();
    Ok(Parsed { front, body, body_offset: body_start })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm() -> FrontMatter {
        FrontMatter {
            title: "T".into(),
            source_kind: "doc".into(),
            logical_source_id: "file:sha256:abc".into(),
            created_at: "2026-05-19T00:00:00Z".into(),
            tags: vec!["a".into()],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn roundtrip() {
        let body = "# Hi\n\nWorld.";
        let composed = compose_doc_file(&fm(), body).unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(parsed.body, body);
        assert_eq!(parsed.front.title, "T");
    }

    #[test]
    fn body_offset_correct() {
        let composed = compose_doc_file(&fm(), "BODY").unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(&composed.as_bytes()[parsed.body_offset..], b"BODY");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_doc_file("no front matter").is_err());
        assert!(parse_doc_file("---\nfoo\nbody").is_err());
    }
}
```

Update `content_store/mod.rs`:
```rust
pub mod atomic;
pub mod paths;
pub mod compose;
pub use compose::{compose_doc_file, parse_doc_file, FrontMatter, Parsed};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::content_store::compose
git add src/kb/content_store/
git commit -m "feat(kb): YAML front-matter compose + parse (body_offset preserved)"
```

---

## Task 14: `content_store/read.rs` — read_doc_body + read_doc_range + verify_doc_sha

**Files:** `src/kb/content_store/read.rs`, update `mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use crate::kb::content_store::atomic::sha256_hex;
use crate::kb::content_store::compose::parse_doc_file;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

pub fn read_doc_body(abs: &Path) -> Result<String> {
    let s = std::fs::read_to_string(abs).with_context(|| format!("read {}", abs.display()))?;
    Ok(parse_doc_file(&s)?.body)
}

pub fn read_doc_range(abs: &Path, start: u64, end_excl: u64) -> Result<String> {
    let s = std::fs::read_to_string(abs)?;
    let parsed = parse_doc_file(&s)?;
    let bytes = parsed.body.as_bytes();
    let (s_, e_) = (start as usize, end_excl as usize);
    if e_ > bytes.len() || s_ > e_ {
        return Err(anyhow!("range {s_}..{e_} oob (body len {})", bytes.len()));
    }
    Ok(std::str::from_utf8(&bytes[s_..e_])?.to_string())
}

pub fn verify_doc_sha(abs: &Path, expected: &str) -> Result<()> {
    let body = read_doc_body(abs)?;
    let actual = sha256_hex(body.as_bytes());
    if actual != expected {
        return Err(anyhow!("sha mismatch for {}: expected {expected} got {actual}", abs.display()));
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
```

Update `content_store/mod.rs`:
```rust
pub mod atomic;
pub mod paths;
pub mod compose;
pub mod read;
pub use compose::{compose_doc_file, parse_doc_file, FrontMatter, Parsed};
pub use read::{read_doc_body, read_doc_range, verify_doc_sha};
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::content_store::read
git add src/kb/content_store/
git commit -m "feat(kb): read_doc_body + read_doc_range + verify_doc_sha"
```

---

## Task 15: `content_store/mod.rs` — `stage_doc` public API

**Files:** modify `src/kb/content_store/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! On-disk content store. Files at `md/<kind>/<slug>.md` (atomic) and
//! optional raw at `raw/<doc_id>.<ext>`. DB stores relative paths +
//! sha256 + byte_offset only.

pub mod atomic;
pub mod paths;
pub mod compose;
pub mod read;

use crate::kb::model::KbSourceKind;
use crate::kb::paths::KbPaths;
use anyhow::Result;
pub use compose::{compose_doc_file, parse_doc_file, FrontMatter, Parsed};
pub use read::{read_doc_body, read_doc_range, verify_doc_sha};

#[derive(Debug, Clone)]
pub struct StagedDoc {
    pub doc_id: String,
    pub markdown_rel_path: String,
    pub markdown_sha256: String,
    pub raw_rel_path: Option<String>,
    pub body_offset_in_file: usize,
}

#[derive(Debug)]
pub struct StageInput<'a> {
    pub doc_id: &'a str,
    pub kind: KbSourceKind,
    pub slug: &'a str,
    pub front: FrontMatter,
    pub body: &'a str,
    pub raw: Option<(&'a [u8], &'a str)>,  // (bytes, ext)
    pub keep_raw: bool,
}

pub fn stage_doc(paths: &KbPaths, input: StageInput<'_>) -> Result<StagedDoc> {
    let md_rel = paths::markdown_rel_path(input.kind, input.slug);
    let md_abs = paths.root.join(&md_rel);
    let composed = compose_doc_file(&input.front, input.body)?;
    atomic::write_if_new(&md_abs, composed.as_bytes())?;
    let parsed = parse_doc_file(&composed)?;
    let md_sha = atomic::sha256_hex(parsed.body.as_bytes());

    let raw_rel = if input.keep_raw {
        if let Some((bytes, ext)) = input.raw {
            let rel = paths::raw_rel_path(input.doc_id, ext);
            atomic::write_if_new(&paths.root.join(&rel), bytes)?;
            Some(rel)
        } else { None }
    } else { None };

    Ok(StagedDoc {
        doc_id: input.doc_id.to_string(),
        markdown_rel_path: md_rel,
        markdown_sha256: md_sha,
        raw_rel_path: raw_rel,
        body_offset_in_file: parsed.body_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn stage_md_and_raw() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(&p, StageInput {
            doc_id: "01HXY", kind: KbSourceKind::Doc, slug: "test",
            front: fm(), body: "# Hi",
            raw: Some((b"raw", "pdf")), keep_raw: true,
        }).unwrap();
        assert_eq!(s.markdown_rel_path, "md/doc/test.md");
        assert_eq!(s.raw_rel_path.as_deref(), Some("raw/01HXY.pdf"));
    }

    #[test]
    fn skip_raw_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(&p, StageInput {
            doc_id: "01H", kind: KbSourceKind::Doc, slug: "n",
            front: fm(), body: "x",
            raw: Some((b"r", "txt")), keep_raw: false,
        }).unwrap();
        assert!(s.raw_rel_path.is_none());
    }

    #[test]
    fn stage_then_read_range() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        let s = stage_doc(&p, StageInput {
            doc_id: "01H", kind: KbSourceKind::Doc, slug: "r",
            front: fm(), body: "0123456789",
            raw: None, keep_raw: false,
        }).unwrap();
        assert_eq!(read_doc_range(&p.root.join(&s.markdown_rel_path), 3, 7).unwrap(), "3456");
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::content_store
git add src/kb/content_store/
git commit -m "feat(kb): content_store::stage_doc public API"
```

---

## Task 16: `canonicalize/url_canon.rs` — URL canonicalization

**Files:** `src/kb/canonicalize/url_canon.rs`, `src/kb/canonicalize/mod.rs` stub

- [ ] **Step 1: Write impl + tests**

Strip common trackers (utm_*, fbclid, gclid, ref, mc_*, etc.), sort query params, lowercase scheme+host, drop fragment.

```rust
use anyhow::{Context, Result};

/// Canonicalize a URL into a form usable as logical_source_id.
/// See spec §I.
pub fn canonicalize_url(raw: &str) -> Result<String> {
    let mut u = url::Url::parse(raw).context("parse url")?;
    let scheme = u.scheme().to_lowercase();
    let _ = u.set_scheme(&scheme);
    if let Some(host) = u.host_str() {
        let lc = host.to_lowercase();
        let _ = u.set_host(Some(&lc));
    }
    u.set_fragment(None);
    let pairs: Vec<(String, String)> = u.query_pairs()
        .filter(|(k, _)| !is_tracker(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let mut sorted = pairs;
    sorted.sort();
    u.query_pairs_mut().clear();
    for (k, v) in &sorted {
        u.query_pairs_mut().append_pair(k, v);
    }
    if u.query() == Some("") { u.set_query(None); }
    Ok(u.to_string())
}

fn is_tracker(k: &str) -> bool {
    if k.starts_with("utm_") { return true; }
    matches!(k,
        "fbclid" | "gclid" | "msclkid" | "yclid" | "dclid" | "_ga" | "_gl"
        | "ref"   | "ref_src" | "ref_url" | "referrer" | "source"
        | "mc_cid" | "mc_eid"
        | "spm"   // taobao/aliexpress
        | "share_session_id" | "share_id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_host_scheme() {
        assert_eq!(
            canonicalize_url("HTTPS://Example.COM/path").unwrap(),
            "https://example.com/path"
        );
    }

    #[test]
    fn strip_fragment() {
        assert_eq!(
            canonicalize_url("https://a.com/x#frag").unwrap(),
            "https://a.com/x"
        );
    }

    #[test]
    fn strip_utm() {
        let c = canonicalize_url(
            "https://example.com/x?utm_source=a&utm_campaign=b&real=keep"
        ).unwrap();
        assert!(!c.contains("utm_"));
        assert!(c.contains("real=keep"));
    }

    #[test]
    fn strip_common_trackers() {
        let c = canonicalize_url("https://x.com/p?fbclid=1&gclid=2&keep=yes").unwrap();
        assert!(!c.contains("fbclid"));
        assert!(!c.contains("gclid"));
        assert!(c.contains("keep=yes"));
    }

    #[test]
    fn sort_params_for_stability() {
        let a = canonicalize_url("https://x.com/p?b=2&a=1").unwrap();
        let b = canonicalize_url("https://x.com/p?a=1&b=2").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn tracking_only_url_collapses_query() {
        let c = canonicalize_url("https://x.com/p?utm_source=a").unwrap();
        assert_eq!(c, "https://x.com/p");
    }
}
```

`src/kb/canonicalize/mod.rs` (initial):
```rust
pub mod url_canon;
pub use url_canon::canonicalize_url;
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize::url_canon
git add src/kb/canonicalize/
git commit -m "feat(kb): canonicalize_url strips trackers, sorts query, lowercases host"
```

---

## Task 17: `canonicalize/mod.rs` — Canonicalizer trait + CanonicalizedSource

**Files:** modify `src/kb/canonicalize/mod.rs`

- [ ] **Step 1: Write trait + types + tests**

```rust
pub mod url_canon;
pub mod text;
pub mod md;
pub mod html;
pub mod pdf;
pub mod mime;

use crate::kb::model::{KbSourceKind, LogicalSourceId};
use anyhow::Result;
use serde::{Deserialize, Serialize};

pub use url_canon::canonicalize_url;

#[derive(Debug, Clone)]
pub struct CanonicalizedSource {
    pub markdown: String,
    pub metadata: CanonicalMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalMetadata {
    pub source_kind: KbSourceKind,
    pub logical_source_id: LogicalSourceId,
    pub title: String,
    pub mime: String,
    pub created_at_ms: i64,
    pub tags: Vec<String>,
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CanonicalizeInput<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
    pub hint_title: Option<&'a str>,
    /// For file sources, sha256 of `bytes`. For URL sources, the
    /// canonicalized URL string. Used to seed logical_source_id when
    /// the canonicalizer can't compute it itself.
    pub logical_source_id_seed: Option<LogicalSourceId>,
}

pub trait Canonicalizer: Send + Sync {
    fn source_kind(&self) -> KbSourceKind;
    fn supports_mime(&self, mime: &str) -> bool;
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::canonicalize::text::TextCanonicalizer;

    #[test]
    fn trait_dispatch() {
        let c = TextCanonicalizer;
        assert!(c.supports_mime("text/plain"));
        assert!(!c.supports_mime("application/pdf"));
    }
}
```

Create stub files so `cargo check` passes (replaced in Tasks 18–21):

```bash
for f in text md html pdf mime; do : > "src/kb/canonicalize/$f.rs"; done
```

For `text.rs` provide a minimal stub:

```rust
// src/kb/canonicalize/text.rs (will be replaced in Task 18)
use super::*;

pub struct TextCanonicalizer;

impl Canonicalizer for TextCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool { mime == "text/plain" }
    fn canonicalize(&self, _input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        unimplemented!()
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize
git add src/kb/canonicalize/
git commit -m "feat(kb): Canonicalizer trait + CanonicalizedSource + CanonicalizeInput"
```

---

## Task 18: `canonicalize/text.rs` — passthrough

**Files:** replace `src/kb/canonicalize/text.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct TextCanonicalizer;

impl Canonicalizer for TextCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/plain" | "text/x-log" | "text/csv")
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("not utf8: {e}"))?
            .trim().to_string();
        if body.is_empty() { return Ok(None); }
        let lsid = input.logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Untitled").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough() {
        let r = TextCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: b"hello",
            mime: "text/plain",
            hint_title: Some("G"),
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        assert_eq!(r.markdown, "hello");
        assert_eq!(r.metadata.title, "G");
        assert!(r.metadata.logical_source_id.as_str().starts_with("file:sha256:"));
    }

    #[test]
    fn empty_returns_none() {
        let r = TextCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: b"  \n  ",
            mime: "text/plain",
            hint_title: None,
            logical_source_id_seed: None,
        }).unwrap();
        assert!(r.is_none());
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize::text
git add src/kb/canonicalize/text.rs
git commit -m "feat(kb): TextCanonicalizer (passthrough + logical_source_id from sha256)"
```

---

## Task 19: `canonicalize/md.rs` — markdown + heading_path

**Files:** `src/kb/canonicalize/md.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct MdCanonicalizer;

impl Canonicalizer for MdCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/markdown" | "text/x-markdown")
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("not utf8: {e}"))?
            .trim().to_string();
        if body.is_empty() { return Ok(None); }
        let title = first_h1(&body)
            .or_else(|| input.hint_title.map(String::from))
            .unwrap_or_else(|| "Untitled".to_string());
        let lsid = input.logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title,
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

fn first_h1(md: &str) -> Option<String> {
    md.lines().find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches('#').trim().to_string())
}

/// Given a byte position in markdown, return the heading path stack.
pub fn heading_path_at(md: &str, byte_pos: usize) -> Vec<String> {
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut offset = 0usize;
    for line in md.lines() {
        if offset > byte_pos { break; }
        if let Some((level, text)) = parse_heading_line(line) {
            while let Some(top) = stack.last() {
                if top.0 >= level as u8 { stack.pop(); } else { break; }
            }
            stack.push((level as u8, text.trim().to_string()));
        }
        offset += line.len() + 1;
    }
    stack.into_iter().map(|(_, t)| t).collect()
}

fn parse_heading_line(line: &str) -> Option<(usize, &str)> {
    let lead = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&lead) && line.as_bytes().get(lead) == Some(&b' ') {
        Some((lead, &line[lead + 1..]))
    } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulls_h1_title() {
        let r = MdCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: b"# Doc\n\nbody",
            mime: "text/markdown",
            hint_title: None,
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "Doc");
    }

    #[test]
    fn heading_path_basic() {
        let md = "# A\n## B\nbody1\n## C\nbody2\n### C1\nbody3";
        assert_eq!(
            heading_path_at(md, md.find("body3").unwrap()),
            vec!["A".to_string(), "C".to_string(), "C1".to_string()]
        );
    }

    #[test]
    fn heading_path_pops_correctly() {
        let md = "# A\n## B\n## C\nbody";
        assert_eq!(
            heading_path_at(md, md.find("body").unwrap()),
            vec!["A".to_string(), "C".to_string()]
        );
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize::md
git add src/kb/canonicalize/md.rs
git commit -m "feat(kb): MdCanonicalizer + heading_path_at"
```

---

## Task 20: `canonicalize/html.rs` — HTML strip → markdown

**Files:** `src/kb/canonicalize/html.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use super::*;
use crate::kb::content_store::atomic::sha256_hex;
use lol_html::{element, HtmlRewriter, Settings};

pub struct HtmlCanonicalizer;

impl Canonicalizer for HtmlCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/html" | "application/xhtml+xml")
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let stripped = strip_to_text(input.bytes)?;
        let trimmed = stripped.trim();
        if trimmed.is_empty() { return Ok(None); }
        let title = extract_title(input.bytes)
            .unwrap_or_else(|| input.hint_title.unwrap_or("Untitled").to_string());
        let lsid = input.logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: trimmed.to_string(),
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title,
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

fn strip_to_text(html: &[u8]) -> Result<String> {
    let mut sink = Vec::<u8>::new();
    {
        let mut r = HtmlRewriter::new(
            Settings {
                element_content_handlers: vec![
                    element!("script", |el| { el.remove(); Ok(()) }),
                    element!("style",  |el| { el.remove(); Ok(()) }),
                    element!("h1, h2, h3, h4, h5, h6", |el| {
                        let level = el.tag_name().as_str()
                            .strip_prefix('h').and_then(|n| n.parse::<usize>().ok())
                            .unwrap_or(1);
                        let prefix = "#".repeat(level);
                        el.before(&format!("\n{prefix} "), lol_html::html_content::ContentType::Text);
                        el.after("\n", lol_html::html_content::ContentType::Text);
                        Ok(())
                    }),
                    element!("p, br, li", |el| {
                        el.before("\n", lol_html::html_content::ContentType::Text);
                        Ok(())
                    }),
                ],
                ..Settings::default()
            },
            |chunk: &[u8]| sink.extend_from_slice(chunk),
        );
        r.write(html)?;
        r.end()?;
    }
    let s = String::from_utf8(sink).map_err(|e| anyhow::anyhow!(e))?;
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    Ok(out.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn extract_title(html: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(html).ok()?;
    let lower = s.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")? + start;
    Some(s[start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_scripts_styles() {
        let r = HtmlCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: b"<html><body><script>alert(1)</script><p>Hi</p><style>x{}</style></body></html>",
            mime: "text/html", hint_title: None,
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        assert!(!r.markdown.contains("alert"));
        assert!(!r.markdown.contains("x{}"));
        assert!(r.markdown.contains("Hi"));
    }

    #[test]
    fn extract_title_from_head() {
        let r = HtmlCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: b"<html><head><title>Page</title></head><body><p>X</p></body></html>",
            mime: "text/html", hint_title: None,
            logical_source_id_seed: None,
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "Page");
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize::html
git add src/kb/canonicalize/html.rs
git commit -m "feat(kb): HtmlCanonicalizer (lol-html strip → markdown)"
```

---

## Task 21: `canonicalize/pdf.rs` — text-layer PDF (no OCR)

**Files:** `src/kb/canonicalize/pdf.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use super::*;
use crate::kb::content_store::atomic::sha256_hex;

pub struct PdfCanonicalizer;

impl Canonicalizer for PdfCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool { mime == "application/pdf" }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let pages = extract_pages(input.bytes)?;
        let mut md = String::new();
        let mut has = false;
        for (i, p) in pages.iter().enumerate() {
            let t = p.trim();
            if t.is_empty() { continue; }
            if has { md.push_str("\n\n"); }
            md.push_str(&format!("## Page {}\n\n{t}", i + 1));
            has = true;
        }
        if !has { return Ok(None); }
        let lsid = input.logical_source_id_seed
            .clone()
            .unwrap_or_else(|| LogicalSourceId::for_file(&sha256_hex(input.bytes)));
        Ok(Some(CanonicalizedSource {
            markdown: md,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                logical_source_id: lsid,
                title: input.hint_title.unwrap_or("Untitled PDF").to_string(),
                mime: input.mime.to_string(),
                created_at_ms: chrono::Utc::now().timestamp_millis(),
                tags: vec![],
                extra: serde_json::json!({ "n_pages": pages.len() }),
            },
        }))
    }
}

fn extract_pages(bytes: &[u8]) -> Result<Vec<String>> {
    let text = pdf_extract::extract_text_from_mem(bytes)
        .map_err(|e| anyhow::anyhow!("pdf-extract: {e:?}"))?;
    Ok(text.split('\u{0C}').map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_handled() {
        let r = PdfCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: &[], mime: "application/pdf",
            hint_title: None, logical_source_id_seed: None,
        });
        // Either Ok(None) or Err is acceptable for empty input.
        match r {
            Ok(None) | Err(_) => {}
            Ok(Some(_)) => panic!("unexpected content from empty"),
        }
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize::pdf
git add src/kb/canonicalize/pdf.rs
git commit -m "feat(kb): PdfCanonicalizer text-layer extraction (no OCR in Week 1)"
```

---

## Task 22: `canonicalize/mime.rs` — detect_mime + dispatch

**Files:** `src/kb/canonicalize/mime.rs`

- [ ] **Step 1: Write impl + tests**

```rust
use super::*;
use crate::kb::canonicalize::{
    html::HtmlCanonicalizer, md::MdCanonicalizer,
    pdf::PdfCanonicalizer, text::TextCanonicalizer,
};

pub fn detect_mime(bytes: &[u8], filename_hint: Option<&str>) -> String {
    if bytes.starts_with(b"%PDF-") { return "application/pdf".into(); }
    if let Some(name) = filename_hint {
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "md" | "markdown" => return "text/markdown".into(),
            "html" | "htm" => return "text/html".into(),
            "pdf" => return "application/pdf".into(),
            "txt" | "log" => return "text/plain".into(),
            _ => {}
        }
    }
    if bytes.iter().take(512).all(|b|
        *b == b'\n' || *b == b'\r' || *b == b'\t' || (*b >= 0x20 && *b < 0x7f)
    ) {
        return "text/plain".into();
    }
    "application/octet-stream".into()
}

pub fn canonicalize_by_mime(input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
    let registered: &[&dyn Canonicalizer] = &[
        &MdCanonicalizer, &HtmlCanonicalizer, &PdfCanonicalizer, &TextCanonicalizer,
    ];
    for c in registered {
        if c.supports_mime(input.mime) {
            return c.canonicalize(input);
        }
    }
    Err(anyhow::anyhow!("no canonicalizer for mime '{}'", input.mime))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_pdf_magic() {
        assert_eq!(detect_mime(b"%PDF-1.5\n", None), "application/pdf");
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect_mime(b"# x", Some("a.md")), "text/markdown");
        assert_eq!(detect_mime(b"<", Some("a.html")), "text/html");
        assert_eq!(detect_mime(b"x", Some("a.txt")), "text/plain");
    }

    #[test]
    fn dispatch_routes_to_md() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"# Hi\nbody", mime: "text/markdown",
            hint_title: None, logical_source_id_seed: None,
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "Hi");
    }

    #[test]
    fn unknown_mime_errors() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"x", mime: "application/x-unknown",
            hint_title: None, logical_source_id_seed: None,
        });
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::canonicalize
git add src/kb/canonicalize/mime.rs
git commit -m "feat(kb): mime detection + canonicalize_by_mime dispatch"
```

---

## Task 23: `chunker/tokens.rs` + `chunker/splitter.rs`

**Files:** `src/kb/chunker/tokens.rs`, `src/kb/chunker/splitter.rs`, `src/kb/chunker/mod.rs` stub

- [ ] **Step 1: tokens.rs**

```rust
//! Approximate token count (4-char heuristic). Week 2 will replace
//! with actual BGE-M3 tokenizer.

pub fn approx_token_count(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    chars.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linear() {
        assert_eq!(approx_token_count(""), 0);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
        assert_eq!(approx_token_count(&"x".repeat(400)), 100);
    }
}
```

- [ ] **Step 2: splitter.rs**

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct Paragraph {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

pub fn split_paragraphs(md: &str) -> Vec<Paragraph> {
    let bytes = md.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i+1] == b'\n' {
            push(&mut out, md, start, i);
            i += 2;
            while i < bytes.len() && bytes[i] == b'\n' { i += 1; }
            start = i;
        } else { i += 1; }
    }
    push(&mut out, md, start, bytes.len());
    out
}

fn push(out: &mut Vec<Paragraph>, md: &str, start: usize, end: usize) {
    let slice = &md[start..end];
    let t = slice.trim();
    if !t.is_empty() {
        let leading = slice.len() - slice.trim_start().len();
        let trailing = slice.len() - slice.trim_end().len();
        out.push(Paragraph {
            start: start + leading,
            end: end - trailing,
            text: t.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_blank_lines() {
        let md = "a\n\nb\n\nc";
        let p = split_paragraphs(md);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].text, "a");
        assert_eq!(p[2].text, "c");
    }

    #[test]
    fn handles_trailing_newlines() {
        let p = split_paragraphs("a\n\n\n\nb\n\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "a");
        assert_eq!(p[1].text, "b");
    }

    #[test]
    fn preserves_byte_offsets() {
        let md = "first.\n\nsecond.";
        let p = split_paragraphs(md);
        assert_eq!(&md[p[0].start..p[0].end], "first.");
        assert_eq!(&md[p[1].start..p[1].end], "second.");
    }
}
```

`src/kb/chunker/mod.rs`:
```rust
pub mod tokens;
pub mod splitter;
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p rsclaw --lib kb::chunker
git add src/kb/chunker/
git commit -m "feat(kb): approx_token_count + paragraph splitter (byte_offset preserved)"
```

---

## Task 24: `chunker/mod.rs` — `chunk_markdown` (uses logical_source_id)

**Files:** modify `src/kb/chunker/mod.rs`

- [ ] **Step 1: Write impl + tests**

```rust
//! Slice canonical markdown into chunks with deterministic ids
//! derived from logical_source_id. See spec §I + §2.

pub mod tokens;
pub mod splitter;

use crate::kb::canonicalize::md::heading_path_at;
use crate::kb::model::{
    chunk_id, hamming64, simhash64,
    ChunkStatus, KbChunk, KbLocator, LogicalSourceId,
};
use splitter::split_paragraphs;
use tokens::approx_token_count;

pub const DEFAULT_TARGET_TOKENS: u32 = 512;
pub const DEFAULT_MIN_TOKENS: u32 = 50;
pub const SIMHASH_DEDUP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone)]
pub struct ChunkerInput<'a> {
    pub logical_source_id: &'a LogicalSourceId,
    pub doc_id: &'a str,
    pub doc_version: u32,
    pub markdown_body: &'a str,
    pub default_locator_kind: LocatorKind,
}

#[derive(Debug, Clone, Copy)]
pub enum LocatorKind { Offset, MdSection }

pub fn chunk_markdown(input: ChunkerInput<'_>) -> Vec<KbChunk> {
    let paras = split_paragraphs(input.markdown_body);
    let mut chunks: Vec<KbChunk> = Vec::new();
    let mut buf = String::new();
    let mut buf_start: Option<usize> = None;
    let mut buf_end: usize = 0;
    let mut seq = 0u32;

    for p in &paras {
        let tentative = approx_token_count(&buf) + approx_token_count(&p.text);
        if !buf.is_empty() && tentative > DEFAULT_TARGET_TOKENS {
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
            buf.clear();
            buf_start = None;
        }
        if buf.is_empty() { buf_start = Some(p.start); }
        if !buf.is_empty() { buf.push_str("\n\n"); }
        buf.push_str(&p.text);
        buf_end = p.end;
        if approx_token_count(&buf) >= DEFAULT_TARGET_TOKENS {
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
            buf.clear();
            buf_start = None;
        }
    }
    if !buf.is_empty() {
        flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf);
    }
    deduplicate(&mut chunks);
    chunks
}

fn flush(
    out: &mut Vec<KbChunk>, seq: &mut u32, input: &ChunkerInput<'_>,
    start: usize, end: usize, body: &str,
) {
    let path = heading_path_at(input.markdown_body, start);
    let indexed = if path.is_empty() {
        body.to_string()
    } else {
        format!("{}\n\n{body}", path.join(" > "))
    };
    let id = chunk_id(input.logical_source_id, *seq, body);
    let sim = simhash64(body);
    let locator = match input.default_locator_kind {
        LocatorKind::Offset => KbLocator::Offset { start, end },
        LocatorKind::MdSection => KbLocator::MdSection { heading_path: path.clone() },
    };
    out.push(KbChunk {
        id,
        doc_id: input.doc_id.to_string(),
        logical_source_id: input.logical_source_id.0.clone(),
        doc_version: input.doc_version,
        seq: *seq,
        heading_path: path,
        byte_offset: (start as u64, end as u64),
        indexed_text: indexed,
        vector: vec![],   // Week 2 fills
        simhash: sim,
        locator,
        status: ChunkStatus::Active,
        source_quality: 1.0,
        embedder_id: String::new(),
    });
    *seq += 1;
}

fn deduplicate(chunks: &mut Vec<KbChunk>) {
    let mut kept: Vec<u64> = Vec::with_capacity(chunks.len());
    chunks.retain(|c| {
        for sh in &kept {
            if hamming64(*sh, c.simhash) <= SIMHASH_DEDUP_THRESHOLD {
                return false;
            }
        }
        kept.push(c.simhash);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsid() -> LogicalSourceId { LogicalSourceId::for_file("hash") }

    #[test]
    fn short_doc_one_chunk() {
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: "tiny.",
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn long_doc_multi_chunks() {
        let md = "para one.\n\n".to_string() + &"para text here.\n\n".repeat(500);
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: &md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert!(chunks.len() > 1);
    }

    #[test]
    fn heading_path_in_indexed_text() {
        let md = "# Mengniu\n## Recipe\n100g + 100ml.";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::MdSection,
        });
        assert!(chunks[0].indexed_text.starts_with("Mengniu > Recipe"));
        assert_eq!(chunks[0].heading_path, vec!["Mengniu".to_string(), "Recipe".to_string()]);
    }

    #[test]
    fn idempotent_chunk_ids() {
        let md = "hello.\n\nworld.";
        let a = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        let b = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d2", doc_version: 5,    // different doc_id and version
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.id, y.id, "chunk_id must NOT depend on doc_id/version, only on logical_source_id");
        }
    }

    #[test]
    fn near_dup_dedup() {
        let md = "the quick brown fox jumps over the lazy dog\n\n\
                  the quick brown fox jumps over the lazy dog";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(chunks.len(), 1, "identical chunks must be deduped");
    }

    #[test]
    fn byte_offsets_in_bounds() {
        let md = "first.\n\nsecond.";
        let chunks = chunk_markdown(ChunkerInput {
            logical_source_id: &lsid(),
            doc_id: "d1", doc_version: 1,
            markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        for c in chunks {
            let (s, e) = (c.byte_offset.0 as usize, c.byte_offset.1 as usize);
            assert!(e <= md.len());
            assert!(s < e);
            assert!(!md[s..e].trim().is_empty());
        }
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p rsclaw --lib kb::chunker
git add src/kb/chunker/mod.rs
git commit -m "feat(kb): chunk_markdown with logical_source_id-derived chunk_id + heading_path + dedup"
```

---

## Task 25: Public façade `src/kb/mod.rs`

**Files:** modify `src/kb/mod.rs`

- [ ] **Step 1: Add re-exports**

```rust
//! rsclaw Knowledge Base. See docs/specs/2026-05-19-knowledge-base.md.

pub mod paths;
pub mod model;
pub mod content_store;
pub mod store;
pub mod canonicalize;
pub mod chunker;
pub mod ledger;
pub mod jobs;
pub mod util;

pub use paths::KbPaths;
pub use model::{
    chunk_id, hamming64, simhash64,
    CallerScope, ChunkStatus, EntityKind, KbChunk, KbDoc, KbEntity,
    KbEntityIndex, KbLocator, KbSource, KbSourceKind, KbStatus,
    KbVisibility, LogicalSourceId, MailSource, VersionPointer,
};
pub use content_store::{
    compose_doc_file, parse_doc_file, read_doc_body, read_doc_range,
    stage_doc, verify_doc_sha, FrontMatter, StageInput, StagedDoc,
};
pub use canonicalize::{
    canonicalize_by_mime, canonicalize_url, detect_mime,
    CanonicalMetadata, CanonicalizeInput, CanonicalizedSource,
};
pub use chunker::{chunk_markdown, ChunkerInput, LocatorKind};
pub use store::open_db;
pub use ledger::{IngestLedgerEntry, LedgerOp, LedgerStatus};
pub use jobs::{ClaimToken, Job, JobKind, JobStatus};
pub use util::redact;
```

- [ ] **Step 2: Verify + commit**

```bash
cargo check
cargo test -p rsclaw --lib kb::
git add src/kb/mod.rs
git commit -m "feat(kb): public façade with canonical re-exports"
```

---

## Task 26: Integration test — end-to-end (canonicalize → stage → chunk)

**Files:** `tests/kb_week1_e2e.rs`, `tests/fixtures/kb/sample.{md,html,txt}`

- [ ] **Step 1: Add fixtures**

```bash
mkdir -p tests/fixtures/kb
cat > tests/fixtures/kb/sample.md <<'EOF'
# Mengniu Milk Powder Guide

## Recipe
Mix 100g of Mengniu milk powder with 100ml of warm water.

## Storage
Keep cool and dry. Use within 30 days.
EOF

cat > tests/fixtures/kb/sample.html <<'EOF'
<html><head><title>Sample HTML</title></head>
<body>
<script>alert(1)</script>
<h1>Heading</h1>
<p>Hello world.</p>
</body></html>
EOF

cat > tests/fixtures/kb/sample.txt <<'EOF'
Plain text fixture for Week 1 e2e.
EOF
```

- [ ] **Step 2: Write integration test**

```rust
//! Week 1 end-to-end: canonicalize → stage → chunk. No DB writes yet
//! (Week 2 introduces ledger/outbox writes).

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown,
    content_store::{stage_doc, FrontMatter, StageInput},
    detect_mime, read_doc_range,
    CanonicalizeInput, ChunkerInput, KbPaths, KbSourceKind, LocatorKind,
};
use std::path::Path;
use tempfile::TempDir;
use ulid::Ulid;

fn pipeline(paths: &KbPaths, src_path: &Path) -> Result<(String, usize)> {
    let bytes = std::fs::read(src_path)?;
    let name = src_path.file_name().and_then(|s| s.to_str()).unwrap_or("untitled");
    let mime = detect_mime(&bytes, Some(name));

    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes,
        mime: &mime,
        hint_title: Some(name),
        logical_source_id_seed: None,
    })?.expect("should canonicalize");

    let doc_id = Ulid::new().to_string();
    let staged = stage_doc(paths, StageInput {
        doc_id: &doc_id,
        kind: canon.metadata.source_kind,
        slug: name,
        front: FrontMatter {
            title: canon.metadata.title.clone(),
            source_kind: canon.metadata.source_kind.as_str().to_string(),
            logical_source_id: canon.metadata.logical_source_id.as_str().to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            tags: canon.metadata.tags.clone(),
            meta: canon.metadata.extra.clone(),
        },
        body: &canon.markdown,
        raw: Some((&bytes, name.rsplit('.').next().unwrap_or(""))),
        keep_raw: true,
    })?;

    let chunks = chunk_markdown(ChunkerInput {
        logical_source_id: &canon.metadata.logical_source_id,
        doc_id: &doc_id,
        doc_version: 1,
        markdown_body: &canon.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });

    // Verify chunks' byte_offset reads back from the on-disk file.
    let abs = paths.root.join(&staged.markdown_rel_path);
    for c in &chunks {
        let body = read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1)?;
        assert!(!body.trim().is_empty());
    }
    Ok((doc_id, chunks.len()))
}

#[test]
fn e2e_markdown() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let (doc_id, n) = pipeline(&paths, Path::new("tests/fixtures/kb/sample.md"))?;
    assert!(n > 0, "got {n} chunks");
    let md_path = paths.root.join(format!("md/doc/sample.md"));
    assert!(md_path.exists(), "markdown file should exist");
    let _ = doc_id;
    Ok(())
}

#[test]
fn e2e_html_strips_scripts() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let (_doc_id, _n) = pipeline(&paths, Path::new("tests/fixtures/kb/sample.html"))?;
    let md = std::fs::read_to_string(paths.root.join("md/doc/sample.html.md"))?;
    assert!(!md.contains("alert"), "script must be stripped");
    Ok(())
}

#[test]
fn e2e_text() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let (_id, n) = pipeline(&paths, Path::new("tests/fixtures/kb/sample.txt"))?;
    assert!(n >= 1);
    Ok(())
}

/// CRITICAL: re-ingesting same file produces same chunk_ids regardless
/// of doc_id. This is the idempotency invariant from spec §I.
#[test]
fn reingest_same_file_same_chunk_ids() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;

    let bytes = std::fs::read("tests/fixtures/kb/sample.md")?;
    let mime = detect_mime(&bytes, Some("sample.md"));

    let canon1 = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes, mime: &mime,
        hint_title: Some("sample.md"),
        logical_source_id_seed: None,
    })?.unwrap();

    let canon2 = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes, mime: &mime,
        hint_title: Some("sample.md"),
        logical_source_id_seed: None,
    })?.unwrap();

    assert_eq!(canon1.metadata.logical_source_id, canon2.metadata.logical_source_id,
        "same file bytes → same logical_source_id");

    let a = chunk_markdown(ChunkerInput {
        logical_source_id: &canon1.metadata.logical_source_id,
        doc_id: "doc_A", doc_version: 1,   // simulate first ingest
        markdown_body: &canon1.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });
    let b = chunk_markdown(ChunkerInput {
        logical_source_id: &canon2.metadata.logical_source_id,
        doc_id: "doc_B", doc_version: 5,   // simulate later re-ingest (different doc_id + version)
        markdown_body: &canon2.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });

    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.id, y.id,
            "chunk_id MUST be identical across re-ingests (idempotency)");
    }
    Ok(())
}

#[test]
fn url_canonicalization_idempotent() -> Result<()> {
    use rsclaw::kb::canonicalize_url;
    let a = canonicalize_url("https://example.com/p?utm_source=a&b=2&a=1")?;
    let b = canonicalize_url("https://example.com/p?a=1&b=2")?;
    assert_eq!(a, b, "tracker stripping + sorting must produce same canonical url");
    Ok(())
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test --test kb_week1_e2e
git add tests/kb_week1_e2e.rs tests/fixtures/kb/
git commit -m "test(kb): Week 1 e2e — canonicalize → stage → chunk + idempotency invariants"
```

---

## Task 27: README

**Files:** `src/kb/README.md`

- [ ] **Step 1: Write README**

```markdown
# `src/kb/` — Knowledge Base

User-managed RAG knowledge base. See `docs/specs/2026-05-19-knowledge-base.md`
for the full design.

## What's implemented (Week 1)

- **Types** (`model/`): KbDoc / KbChunk (with `logical_source_id` for
  idempotency) / KbEntity / KbEntityIndex / LogicalSourceId / KbLocator /
  KbVisibility / CallerScope / VersionPointer / SimHash-64.
- **Content store** (`content_store/`): atomic markdown writes under
  `~/.rsclaw/kb/md/<kind>/<slug>.md` with YAML front-matter; optional
  raw bytes under `~/.rsclaw/kb/raw/<doc_id>.<ext>`; `read_doc_range`
  for lazy chunk-body retrieval.
- **redb schema** (`store/`): all 12 tables defined and openable
  (kb_docs / kb_doc_latest_version / kb_chunks / kb_chunk_by_logical /
  kb_entities / kb_entity_index / kb_seen_items / kb_sync_state /
  kb_ledger / kb_jobs_by_id / kb_jobs_by_dedupe_active /
  kb_jobs_by_status_priority / kb_job_claims).
- **Ledger/Jobs types** (`ledger/`, `jobs/`): structs and enums for the
  IngestLedger + Outbox pattern; accessors come in Week 2.
- **Canonicalizers** (`canonicalize/`): markdown, plain text, HTML
  (via lol-html), PDF text layer (no OCR), URL canonicalization
  (tracker stripping, param sorting).
- **Chunker** (`chunker/`): paragraph-based splitter with
  `heading_path` injection into `indexed_text`; chunk_ids derived from
  `logical_source_id` for re-ingest idempotency; SimHash near-dup
  dedup.

## What's NOT in Week 1

- redb accessors (read/write of KbDoc / KbChunk / Ledger / Jobs) → Week 2
- Embedder (BGE-M3 local) → Week 2
- Worker pool + chunk+embed pipeline → Week 2
- Tantivy `add_document` + HNSW `insert` → Week 3
- Hybrid retrieval / kb_search tool → Week 3
- Visibility filter implementation → Week 3
- ManualUploadSyncer + UrlSyncer + CLI → Week 4
- Compactor → Week 4

## Architecture invariants (verify after every code change)

1. **chunk_id depends on logical_source_id, never on doc_id or version**:
   re-ingesting the same file produces identical chunk_ids
   (covered by `tests::reingest_same_file_same_chunk_ids`)
2. **Files are stage-only**: nothing in `canonicalize/` or
   `content_store/` deletes files (deletion happens via compactor +
   ledger reconciliation in Week 4)
3. **No SQL pretense**: redb queries are KV / range-scan only; never use
   SQL terminology (no "partial unique index", no "UPDATE...RETURNING")
4. **PII in logs goes through `util::redact`**: source ids and content
   previews emit only `redact(s)` (first 8 hex of sha256)

## Quick start (Week 1 only)

```rust
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown, detect_mime,
    content_store::stage_doc,
    CanonicalizeInput, ChunkerInput, KbPaths, LocatorKind,
};

let paths = KbPaths::new("~/.rsclaw/kb");
paths.ensure_layout()?;

let bytes = std::fs::read("manual.md")?;
let mime = detect_mime(&bytes, Some("manual.md"));
let canon = canonicalize_by_mime(CanonicalizeInput {
    bytes: &bytes, mime: &mime,
    hint_title: Some("manual.md"), logical_source_id_seed: None,
})?.unwrap();
// ... see tests/kb_week1_e2e.rs for full flow.
```

## Testing

```bash
cargo test -p rsclaw --lib kb::          # unit tests (~60+)
cargo test --test kb_week1_e2e           # integration tests
```
```

- [ ] **Step 2: Commit**

```bash
git add src/kb/README.md
git commit -m "docs(kb): Week 1 README with scope + architecture invariants"
```

---

## Self-review (engineer runs after Task 27)

Before marking Week 1 complete:

- [ ] `cargo test` all green
- [ ] `cargo clippy -- -D warnings` clean
- [ ] `cargo fmt --check` clean
- [ ] No `unwrap()` / `println!()` / `dbg!()` in non-test code
- [ ] All `log::` calls with PII go through `redact()`
- [ ] `~/.rsclaw/kb/{md/doc,md/url,raw,db,idx,hnsw,state}/` layout matches spec
- [ ] All 12 redb tables open + idempotent
- [ ] **`reingest_same_file_same_chunk_ids` test passes** (the most important Week 1 invariant)
- [ ] `url_canonicalization_idempotent` test passes
- [ ] HTML fixture's `<script>` does not appear in any stored markdown

## What's next: Week 2

After Week 1 ships, write Plan for Week 2 via `superpowers:writing-plans`. Week 2 scope:

- redb accessors for KbDoc / KbChunk / Ledger / Job tables
- `ingest_canonicalized` writer: NOOP short-circuit + stage + single-tx
  (KbDoc + Ledger + Job + seen_items + latest_version)
- LocalBgeM3 embedder (BGE-M3 ONNX via `ort`)
- Job worker pool: claim → handler dispatch → mark_done with claim_token
- `ChunkAndEmbed` handler: read body, chunk, embed, write chunks
- `reclaim_stale_jobs` + worker restart recovery test
- Crash-recovery integration tests (kill mid-pipeline, verify resumption)
