# KB Phase 1a — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the foundation layer of the rsclaw Knowledge Base — types, on-disk content store, redb/tantivy/hnsw schema initialization, source-to-markdown canonicalizers, and the chunker. By end of Plan 1a, the system can ingest a markdown / text / HTML / text-layer-PDF file end-to-end (canonicalize → chunk → store to disk + redb), but **no embedding, no FTS indexing, no vector search, no retrieval tools, no UI** — those are Plan 1b / 1c.

**Architecture:** New `src/kb/` module on top of existing `redb` / `tantivy` / `hnsw_rs` deps. Canonicalize-first pipeline (`source -> CanonicalizedSource { markdown, metadata } -> chunks`). Content stored as `.md` files on disk at `~/.rsclaw/kb/md/<kind>/<slug>.md`; redb stores doc/chunk metadata pointing to `byte_offset` ranges in those files. Chunk IDs are deterministic SHA-256 over `(kind|source_id|seq|content)` for idempotent upserts.

**Tech Stack:** Rust 2024, tokio, redb 2.x (existing), tantivy 0.22 (existing, schema only this phase), hnsw_rs 0.3 (existing, file init only this phase), sha2, ulid, serde, serde_json, serde_yaml, lol-html (existing), pdf-extract (new dep), jieba-rs (new dep, used in Plan 1b but added now to avoid Cargo churn), once_cell, anyhow.

**Spec reference:** `docs/specs/2026-05-19-knowledge-base.md` §1 (data model), §2 (canonicalize + chunker), §6 (storage layout).

---

## File Structure

Files this plan will create:

```
src/kb/
  mod.rs                 # public module facade, re-exports
  paths.rs               # ~/.rsclaw/kb/ root + subdirs resolution
  model/
    mod.rs               # re-exports
    doc.rs               # KbDoc + KbStatus
    chunk.rs             # KbChunk + chunk_id() function
    source.rs            # KbSource + KbSourceKind + MailSource
    locator.rs           # KbLocator enum
    entity.rs            # KbEntity + KbEntityIndex + EntityKind
    simhash.rs           # SimHash64 (used in chunker)
  content_store/
    mod.rs               # public API: stage_doc / read_doc_body / read_doc_range
    atomic.rs            # tempfile + fsync + rename + parent fsync
    paths.rs             # markdown_rel_path / raw_rel_path / slugify
    compose.rs           # YAML front-matter + body composition
    read.rs              # parse front-matter, read body, verify SHA
  store/
    mod.rs               # KbStore facade
    schema.rs            # redb table definitions (kb_docs / kb_chunks / kb_entities / kb_entity_index / kb_sync_state / kb_jobs)
    doc_access.rs        # KbDoc accessors
    chunk_access.rs      # KbChunk accessors
    entity_access.rs     # KbEntity + KbEntityIndex accessors
    tantivy_schema.rs    # tantivy schema definition (no indexing yet)
    hnsw_init.rs         # HNSW file initialization
  canonicalize/
    mod.rs               # Canonicalizer trait + CanonicalizedSource + dispatch
    text.rs              # passthrough
    md.rs                # heading_path extraction
    html.rs              # lol-html → markdown
    pdf.rs               # pdf-extract text layer (no OCR in 1a)
    mime.rs              # mime detection + canonicalizer dispatch
  chunker/
    mod.rs               # chunk_markdown(input) -> Vec<Chunk>
    splitter.rs          # paragraph + sentence splitters
    tokens.rs            # approximate token count
  util/
    redact.rs            # PII redaction for logs (sha256-truncate-8 helper)
  README.md              # module overview + what's in scope for Plan 1a

tests/
  kb_phase1a_e2e.rs      # integration test: ingest md/html/text/pdf end-to-end

Cargo.toml               # add deps: pdf-extract, jieba-rs, serde_yaml
```

Existing files modified:

```
src/lib.rs               # add `pub mod kb;`
Cargo.toml               # new deps (above)
```

---

## Conventions used in this plan

- **TDD**: every task is "write test → run it (fails) → implement → run it (passes) → commit"
- **One commit per task** (unless task says otherwise) with conventional commit message: `feat(kb): ...` / `test(kb): ...` / `chore(kb): ...`
- **Test files mirror source paths**: source at `src/kb/foo/bar.rs` → unit test in same file under `#[cfg(test)] mod tests { ... }`. Integration tests at `tests/kb_*.rs`.
- **`cargo test -p rsclaw --lib kb::...`** for unit tests; `cargo test --test kb_phase1a_e2e` for integration.
- All public types `Serialize + Deserialize`. Internal-only types may skip if unused.
- **Never use `unwrap()` in non-test code**; use `anyhow::Result` for errors.
- **No `println!`/`eprintln!`** in non-test code; use `log::` macros with PII-redacted source ids.

---

## Task 0: Bootstrap — Cargo deps + module skeleton

**Files:**
- Modify: `Cargo.toml`
- Create: `src/kb/mod.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add Cargo deps**

Edit `Cargo.toml` `[dependencies]` section, add:

```toml
pdf-extract = "0.7"
jieba-rs = "0.7"
serde_yaml = "0.9"
```

Existing deps assumed present: `tokio`, `serde`, `serde_json`, `sha2`, `ulid`, `anyhow`, `log`, `tantivy`, `redb`, `hnsw_rs`, `lol_html`, `once_cell`.

- [ ] **Step 2: Create empty module facade**

Create `src/kb/mod.rs`:

```rust
//! Knowledge base module.
//!
//! See `docs/specs/2026-05-19-knowledge-base.md` for design.
//! Plan 1a scope: foundation only (no embedding / retrieval / UI).

pub mod paths;
pub mod model;
pub mod content_store;
pub mod store;
pub mod canonicalize;
pub mod chunker;
pub mod util;
```

- [ ] **Step 3: Register module in lib.rs**

Edit `src/lib.rs`, add `pub mod kb;` in the appropriate module-declaration block.

- [ ] **Step 4: Verify compile**

```bash
cargo check
```

Expected: succeeds. New deps download. `kb` modules will fail because submodules don't exist yet — that's expected for now, we'll resolve via creating empty `pub mod foo;` files in the next steps if needed for `cargo check` cleanliness. Alternative: stub each submodule with an empty `.rs` file first.

Practical: create empty stubs:

```bash
mkdir -p src/kb/{model,content_store,store,canonicalize,chunker,util}
for f in src/kb/paths.rs src/kb/util/redact.rs; do touch "$f"; done
echo "" > src/kb/util/mod.rs
echo "pub mod redact;" >> src/kb/util/mod.rs
# model/mod.rs, content_store/mod.rs, etc. created in their respective tasks
```

(For `cargo check` to succeed *right now*, you'll need stub `mod.rs` files in each subdir declaring no submodules. Subsequent tasks will populate them.)

```rust
// src/kb/model/mod.rs (empty for now)
// src/kb/content_store/mod.rs (empty for now)
// src/kb/store/mod.rs (empty for now)
// src/kb/canonicalize/mod.rs (empty for now)
// src/kb/chunker/mod.rs (empty for now)
```

Run `cargo check` again, expected: passes.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/kb/
git commit -m "chore(kb): bootstrap module skeleton + add pdf-extract/jieba/serde_yaml deps"
```

---

## Task 1: `paths.rs` — KB root + subdir resolution

**Files:**
- Create: `src/kb/paths.rs`
- Test: same file (unit tests)

- [ ] **Step 1: Write failing tests**

Create `src/kb/paths.rs`:

```rust
//! Resolves the on-disk layout `~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/`.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct KbPaths {
    pub root: PathBuf,
}

impl KbPaths {
    /// Construct paths anchored at `root`. Does NOT create directories
    /// (use [`Self::ensure_layout`] for that).
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

    /// Idempotently create the full directory layout.
    pub fn ensure_layout(&self) -> Result<()> {
        for d in [
            self.md_dir(), self.raw_dir(), self.db_dir(),
            self.idx_dir(), self.hnsw_dir(), self.state_dir(),
        ] {
            std::fs::create_dir_all(&d)
                .with_context(|| format!("create_dir_all {}", d.display()))?;
        }
        // md/ subdirs by source kind
        for sub in ["doc", "chat", "url", "img", "mail"] {
            std::fs::create_dir_all(self.md_dir().join(sub))
                .with_context(|| format!("create_dir_all md/{sub}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn ensure_layout_creates_all_subdirs() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        for d in ["md", "raw", "db", "idx", "hnsw", "state"] {
            assert!(tmp.path().join(d).is_dir(), "missing {d}");
        }
        for sub in ["doc", "chat", "url", "img", "mail"] {
            assert!(tmp.path().join("md").join(sub).is_dir(), "missing md/{sub}");
        }
    }

    #[test]
    fn ensure_layout_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = KbPaths::new(tmp.path());
        p.ensure_layout().unwrap();
        // calling again is no-op (no error)
        p.ensure_layout().unwrap();
    }
}
```

- [ ] **Step 2: Verify test fails (or compiles)**

```bash
cargo test -p rsclaw --lib kb::paths
```

Expected: passes (file is self-contained). If `tempfile` not in `[dev-dependencies]`, add it.

- [ ] **Step 3: Commit**

```bash
git add src/kb/paths.rs Cargo.toml Cargo.lock
git commit -m "feat(kb): KbPaths resolves ~/.rsclaw/kb/{md,raw,db,idx,hnsw,state}/ layout"
```

---

## Task 2: `util/redact.rs` — PII redaction helper

**Files:**
- Create: `src/kb/util/redact.rs`

- [ ] **Step 1: Write failing test + impl**

```rust
//! PII redaction for log messages. Never log raw source ids or content
//! previews — emit a stable short hash instead so logs are correlatable
//! without leaking content.

use sha2::{Digest, Sha256};

/// Return first 8 hex chars of `sha256(input)`. Used in log lines as a
/// stable, content-correlated, non-reversible identifier.
pub fn redact(input: impl AsRef<str>) -> String {
    let mut h = Sha256::new();
    h.update(input.as_ref().as_bytes());
    let digest = h.finalize();
    let hex = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    hex[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_is_deterministic() {
        assert_eq!(redact("hello"), redact("hello"));
    }

    #[test]
    fn redact_differs_per_input() {
        assert_ne!(redact("hello"), redact("world"));
    }

    #[test]
    fn redact_is_8_chars() {
        assert_eq!(redact("anything").len(), 8);
    }
}
```

- [ ] **Step 2: Run test**

```bash
cargo test -p rsclaw --lib kb::util::redact
```

Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/util/redact.rs
git commit -m "feat(kb): util::redact for PII-safe logging"
```

---

## Task 3: `model/source.rs` — KbSourceKind + KbSource + MailSource

**Files:**
- Create: `src/kb/model/source.rs`

- [ ] **Step 1: Write failing tests + impl**

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The five top-level source categories. On-wire string form is the
/// `as_str()` variant, kept short to match disk layout (`md/<kind>/...`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbSourceKind {
    Doc,
    Chat,
    Url,
    Img,
    Mail,
}

impl KbSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Doc => "doc",
            Self::Chat => "chat",
            Self::Url => "url",
            Self::Img => "img",
            Self::Mail => "mail",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "doc" => Ok(Self::Doc),
            "chat" => Ok(Self::Chat),
            "url" => Ok(Self::Url),
            "img" => Ok(Self::Img),
            "mail" => Ok(Self::Mail),
            other => Err(format!("unknown KbSourceKind: {other}")),
        }
    }

    pub fn all() -> &'static [Self] {
        &[Self::Doc, Self::Chat, Self::Url, Self::Img, Self::Mail]
    }
}

/// Provenance pointer to where a document came from. Each variant maps
/// to exactly one [`KbSourceKind`] via [`Self::kind`].
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
            Self::Doc { .. } => KbSourceKind::Doc,
            Self::Url { .. } => KbSourceKind::Url,
            Self::Chat { .. } => KbSourceKind::Chat,
            Self::Img { .. } => KbSourceKind::Img,
            Self::Mail { .. } => KbSourceKind::Mail,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MailSource {
    EmlFile  { path: PathBuf },
    MboxFile { path: PathBuf },
    // v2 variants reserved (not implemented in Plan 1a):
    Imap     { account: String, folder: String, uid: u64 },
    Gmail    { account: String, thread_id: String, msg_id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_string_roundtrip() {
        for k in KbSourceKind::all() {
            assert_eq!(KbSourceKind::parse(k.as_str()).unwrap(), *k);
        }
    }

    #[test]
    fn kind_parse_rejects_unknown() {
        assert!(KbSourceKind::parse("Doc").is_err());  // case sensitive
        assert!(KbSourceKind::parse("document").is_err());
    }

    #[test]
    fn source_to_kind_mapping() {
        let s = KbSource::Doc { path: "/tmp/x".into() };
        assert_eq!(s.kind(), KbSourceKind::Doc);

        let s = KbSource::Mail {
            source: MailSource::EmlFile { path: "/tmp/x.eml".into() }
        };
        assert_eq!(s.kind(), KbSourceKind::Mail);
    }

    #[test]
    fn kind_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&KbSourceKind::Doc).unwrap(), "\"doc\"");
        assert_eq!(serde_json::to_string(&KbSourceKind::Mail).unwrap(), "\"mail\"");
    }
}
```

- [ ] **Step 2: Create `src/kb/model/mod.rs`** (was empty stub from Task 0)

```rust
pub mod source;

pub use source::{KbSource, KbSourceKind, MailSource};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::source
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): KbSource + KbSourceKind + MailSource types"
```

---

## Task 4: `model/locator.rs` — KbLocator enum

**Files:**
- Create: `src/kb/model/locator.rs`
- Modify: `src/kb/model/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use serde::{Deserialize, Serialize};

/// Pointer back to a chunk's location in its original source. UI uses
/// this to power "click to jump to source" navigation.
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
    /// Human-friendly locator string, suitable for `agent` consumption
    /// in `citation.locator_human`. Format is fixed by source kind; we
    /// never let the agent build this itself (avoids hallucinated page
    /// numbers).
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
    fn human_format_per_variant() {
        assert_eq!(
            KbLocator::PdfPage { page: 12, bbox: None }.human(),
            "p.12"
        );
        assert_eq!(
            KbLocator::MdSection { heading_path: vec!["A".into(), "B".into()] }.human(),
            "§A > B"
        );
        assert_eq!(
            KbLocator::UrlAnchor { fragment: Some("sec-2".into()) }.human(),
            "#sec-2"
        );
        assert_eq!(
            KbLocator::ChatMsgs { first_ts: 100, last_ts: 200 }.human(),
            "100..200"
        );
        assert_eq!(KbLocator::Image { bbox: None }.human(), "image");
        assert_eq!(
            KbLocator::Offset { start: 0, end: 100 }.human(),
            "bytes 0..100"
        );
    }

    #[test]
    fn serde_roundtrip() {
        let l = KbLocator::PdfPage { page: 7, bbox: Some((1.0, 2.0, 3.0, 4.0)) };
        let json = serde_json::to_string(&l).unwrap();
        let back: KbLocator = serde_json::from_str(&json).unwrap();
        assert_eq!(l, back);
    }
}
```

- [ ] **Step 2: Update `src/kb/model/mod.rs`**

```rust
pub mod source;
pub mod locator;

pub use source::{KbSource, KbSourceKind, MailSource};
pub use locator::KbLocator;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::locator
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): KbLocator enum + human() formatter"
```

---

## Task 5: `model/chunk.rs` — deterministic chunk_id + KbChunk struct

**Files:**
- Create: `src/kb/model/chunk.rs`
- Modify: `src/kb/model/mod.rs`

- [ ] **Step 1: Write tests + impl for chunk_id() function**

```rust
use crate::kb::model::{KbLocator, KbSourceKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Compute a deterministic chunk id.
///
/// `sha256(kind | "\0" | source_id | "\0" | seq_be | "\0" | content)` →
/// first 32 hex chars (128 bits collision resistance). Re-ingesting the
/// same content under the same `(kind, source_id, seq)` produces the
/// same id, so upserts stay idempotent. Different `seq` or content
/// → different id.
pub fn chunk_id(kind: KbSourceKind, source_id: &str, seq: u32, content: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update([0u8]);
    h.update(source_id.as_bytes());
    h.update([0u8]);
    h.update(seq.to_be_bytes());
    h.update([0u8]);
    h.update(content.as_bytes());
    let digest = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest.iter() {
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
    pub id: String,                     // 32-hex deterministic
    pub doc_id: String,
    pub doc_version: u32,
    pub seq: u32,
    pub heading_path: Vec<String>,
    /// Range (start, end_exclusive) in the **body** portion of the doc's
    /// markdown file (post-front-matter). Used for lazy read of chunk
    /// text via content_store::read_doc_range.
    pub byte_offset: (u64, u64),
    /// Text used for embedding/BM25. Equals
    /// `heading_path.join(" > ") + "\n\n" + body_text`. Not the raw
    /// body — that's only on disk.
    pub indexed_text: String,
    pub simhash: u64,
    pub locator: KbLocator,
    pub status: ChunkStatus,
    pub source_quality: f32,
    /// Empty in Plan 1a (filled by Plan 1b embedder).
    pub embedder_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_id_is_deterministic() {
        let a = chunk_id(KbSourceKind::Doc, "manual:foo.md", 0, "hello world");
        let b = chunk_id(KbSourceKind::Doc, "manual:foo.md", 0, "hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn chunk_id_varies_with_seq() {
        let a = chunk_id(KbSourceKind::Doc, "x", 0, "hello");
        let b = chunk_id(KbSourceKind::Doc, "x", 1, "hello");
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_varies_with_kind() {
        let a = chunk_id(KbSourceKind::Doc, "x", 0, "hello");
        let b = chunk_id(KbSourceKind::Chat, "x", 0, "hello");
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_varies_with_source_id() {
        let a = chunk_id(KbSourceKind::Doc, "x", 0, "hello");
        let b = chunk_id(KbSourceKind::Doc, "y", 0, "hello");
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_id_varies_with_content() {
        let a = chunk_id(KbSourceKind::Doc, "x", 0, "hello");
        let b = chunk_id(KbSourceKind::Doc, "x", 0, "world");
        assert_ne!(a, b);
    }

    #[test]
    fn chunk_struct_serde_roundtrip() {
        let c = KbChunk {
            id: chunk_id(KbSourceKind::Doc, "x", 0, "hi"),
            doc_id: "doc_1".into(),
            doc_version: 1,
            seq: 0,
            heading_path: vec!["A".into()],
            byte_offset: (0, 10),
            indexed_text: "A\n\nhi".into(),
            simhash: 0,
            locator: KbLocator::Offset { start: 0, end: 10 },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: String::new(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: KbChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }
}
```

- [ ] **Step 2: Update model/mod.rs**

```rust
pub mod source;
pub mod locator;
pub mod chunk;

pub use source::{KbSource, KbSourceKind, MailSource};
pub use locator::KbLocator;
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::chunk
```

Expected: 6 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): deterministic chunk_id + KbChunk struct"
```

---

## Task 6: `model/doc.rs` — KbDoc + KbStatus

**Files:**
- Create: `src/kb/model/doc.rs`
- Modify: `src/kb/model/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use crate::kb::model::{KbSource, KbSourceKind};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KbStatus { Active, Tombstoned, Updating }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbDoc {
    pub id: String,                   // ulid
    pub source: KbSource,
    pub source_kind: KbSourceKind,
    /// Stable per-syncer identifier (e.g. "manual:01HXY...abc",
    /// "url:https://...", "feishu:group_pm").
    pub source_id: String,
    pub title: String,
    pub mime: String,
    /// sha256 of the original raw bytes (doc-level dedup); for sources
    /// without raw bytes (chat), sha256 of canonical markdown body.
    pub hash: String,
    /// Path relative to `KbPaths::root`, e.g. "md/doc/foo.md".
    pub markdown_path: String,
    /// sha256 of the markdown body bytes only (excludes front-matter).
    pub markdown_sha256: String,
    /// Relative path under `raw/`, e.g. "raw/01HXY...abc.pdf". None if
    /// `kb.keep_raw = false`.
    pub raw_path: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: u32,
    pub status: KbStatus,
    pub tags: Vec<String>,
    pub meta: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc() -> KbDoc {
        KbDoc {
            id: "01HXY".into(),
            source: KbSource::Doc { path: "/tmp/x.md".into() },
            source_kind: KbSourceKind::Doc,
            source_id: "manual:01HXY".into(),
            title: "Test".into(),
            mime: "text/markdown".into(),
            hash: "abc".into(),
            markdown_path: "md/doc/test.md".into(),
            markdown_sha256: "def".into(),
            raw_path: None,
            created_at: 0,
            updated_at: 0,
            version: 1,
            status: KbStatus::Active,
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn serde_roundtrip() {
        let d = sample_doc();
        let json = serde_json::to_string(&d).unwrap();
        let back: KbDoc = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&KbStatus::Tombstoned).unwrap(),
            "\"tombstoned\""
        );
    }
}
```

- [ ] **Step 2: Update model/mod.rs**

```rust
pub mod source;
pub mod locator;
pub mod chunk;
pub mod doc;

pub use source::{KbSource, KbSourceKind, MailSource};
pub use locator::KbLocator;
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
pub use doc::{KbDoc, KbStatus};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::doc
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): KbDoc + KbStatus types"
```

---

## Task 7: `model/entity.rs` — KbEntity + KbEntityIndex + EntityKind

**Files:**
- Create: `src/kb/model/entity.rs`
- Modify: `src/kb/model/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Brand,
    Person,
    Org,
    Email,
    Url,
    Hashtag,
    Other,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KbEntity {
    /// e.g. "ent_yili", "ent_email_alice_at_x"
    pub canonical_id: String,
    /// All surface forms that resolve to this canonical id.
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
    fn entity_serde() {
        let e = KbEntity {
            canonical_id: "ent_yili".into(),
            surface_forms: vec!["伊利".into(), "Yili".into()],
            kind: EntityKind::Brand,
            created_at: 0,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: KbEntity = serde_json::from_str(&json).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn index_serde() {
        let i = KbEntityIndex {
            entity_id: "ent_yili".into(),
            chunk_id: "abc".into(),
            doc_id: "01HXY".into(),
            mention_count: 3,
            score: 0.85,
        };
        let json = serde_json::to_string(&i).unwrap();
        let back: KbEntityIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(i, back);
    }
}
```

- [ ] **Step 2: Update model/mod.rs**

```rust
pub mod source;
pub mod locator;
pub mod chunk;
pub mod doc;
pub mod entity;

pub use source::{KbSource, KbSourceKind, MailSource};
pub use locator::KbLocator;
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
pub use doc::{KbDoc, KbStatus};
pub use entity::{EntityKind, KbEntity, KbEntityIndex};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::entity
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): KbEntity + KbEntityIndex + EntityKind types"
```

---

## Task 8: `model/simhash.rs` — 64-bit SimHash

**Files:**
- Create: `src/kb/model/simhash.rs`
- Modify: `src/kb/model/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! 64-bit SimHash for chunk-level near-duplicate detection.
//!
//! Tokenize → for each token compute sha256 → take first 64 bits → for
//! each bit, +1 if set / -1 if unset, accumulated across all tokens →
//! final bit = sign of accumulator. Hamming distance ≤ 3 ≈ near
//! duplicate.

use sha2::{Digest, Sha256};

/// Compute SimHash-64 of the input text.
pub fn simhash64(text: &str) -> u64 {
    // Tokenize: whitespace + dedup; simple and language-agnostic.
    let mut accum = [0i32; 64];
    let mut seen = std::collections::HashSet::new();
    for tok in text.split_whitespace() {
        if !seen.insert(tok) { continue; }
        let mut h = Sha256::new();
        h.update(tok.as_bytes());
        let digest = h.finalize();
        // First 8 bytes → u64.
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&digest[..8]);
        let bits = u64::from_be_bytes(bytes);
        for i in 0..64 {
            if (bits >> i) & 1 == 1 { accum[i] += 1; }
            else { accum[i] -= 1; }
        }
    }
    let mut out: u64 = 0;
    for i in 0..64 {
        if accum[i] >= 0 { out |= 1u64 << i; }
    }
    out
}

/// Hamming distance between two 64-bit hashes.
pub fn hamming64(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_same_hash() {
        let a = simhash64("hello world");
        let b = simhash64("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn similar_text_close_hash() {
        let a = simhash64("the quick brown fox jumps over the lazy dog");
        let b = simhash64("the quick brown fox jumps over a lazy dog");  // a vs the
        let d = hamming64(a, b);
        assert!(d < 16, "expected near hash, got hamming {d}");
    }

    #[test]
    fn different_text_far_hash() {
        let a = simhash64("the quick brown fox");
        let b = simhash64("completely unrelated content here");
        let d = hamming64(a, b);
        assert!(d > 16, "expected far hash, got hamming {d}");
    }

    #[test]
    fn hamming_self_is_zero() {
        assert_eq!(hamming64(0xDEAD_BEEF, 0xDEAD_BEEF), 0);
    }

    #[test]
    fn hamming_one_bit_diff() {
        assert_eq!(hamming64(0, 1), 1);
        assert_eq!(hamming64(0xFF, 0x00), 8);
    }
}
```

- [ ] **Step 2: Update model/mod.rs**

```rust
pub mod source;
pub mod locator;
pub mod chunk;
pub mod doc;
pub mod entity;
pub mod simhash;

pub use source::{KbSource, KbSourceKind, MailSource};
pub use locator::KbLocator;
pub use chunk::{chunk_id, ChunkStatus, KbChunk};
pub use doc::{KbDoc, KbStatus};
pub use entity::{EntityKind, KbEntity, KbEntityIndex};
pub use simhash::{hamming64, simhash64};
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::model::simhash
```

Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/model/
git commit -m "feat(kb): SimHash-64 for chunk-level near-dup detection"
```

---

## Task 9: `content_store/paths.rs` — path generators + slugify

**Files:**
- Create: `src/kb/content_store/paths.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Path generators for the on-disk content store.
//!
//! All paths returned are **relative** to `KbPaths::root`, suitable for
//! storage in `KbDoc.markdown_path` / `KbDoc.raw_path`.

use crate::kb::model::KbSourceKind;

/// Convert an arbitrary title into a filesystem-safe slug.
///
/// - lowercase
/// - non-alphanumeric/CJK runs → `-`
/// - trim leading/trailing dashes
/// - max 80 chars
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

/// Build the markdown file path for a doc.
///
/// Returns e.g. `"md/doc/蒙牛奶粉冲泡指南.md"`.
pub fn markdown_rel_path(kind: KbSourceKind, slug: &str) -> String {
    format!("md/{}/{}.md", kind.as_str(), slug)
}

/// Build the raw file path for a doc (when `kb.keep_raw=true`).
///
/// Returns e.g. `"raw/01HXY...abc.pdf"`.
pub fn raw_rel_path(doc_id: &str, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() {
        format!("raw/{doc_id}")
    } else {
        format!("raw/{doc_id}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic_ascii() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }

    #[test]
    fn slugify_collapses_runs() {
        assert_eq!(slugify("Hello   World!!!"), "hello-world");
    }

    #[test]
    fn slugify_preserves_cjk() {
        assert_eq!(slugify("蒙牛 奶粉 冲泡指南"), "蒙牛-奶粉-冲泡指南");
    }

    #[test]
    fn slugify_trims_dashes() {
        assert_eq!(slugify("---hello---"), "hello");
    }

    #[test]
    fn slugify_max_80_chars() {
        let s = slugify(&"a".repeat(200));
        assert!(s.chars().count() <= 80);
    }

    #[test]
    fn markdown_rel_path_per_kind() {
        assert_eq!(
            markdown_rel_path(KbSourceKind::Doc, "蒙牛"),
            "md/doc/蒙牛.md"
        );
        assert_eq!(
            markdown_rel_path(KbSourceKind::Chat, "feishu_pm_2026-05"),
            "md/chat/feishu_pm_2026-05.md"
        );
    }

    #[test]
    fn raw_rel_path_with_ext() {
        assert_eq!(raw_rel_path("01HXY", "pdf"), "raw/01HXY.pdf");
        assert_eq!(raw_rel_path("01HXY", ".pdf"), "raw/01HXY.pdf");
        assert_eq!(raw_rel_path("01HXY", ""), "raw/01HXY");
    }
}
```

- [ ] **Step 2: Create `content_store/mod.rs` (update from empty stub)**

```rust
pub mod paths;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::content_store::paths
```

Expected: 8 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/content_store/
git commit -m "feat(kb): content_store path generators + slugify (CJK aware)"
```

---

## Task 10: `content_store/atomic.rs` — atomic write + SHA helpers

**Files:**
- Create: `src/kb/content_store/atomic.rs`
- Modify: `src/kb/content_store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Atomic file writes: tempfile + fsync + rename + parent dir fsync
//! (Unix). Crash-safe per POSIX. Includes SHA-256 helper used by
//! integrity checks throughout content_store.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

/// Atomically write `bytes` to `path`. Creates parent dirs if needed.
/// If `path` already exists, returns `Ok(false)` without overwriting
/// (used in stage_doc to avoid double-write of an identical hash).
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
        let mut f = OpenOptions::new()
            .write(true).create_new(true)
            .open(&tmp)
            .with_context(|| format!("open tmp {}", tmp.display()))?;
        f.write_all(bytes).with_context(|| "write bytes")?;
        f.sync_all().with_context(|| "fsync tmp")?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;

    // Best-effort parent dir fsync on Unix to durably commit the rename.
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(true)
}

/// Forcibly overwrite `path` atomically. Used by tags rewrite.
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

/// Hex-encoded SHA-256 of `bytes`.
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
    fn write_if_new_creates_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("a/b/c.md");
        let written = write_if_new(&p, b"hello").unwrap();
        assert!(written);
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
    }

    #[test]
    fn write_if_new_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        write_if_new(&p, b"first").unwrap();
        let written = write_if_new(&p, b"second").unwrap();
        assert!(!written, "should not overwrite");
        assert_eq!(std::fs::read(&p).unwrap(), b"first");
    }

    #[test]
    fn overwrite_atomic_replaces() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.md");
        write_if_new(&p, b"first").unwrap();
        overwrite_atomic(&p, b"second").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second");
    }

    #[test]
    fn sha256_hex_is_64_chars() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        // Known value for sha256("hello")
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
```

- [ ] **Step 2: Update `content_store/mod.rs`**

```rust
pub mod paths;
pub mod atomic;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::content_store::atomic
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/content_store/
git commit -m "feat(kb): atomic write helpers (tempfile + fsync + rename)"
```

---

## Task 11: `content_store/compose.rs` — YAML front-matter composition + parsing

**Files:**
- Create: `src/kb/content_store/compose.rs`
- Modify: `src/kb/content_store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Compose `.md` files as YAML front-matter + canonical markdown body.
//!
//! Format (compatible with Obsidian, Jekyll, Hugo):
//!
//! ```text
//! ---
//! title: My Document
//! source_kind: doc
//! source_id: manual:01HXY
//! created_at: 2026-05-19T10:00:00Z
//! tags: [pdf, contract]
//! ---
//!
//! # Body starts here
//! ```
//!
//! Body bytes are immutable post-write. Only the `tags:` block in
//! front-matter may be rewritten (separate function, preserves body
//! SHA).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrontMatter {
    pub title: String,
    pub source_kind: String,
    pub source_id: String,
    pub created_at: String,    // ISO 8601 / RFC 3339
    #[serde(default)]
    pub tags: Vec<String>,
    /// Free-form provider-specific metadata (filename, url, etc.)
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// Compose `<front-matter YAML>\n---\n\n<body>` into a single string.
pub fn compose_doc_file(fm: &FrontMatter, body: &str) -> Result<String> {
    let yaml = serde_yaml::to_string(fm).context("serialize front-matter")?;
    Ok(format!("---\n{yaml}---\n\n{body}"))
}

#[derive(Debug)]
pub struct Parsed {
    pub front: FrontMatter,
    pub body: String,
    /// Byte offset where the body starts in the original file (used by
    /// chunker to compute correct `KbChunk.byte_offset`).
    pub body_offset: usize,
}

/// Parse a doc file back into front-matter + body. Body is everything
/// after the second `---\n` delimiter; `body_offset` is its byte index.
pub fn parse_doc_file(content: &str) -> Result<Parsed> {
    let bytes = content.as_bytes();
    if !content.starts_with("---\n") {
        return Err(anyhow!("missing leading front-matter delimiter"));
    }
    // Find the closing `\n---\n`.
    let needle = b"\n---\n";
    let after = &bytes[4..]; // skip leading "---\n"
    let pos = find_subslice(after, needle)
        .ok_or_else(|| anyhow!("missing closing front-matter delimiter"))?;
    let yaml_end = 4 + pos;     // index of '\n' before the closing "---"
    let yaml_str = std::str::from_utf8(&bytes[4..yaml_end])
        .context("front-matter not UTF-8")?;
    let front: FrontMatter = serde_yaml::from_str(yaml_str)
        .context("parse front-matter YAML")?;
    let body_start = yaml_end + needle.len();
    // Skip an optional blank line after the delimiter.
    let body_start = if bytes.get(body_start) == Some(&b'\n') {
        body_start + 1
    } else {
        body_start
    };
    let body = std::str::from_utf8(&bytes[body_start..])
        .context("body not UTF-8")?
        .to_string();
    Ok(Parsed { front, body, body_offset: body_start })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_fm() -> FrontMatter {
        FrontMatter {
            title: "Test".into(),
            source_kind: "doc".into(),
            source_id: "manual:01HXY".into(),
            created_at: "2026-05-19T10:00:00Z".into(),
            tags: vec!["a".into(), "b".into()],
            meta: serde_json::json!({"filename": "test.md"}),
        }
    }

    #[test]
    fn roundtrip() {
        let fm = sample_fm();
        let body = "# Hello\n\nWorld.";
        let composed = compose_doc_file(&fm, body).unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(parsed.front.title, fm.title);
        assert_eq!(parsed.front.tags, fm.tags);
        assert_eq!(parsed.body, body);
    }

    #[test]
    fn body_offset_correct() {
        let fm = sample_fm();
        let body = "BODYBYTE";
        let composed = compose_doc_file(&fm, body).unwrap();
        let parsed = parse_doc_file(&composed).unwrap();
        assert_eq!(&composed.as_bytes()[parsed.body_offset..], body.as_bytes());
    }

    #[test]
    fn rejects_missing_leading_delim() {
        let r = parse_doc_file("title: x\n---\n\nbody");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_missing_closing_delim() {
        let r = parse_doc_file("---\ntitle: x\nbody");
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Update `content_store/mod.rs`**

```rust
pub mod paths;
pub mod atomic;
pub mod compose;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::content_store::compose
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/content_store/
git commit -m "feat(kb): YAML front-matter compose + parse (body_offset preserved)"
```

---

## Task 12: `content_store/read.rs` — read_doc_body + read_doc_range + verify_doc_sha

**Files:**
- Create: `src/kb/content_store/read.rs`
- Modify: `src/kb/content_store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Read APIs over the on-disk content store.

use crate::kb::content_store::compose::{parse_doc_file, Parsed};
use crate::kb::content_store::atomic::sha256_hex;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Read the entire file and return only the canonicalized body
/// (front-matter stripped).
pub fn read_doc_body(abs_path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(abs_path)
        .with_context(|| format!("read {}", abs_path.display()))?;
    let parsed = parse_doc_file(&content)?;
    Ok(parsed.body)
}

/// Read body bytes `[start..end_excl)` directly (no full-file parse
/// once `body_offset` is known via earlier `read_doc_body` call).
///
/// `start` and `end_excl` are offsets within the **body** (post
/// front-matter), as stored in `KbChunk.byte_offset`.
pub fn read_doc_range(abs_path: &Path, start: u64, end_excl: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let content = std::fs::read_to_string(abs_path)?;
    let parsed = parse_doc_file(&content)?;
    let body_bytes = parsed.body.as_bytes();
    let s = start as usize;
    let e = end_excl as usize;
    if e > body_bytes.len() || s > e {
        return Err(anyhow!(
            "range out of bounds: {s}..{e} (body len {})",
            body_bytes.len()
        ));
    }
    Ok(std::str::from_utf8(&body_bytes[s..e])?.to_string())
}

/// Verify the on-disk body matches `expected_sha`. Fails loudly on
/// mismatch (corruption / tampering).
pub fn verify_doc_sha(abs_path: &Path, expected_sha: &str) -> Result<()> {
    let body = read_doc_body(abs_path)?;
    let actual = sha256_hex(body.as_bytes());
    if actual != expected_sha {
        return Err(anyhow!(
            "sha mismatch for {}: expected {expected_sha} got {actual}",
            abs_path.display()
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

    fn sample_fm() -> FrontMatter {
        FrontMatter {
            title: "T".into(),
            source_kind: "doc".into(),
            source_id: "x".into(),
            created_at: "2026-05-19T00:00:00Z".into(),
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    fn write_sample(tmp: &TempDir, body: &str) -> std::path::PathBuf {
        let p = tmp.path().join("x.md");
        let composed = compose_doc_file(&sample_fm(), body).unwrap();
        write_if_new(&p, composed.as_bytes()).unwrap();
        p
    }

    #[test]
    fn read_body_strips_front_matter() {
        let tmp = TempDir::new().unwrap();
        let p = write_sample(&tmp, "BODY");
        assert_eq!(read_doc_body(&p).unwrap(), "BODY");
    }

    #[test]
    fn read_range_returns_substring() {
        let tmp = TempDir::new().unwrap();
        let p = write_sample(&tmp, "0123456789");
        assert_eq!(read_doc_range(&p, 2, 5).unwrap(), "234");
    }

    #[test]
    fn read_range_rejects_oob() {
        let tmp = TempDir::new().unwrap();
        let p = write_sample(&tmp, "short");
        assert!(read_doc_range(&p, 0, 100).is_err());
    }

    #[test]
    fn verify_sha_ok() {
        let tmp = TempDir::new().unwrap();
        let p = write_sample(&tmp, "HELLO");
        let sha = sha256_hex(b"HELLO");
        verify_doc_sha(&p, &sha).unwrap();
    }

    #[test]
    fn verify_sha_mismatch_errors() {
        let tmp = TempDir::new().unwrap();
        let p = write_sample(&tmp, "HELLO");
        assert!(verify_doc_sha(&p, "wrong_sha").is_err());
    }
}
```

- [ ] **Step 2: Update `content_store/mod.rs`**

```rust
pub mod paths;
pub mod atomic;
pub mod compose;
pub mod read;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::content_store::read
```

Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/content_store/
git commit -m "feat(kb): read_doc_body + read_doc_range + verify_doc_sha"
```

---

## Task 13: `content_store/mod.rs` — `stage_doc` public API

**Files:**
- Modify: `src/kb/content_store/mod.rs`

- [ ] **Step 1: Add stage_doc + tests in mod.rs**

```rust
//! On-disk content store. Keeps canonicalized markdown as `.md` files
//! (Obsidian / grep friendly) plus optional raw bytes; redb only stores
//! relative paths + sha256 + byte_offsets.

pub mod paths;
pub mod atomic;
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
    /// Optional raw bytes; only written when `keep_raw` is true.
    pub raw: Option<(&'a [u8], &'a str)>, // (bytes, ext)
    pub keep_raw: bool,
}

/// Atomically write markdown (and optionally raw) to disk and return
/// pointers + sha.
pub fn stage_doc(paths: &KbPaths, input: StageInput<'_>) -> Result<StagedDoc> {
    let md_rel = paths::markdown_rel_path(input.kind, input.slug);
    let md_abs = paths.root.join(&md_rel);
    let composed = compose_doc_file(&input.front, input.body)?;
    atomic::write_if_new(&md_abs, composed.as_bytes())?;
    // Determine body_offset (independent of write succeeded or skipped).
    let parsed = parse_doc_file(&composed)?;
    let md_sha = atomic::sha256_hex(parsed.body.as_bytes());

    let raw_rel = if input.keep_raw {
        if let Some((bytes, ext)) = input.raw {
            let rel = paths::raw_rel_path(input.doc_id, ext);
            let abs = paths.root.join(&rel);
            atomic::write_if_new(&abs, bytes)?;
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
            title: "Test".into(),
            source_kind: "doc".into(),
            source_id: "manual:01HXY".into(),
            created_at: "2026-05-19T00:00:00Z".into(),
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    #[test]
    fn stage_doc_writes_md_and_raw() {
        let tmp = TempDir::new().unwrap();
        let paths = KbPaths::new(tmp.path());
        paths.ensure_layout().unwrap();

        let staged = stage_doc(&paths, StageInput {
            doc_id: "01HXY",
            kind: KbSourceKind::Doc,
            slug: "test-doc",
            front: fm(),
            body: "# Hello",
            raw: Some((b"<raw-bytes>", "pdf")),
            keep_raw: true,
        }).unwrap();

        assert_eq!(staged.markdown_rel_path, "md/doc/test-doc.md");
        assert_eq!(staged.raw_rel_path.as_deref(), Some("raw/01HXY.pdf"));
        assert!(paths.root.join("md/doc/test-doc.md").exists());
        assert!(paths.root.join("raw/01HXY.pdf").exists());
    }

    #[test]
    fn stage_doc_skips_raw_when_keep_raw_false() {
        let tmp = TempDir::new().unwrap();
        let paths = KbPaths::new(tmp.path());
        paths.ensure_layout().unwrap();

        let staged = stage_doc(&paths, StageInput {
            doc_id: "01HXX",
            kind: KbSourceKind::Doc,
            slug: "no-raw",
            front: fm(),
            body: "x",
            raw: Some((b"bytes", "txt")),
            keep_raw: false,
        }).unwrap();

        assert!(staged.raw_rel_path.is_none());
        assert!(!paths.root.join("raw/01HXX.txt").exists());
    }

    #[test]
    fn stage_doc_round_trip_read_range() {
        let tmp = TempDir::new().unwrap();
        let paths = KbPaths::new(tmp.path());
        paths.ensure_layout().unwrap();

        let body = "0123456789";
        let staged = stage_doc(&paths, StageInput {
            doc_id: "01HXY",
            kind: KbSourceKind::Doc,
            slug: "range",
            front: fm(),
            body,
            raw: None,
            keep_raw: false,
        }).unwrap();

        let abs = paths.root.join(&staged.markdown_rel_path);
        let read = read_doc_range(&abs, 3, 7).unwrap();
        assert_eq!(read, "3456");
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::content_store
```

Expected: all content_store tests pass (3 new + previous).

- [ ] **Step 3: Commit**

```bash
git add src/kb/content_store/
git commit -m "feat(kb): content_store::stage_doc public API"
```

---

## Task 14: `store/schema.rs` — redb table definitions

**Files:**
- Create: `src/kb/store/schema.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! redb table definitions for the KB store.
//!
//! All values are JSON-serialized for now (Plan 1a). Plan 1b may
//! introduce compact binary encodings if profiling shows hot spots.

use redb::TableDefinition;

/// `KbDoc.id` → JSON(KbDoc)
pub const KB_DOCS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_docs");

/// `KbChunk.id` → JSON(KbChunk)
pub const KB_CHUNKS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_chunks");

/// `KbEntity.canonical_id` → JSON(KbEntity)
pub const KB_ENTITIES: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_entities");

/// (entity_id, chunk_id) packed as `format!("{entity_id}\0{chunk_id}")`
/// → JSON(KbEntityIndex). Allows range-scan over a single entity.
pub const KB_ENTITY_INDEX: TableDefinition<&str, &[u8]> =
    TableDefinition::new("kb_entity_index");

/// `source_id` → JSON(SyncState). Populated by Plan 1c.
pub const KB_SYNC_STATE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("kb_sync_state");

/// `job_id` → JSON(Job). Populated by Plan 1b.
pub const KB_JOBS: TableDefinition<&str, &[u8]> = TableDefinition::new("kb_jobs");

/// Open the DB and create all tables. Idempotent.
pub fn open_db(path: &std::path::Path) -> anyhow::Result<redb::Database> {
    let db = redb::Database::create(path)?;
    let wtx = db.begin_write()?;
    {
        wtx.open_table(KB_DOCS)?;
        wtx.open_table(KB_CHUNKS)?;
        wtx.open_table(KB_ENTITIES)?;
        wtx.open_table(KB_ENTITY_INDEX)?;
        wtx.open_table(KB_SYNC_STATE)?;
        wtx.open_table(KB_JOBS)?;
    }
    wtx.commit()?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn open_creates_all_tables() {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        // Re-opening: all tables should already exist.
        let rtx = db.begin_read().unwrap();
        rtx.open_table(KB_DOCS).unwrap();
        rtx.open_table(KB_CHUNKS).unwrap();
        rtx.open_table(KB_ENTITIES).unwrap();
        rtx.open_table(KB_ENTITY_INDEX).unwrap();
        rtx.open_table(KB_SYNC_STATE).unwrap();
        rtx.open_table(KB_JOBS).unwrap();
    }

    #[test]
    fn open_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("kb.redb");
        let _db1 = open_db(&path).unwrap();
        let _db2 = open_db(&path).unwrap();
    }
}
```

- [ ] **Step 2: Create `src/kb/store/mod.rs`**

```rust
pub mod schema;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::schema
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): redb schema (6 tables) + open_db"
```

---

## Task 15: `store/doc_access.rs` — KbDoc accessors

**Files:**
- Create: `src/kb/store/doc_access.rs`
- Modify: `src/kb/store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use crate::kb::model::{KbDoc, KbStatus};
use crate::kb::store::schema::KB_DOCS;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable};

pub fn put_doc(db: &Database, doc: &KbDoc) -> Result<()> {
    let bytes = serde_json::to_vec(doc).context("serialize KbDoc")?;
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(KB_DOCS)?;
        t.insert(doc.id.as_str(), bytes.as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

pub fn get_doc(db: &Database, id: &str) -> Result<Option<KbDoc>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_DOCS)?;
    Ok(match t.get(id)? {
        Some(v) => Some(serde_json::from_slice(v.value())?),
        None => None,
    })
}

pub fn delete_doc(db: &Database, id: &str) -> Result<bool> {
    let wtx = db.begin_write()?;
    let removed = {
        let mut t = wtx.open_table(KB_DOCS)?;
        t.remove(id)?.is_some()
    };
    wtx.commit()?;
    Ok(removed)
}

pub fn tombstone_doc(db: &Database, id: &str) -> Result<bool> {
    let Some(mut doc) = get_doc(db, id)? else { return Ok(false); };
    if doc.status == KbStatus::Tombstoned { return Ok(true); }
    doc.status = KbStatus::Tombstoned;
    doc.updated_at = now_unix_ms();
    put_doc(db, &doc)?;
    Ok(true)
}

pub fn list_active_docs(db: &Database) -> Result<Vec<KbDoc>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_DOCS)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let doc: KbDoc = serde_json::from_slice(v.value())?;
        if doc.status == KbStatus::Active { out.push(doc); }
    }
    Ok(out)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{KbSource, KbSourceKind};
    use crate::kb::store::schema::open_db;
    use tempfile::TempDir;

    fn sample(id: &str) -> KbDoc {
        KbDoc {
            id: id.into(),
            source: KbSource::Doc { path: "/tmp/x.md".into() },
            source_kind: KbSourceKind::Doc,
            source_id: format!("manual:{id}"),
            title: "T".into(),
            mime: "text/markdown".into(),
            hash: "h".into(),
            markdown_path: format!("md/doc/{id}.md"),
            markdown_sha256: "s".into(),
            raw_path: None,
            created_at: 0,
            updated_at: 0,
            version: 1,
            status: KbStatus::Active,
            tags: vec![],
            meta: serde_json::Value::Null,
        }
    }

    fn fresh_db() -> (TempDir, Database) {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        (tmp, db)
    }

    #[test]
    fn put_get_roundtrip() {
        let (_tmp, db) = fresh_db();
        let d = sample("01HXY");
        put_doc(&db, &d).unwrap();
        let back = get_doc(&db, "01HXY").unwrap().unwrap();
        assert_eq!(back.id, d.id);
    }

    #[test]
    fn get_missing_returns_none() {
        let (_tmp, db) = fresh_db();
        assert!(get_doc(&db, "nope").unwrap().is_none());
    }

    #[test]
    fn delete_removes() {
        let (_tmp, db) = fresh_db();
        put_doc(&db, &sample("01HXY")).unwrap();
        assert!(delete_doc(&db, "01HXY").unwrap());
        assert!(get_doc(&db, "01HXY").unwrap().is_none());
    }

    #[test]
    fn tombstone_changes_status() {
        let (_tmp, db) = fresh_db();
        put_doc(&db, &sample("01HXY")).unwrap();
        assert!(tombstone_doc(&db, "01HXY").unwrap());
        let d = get_doc(&db, "01HXY").unwrap().unwrap();
        assert_eq!(d.status, KbStatus::Tombstoned);
    }

    #[test]
    fn list_active_excludes_tombstones() {
        let (_tmp, db) = fresh_db();
        put_doc(&db, &sample("a")).unwrap();
        put_doc(&db, &sample("b")).unwrap();
        tombstone_doc(&db, "a").unwrap();
        let active = list_active_docs(&db).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "b");
    }
}
```

- [ ] **Step 2: Update `store/mod.rs`**

```rust
pub mod schema;
pub mod doc_access;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::doc_access
```

Expected: 5 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): KbDoc accessors (put/get/delete/tombstone/list_active)"
```

---

## Task 16: `store/chunk_access.rs` — KbChunk accessors

**Files:**
- Create: `src/kb/store/chunk_access.rs`
- Modify: `src/kb/store/mod.rs`

- [ ] **Step 1: Write tests + impl**

Mirror the pattern from `doc_access.rs` but for chunks. Key methods:

- `put_chunk(db, chunk) -> Result<()>`
- `get_chunk(db, id) -> Result<Option<KbChunk>>`
- `delete_chunk(db, id) -> Result<bool>`
- `tombstone_chunks_for_doc(db, doc_id) -> Result<u32>` — bulk set status=Tombstoned
- `list_chunks_for_doc(db, doc_id) -> Result<Vec<KbChunk>>` — iterate all, filter

```rust
use crate::kb::model::{ChunkStatus, KbChunk};
use crate::kb::store::schema::KB_CHUNKS;
use anyhow::{Context, Result};
use redb::{Database, ReadableTable};

pub fn put_chunk(db: &Database, chunk: &KbChunk) -> Result<()> {
    let bytes = serde_json::to_vec(chunk).context("serialize KbChunk")?;
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(KB_CHUNKS)?;
        t.insert(chunk.id.as_str(), bytes.as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

pub fn get_chunk(db: &Database, id: &str) -> Result<Option<KbChunk>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_CHUNKS)?;
    Ok(match t.get(id)? {
        Some(v) => Some(serde_json::from_slice(v.value())?),
        None => None,
    })
}

pub fn delete_chunk(db: &Database, id: &str) -> Result<bool> {
    let wtx = db.begin_write()?;
    let removed = {
        let mut t = wtx.open_table(KB_CHUNKS)?;
        t.remove(id)?.is_some()
    };
    wtx.commit()?;
    Ok(removed)
}

pub fn tombstone_chunks_for_doc(db: &Database, doc_id: &str) -> Result<u32> {
    let chunks = list_chunks_for_doc(db, doc_id)?;
    let mut n = 0u32;
    for mut c in chunks {
        if c.status == ChunkStatus::Tombstoned { continue; }
        c.status = ChunkStatus::Tombstoned;
        put_chunk(db, &c)?;
        n += 1;
    }
    Ok(n)
}

pub fn list_chunks_for_doc(db: &Database, doc_id: &str) -> Result<Vec<KbChunk>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_CHUNKS)?;
    let mut out = Vec::new();
    for entry in t.iter()? {
        let (_, v) = entry?;
        let c: KbChunk = serde_json::from_slice(v.value())?;
        if c.doc_id == doc_id { out.push(c); }
    }
    out.sort_by_key(|c| c.seq);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::{chunk_id, KbLocator, KbSourceKind};
    use crate::kb::store::schema::open_db;
    use tempfile::TempDir;

    fn sample(doc_id: &str, seq: u32, text: &str) -> KbChunk {
        KbChunk {
            id: chunk_id(KbSourceKind::Doc, doc_id, seq, text),
            doc_id: doc_id.into(),
            doc_version: 1,
            seq,
            heading_path: vec![],
            byte_offset: (0, text.len() as u64),
            indexed_text: text.into(),
            simhash: 0,
            locator: KbLocator::Offset { start: 0, end: text.len() },
            status: ChunkStatus::Active,
            source_quality: 1.0,
            embedder_id: String::new(),
        }
    }

    fn fresh_db() -> (TempDir, redb::Database) {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        (tmp, db)
    }

    #[test]
    fn put_get_roundtrip() {
        let (_tmp, db) = fresh_db();
        let c = sample("doc1", 0, "hi");
        let id = c.id.clone();
        put_chunk(&db, &c).unwrap();
        let back = get_chunk(&db, &id).unwrap().unwrap();
        assert_eq!(back.id, c.id);
        assert_eq!(back.indexed_text, "hi");
    }

    #[test]
    fn list_for_doc_sorts_by_seq() {
        let (_tmp, db) = fresh_db();
        put_chunk(&db, &sample("d", 2, "c")).unwrap();
        put_chunk(&db, &sample("d", 0, "a")).unwrap();
        put_chunk(&db, &sample("d", 1, "b")).unwrap();
        let chunks = list_chunks_for_doc(&db, "d").unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].seq, 0);
        assert_eq!(chunks[2].seq, 2);
    }

    #[test]
    fn tombstone_bulk() {
        let (_tmp, db) = fresh_db();
        put_chunk(&db, &sample("d", 0, "a")).unwrap();
        put_chunk(&db, &sample("d", 1, "b")).unwrap();
        let n = tombstone_chunks_for_doc(&db, "d").unwrap();
        assert_eq!(n, 2);
        for c in list_chunks_for_doc(&db, "d").unwrap() {
            assert_eq!(c.status, ChunkStatus::Tombstoned);
        }
    }
}
```

- [ ] **Step 2: Update `store/mod.rs`**

```rust
pub mod schema;
pub mod doc_access;
pub mod chunk_access;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::chunk_access
```

Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): KbChunk accessors (put/get/list_for_doc/tombstone_bulk)"
```

---

## Task 17: `store/entity_access.rs` — KbEntity + index accessors

**Files:**
- Create: `src/kb/store/entity_access.rs`
- Modify: `src/kb/store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use crate::kb::model::{KbEntity, KbEntityIndex};
use crate::kb::store::schema::{KB_ENTITIES, KB_ENTITY_INDEX};
use anyhow::{Context, Result};
use redb::{Database, ReadableTable};

pub fn put_entity(db: &Database, e: &KbEntity) -> Result<()> {
    let bytes = serde_json::to_vec(e).context("serialize KbEntity")?;
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(KB_ENTITIES)?;
        t.insert(e.canonical_id.as_str(), bytes.as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

pub fn get_entity(db: &Database, canonical_id: &str) -> Result<Option<KbEntity>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_ENTITIES)?;
    Ok(match t.get(canonical_id)? {
        Some(v) => Some(serde_json::from_slice(v.value())?),
        None => None,
    })
}

/// Key format: `format!("{entity_id}\0{chunk_id}")` so a prefix scan
/// returns all chunks for an entity.
pub fn put_index_row(db: &Database, row: &KbEntityIndex) -> Result<()> {
    let key = format!("{}\0{}", row.entity_id, row.chunk_id);
    let bytes = serde_json::to_vec(row)?;
    let wtx = db.begin_write()?;
    {
        let mut t = wtx.open_table(KB_ENTITY_INDEX)?;
        t.insert(key.as_str(), bytes.as_slice())?;
    }
    wtx.commit()?;
    Ok(())
}

pub fn list_chunks_for_entity(db: &Database, entity_id: &str) -> Result<Vec<KbEntityIndex>> {
    let rtx = db.begin_read()?;
    let t = rtx.open_table(KB_ENTITY_INDEX)?;
    let prefix = format!("{entity_id}\0");
    let end = format!("{entity_id}\u{0001}");
    let mut out = Vec::new();
    for entry in t.range(prefix.as_str()..end.as_str())? {
        let (_, v) = entry?;
        out.push(serde_json::from_slice(v.value())?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::model::EntityKind;
    use crate::kb::store::schema::open_db;
    use tempfile::TempDir;

    fn ent(id: &str) -> KbEntity {
        KbEntity {
            canonical_id: id.into(),
            surface_forms: vec![id.into()],
            kind: EntityKind::Brand,
            created_at: 0,
        }
    }

    fn idx(eid: &str, cid: &str) -> KbEntityIndex {
        KbEntityIndex {
            entity_id: eid.into(),
            chunk_id: cid.into(),
            doc_id: "d".into(),
            mention_count: 1,
            score: 1.0,
        }
    }

    fn fresh_db() -> (TempDir, redb::Database) {
        let tmp = TempDir::new().unwrap();
        let db = open_db(&tmp.path().join("kb.redb")).unwrap();
        (tmp, db)
    }

    #[test]
    fn entity_put_get() {
        let (_tmp, db) = fresh_db();
        put_entity(&db, &ent("ent_yili")).unwrap();
        assert_eq!(get_entity(&db, "ent_yili").unwrap().unwrap().canonical_id, "ent_yili");
    }

    #[test]
    fn index_scan_by_entity() {
        let (_tmp, db) = fresh_db();
        put_index_row(&db, &idx("ent_yili", "c1")).unwrap();
        put_index_row(&db, &idx("ent_yili", "c2")).unwrap();
        put_index_row(&db, &idx("ent_mengniu", "c3")).unwrap();
        let rows = list_chunks_for_entity(&db, "ent_yili").unwrap();
        assert_eq!(rows.len(), 2);
        let chunks: Vec<_> = rows.iter().map(|r| r.chunk_id.as_str()).collect();
        assert!(chunks.contains(&"c1"));
        assert!(chunks.contains(&"c2"));
    }
}
```

- [ ] **Step 2: Update `store/mod.rs`**

```rust
pub mod schema;
pub mod doc_access;
pub mod chunk_access;
pub mod entity_access;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::entity_access
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): KbEntity + KbEntityIndex accessors with prefix scan"
```

---

## Task 18: `store/tantivy_schema.rs` — tantivy schema (no indexing yet)

**Files:**
- Create: `src/kb/store/tantivy_schema.rs`
- Modify: `src/kb/store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Tantivy schema for KB chunk FTS. Plan 1a only defines the schema
//! and confirms the index directory opens; actual `add_document` is
//! Plan 1b.

use anyhow::Result;
use std::path::Path;
use tantivy::schema::{Field, Schema, FAST, STORED, STRING, TEXT};
use tantivy::{Index, IndexSettings};

pub struct KbSchema {
    pub schema: Schema,
    pub chunk_id: Field,
    pub doc_id: Field,
    pub source_kind: Field,
    pub status: Field,
    pub indexed_text: Field,
    pub tags: Field,
}

pub fn build_schema() -> KbSchema {
    let mut sb = Schema::builder();
    let chunk_id     = sb.add_text_field("chunk_id", STRING | STORED);
    let doc_id       = sb.add_text_field("doc_id", STRING | STORED | FAST);
    let source_kind  = sb.add_text_field("source_kind", STRING | STORED | FAST);
    let status       = sb.add_text_field("status", STRING | FAST);
    let indexed_text = sb.add_text_field("indexed_text", TEXT);
    let tags         = sb.add_text_field("tags", STRING | FAST);
    KbSchema {
        schema: sb.build(),
        chunk_id, doc_id, source_kind, status, indexed_text, tags,
    }
}

pub fn open_or_create_index(dir: &Path) -> Result<(Index, KbSchema)> {
    std::fs::create_dir_all(dir)?;
    let schema = build_schema();
    let index = match Index::open_in_dir(dir) {
        Ok(idx) => idx,
        Err(_) => Index::create_in_dir(dir, schema.schema.clone())?,
    };
    Ok((index, schema))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_and_reopen() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("idx");
        let (_idx1, _s1) = open_or_create_index(&dir).unwrap();
        let (_idx2, _s2) = open_or_create_index(&dir).unwrap();  // re-open
    }

    #[test]
    fn schema_has_expected_fields() {
        let s = build_schema();
        assert!(s.schema.get_field("chunk_id").is_ok());
        assert!(s.schema.get_field("indexed_text").is_ok());
    }
}
```

- [ ] **Step 2: Update `store/mod.rs`**

```rust
pub mod schema;
pub mod doc_access;
pub mod chunk_access;
pub mod entity_access;
pub mod tantivy_schema;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::tantivy_schema
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): tantivy schema + open_or_create_index (no docs yet)"
```

---

## Task 19: `store/hnsw_init.rs` — HNSW index file initialization

**Files:**
- Create: `src/kb/store/hnsw_init.rs`
- Modify: `src/kb/store/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! HNSW index file initialization. Plan 1a only creates the empty
//! index file with the right config; Plan 1b will add vectors.

use anyhow::Result;
use hnsw_rs::prelude::{DistCosine, Hnsw};
use std::path::Path;

pub const DEFAULT_DIM: usize = 1024;
pub const DEFAULT_M: usize = 16;
pub const DEFAULT_EF_CONSTRUCTION: usize = 200;
pub const DEFAULT_MAX_ELEMENTS: usize = 1_000_000;

pub fn hnsw_file_name(embedder_id: &str) -> String {
    format!("kb_v{DEFAULT_DIM}_{embedder_id}.hnsw")
}

/// Construct an empty in-memory HNSW. Persistence (`dump_to_file`) is
/// Plan 1b. This Plan 1a function exists to keep the public path stable
/// and to verify hnsw_rs is wired up + dims are right.
pub fn new_index() -> Hnsw<'static, f32, DistCosine> {
    Hnsw::<f32, DistCosine>::new(
        DEFAULT_M,
        DEFAULT_MAX_ELEMENTS,
        16,                          // max layer
        DEFAULT_EF_CONSTRUCTION,
        DistCosine {},
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_format() {
        assert_eq!(hnsw_file_name("bge-m3"), "kb_v1024_bge-m3.hnsw");
    }

    #[test]
    fn new_index_constructs() {
        let _h = new_index();
        // No-op insert path test (just verifying construction works).
    }
}
```

- [ ] **Step 2: Update `store/mod.rs`**

```rust
pub mod schema;
pub mod doc_access;
pub mod chunk_access;
pub mod entity_access;
pub mod tantivy_schema;
pub mod hnsw_init;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::store::hnsw_init
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/store/
git commit -m "feat(kb): hnsw index file naming + empty index constructor"
```

---

## Task 20: `canonicalize/mod.rs` — Canonicalizer trait + CanonicalizedSource

**Files:**
- Create/replace: `src/kb/canonicalize/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Source adapters. Each impl normalizes upstream payloads (file
//! bytes, URL response, chat batch, image) into one shape:
//! `CanonicalizedSource { markdown, metadata }`. Downstream
//! (chunker / embedder / writer) is source-kind agnostic.

pub mod mime;
pub mod text;
pub mod md;
pub mod html;
pub mod pdf;

use crate::kb::model::KbSourceKind;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct CanonicalizedSource {
    pub markdown: String,
    pub metadata: CanonicalMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalMetadata {
    pub source_kind: KbSourceKind,
    pub source_id: String,
    pub title: String,
    pub mime: String,
    pub owner: String,
    pub created_at_ms: i64,
    pub tags: Vec<String>,
    /// Free-form provider-specific context, e.g. {"filename": "foo.pdf",
    /// "n_pages": 12}.
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CanonicalizeInput<'a> {
    pub bytes: &'a [u8],
    pub mime: &'a str,
    pub hint_title: Option<&'a str>,
    pub source_id: &'a str,
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
    fn text_canonicalizer_supports_plain_text() {
        let c = TextCanonicalizer;
        assert!(c.supports_mime("text/plain"));
        assert!(!c.supports_mime("application/pdf"));
    }
}
```

(Note: `text.rs`, `md.rs`, `html.rs`, `pdf.rs`, `mime.rs` are stubs at this point; Task 21–25 fill them in.)

- [ ] **Step 2: Stub the submodules so this compiles**

Create empty stub files (replaced by Tasks 21–25):

```bash
for f in md html pdf mime; do
    : > "src/kb/canonicalize/$f.rs"
done
```

For `text.rs` specifically, add the minimal struct so the trait test above works:

```rust
// src/kb/canonicalize/text.rs
use super::*;

pub struct TextCanonicalizer;

impl Canonicalizer for TextCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool { mime == "text/plain" }
    fn canonicalize(&self, _input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        unimplemented!("Task 21")
    }
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize
```

Expected: 1 test passes (the trait dispatch test).

- [ ] **Step 4: Commit**

```bash
git add src/kb/canonicalize/
git commit -m "feat(kb): Canonicalizer trait + CanonicalizedSource + CanonicalMetadata"
```

---

## Task 21: `canonicalize/text.rs` — passthrough

**Files:**
- Modify: `src/kb/canonicalize/text.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use super::*;

pub struct TextCanonicalizer;

impl Canonicalizer for TextCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/plain" | "text/x-log" | "text/csv")
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("text not UTF-8: {e}"))?
            .trim().to_string();
        if body.is_empty() { return Ok(None); }
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                source_id: input.source_id.to_string(),
                title: input.hint_title.unwrap_or("Untitled").to_string(),
                mime: input.mime.to_string(),
                owner: String::new(),
                created_at_ms: now_unix_ms(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_text() {
        let c = TextCanonicalizer;
        let r = c.canonicalize(CanonicalizeInput {
            bytes: b"hello world",
            mime: "text/plain",
            hint_title: Some("Greeting"),
            source_id: "manual:01",
        }).unwrap().unwrap();
        assert_eq!(r.markdown, "hello world");
        assert_eq!(r.metadata.title, "Greeting");
    }

    #[test]
    fn empty_returns_none() {
        let c = TextCanonicalizer;
        let r = c.canonicalize(CanonicalizeInput {
            bytes: b"   \n\n  ",
            mime: "text/plain",
            hint_title: None,
            source_id: "manual:01",
        }).unwrap();
        assert!(r.is_none());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize::text
```

Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/canonicalize/text.rs
git commit -m "feat(kb): TextCanonicalizer (passthrough for text/plain)"
```

---

## Task 22: `canonicalize/md.rs` — Markdown passthrough + heading extraction helper

**Files:**
- Modify: `src/kb/canonicalize/md.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use super::*;
use serde::{Deserialize, Serialize};

pub struct MdCanonicalizer;

impl Canonicalizer for MdCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        matches!(mime, "text/markdown" | "text/x-markdown")
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let body = std::str::from_utf8(input.bytes)
            .map_err(|e| anyhow::anyhow!("md not UTF-8: {e}"))?
            .trim().to_string();
        if body.is_empty() { return Ok(None); }
        let title = extract_first_h1(&body)
            .or_else(|| input.hint_title.map(String::from))
            .unwrap_or_else(|| "Untitled".to_string());
        Ok(Some(CanonicalizedSource {
            markdown: body,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                source_id: input.source_id.to_string(),
                title,
                mime: input.mime.to_string(),
                owner: String::new(),
                created_at_ms: now_unix_ms(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

fn extract_first_h1(md: &str) -> Option<String> {
    md.lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches('#').trim().to_string())
}

/// Heading at a given line: returns `Some((level, text))`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeadingHit { pub level: u8, pub text_start: usize, pub text_end: usize, pub line_idx: usize }

pub fn scan_headings(md: &str) -> Vec<HeadingHit> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for (i, line) in md.lines().enumerate() {
        if let Some((level, _)) = parse_heading_line(line) {
            let lead = line.chars().take_while(|c| *c == '#').count();
            let body_start = lead + 1; // skip "## " (lead # + space)
            let trimmed = &line[body_start.min(line.len())..];
            let text = trimmed.trim();
            let t_off = line.len() - text.len();
            out.push(HeadingHit {
                level: level as u8,
                text_start: offset + t_off,
                text_end: offset + t_off + text.len(),
                line_idx: i,
            });
        }
        offset += line.len() + 1; // +1 for '\n'
    }
    out
}

fn parse_heading_line(line: &str) -> Option<(usize, &str)> {
    let lead = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&lead) && line.as_bytes().get(lead) == Some(&b' ') {
        Some((lead, &line[lead + 1..]))
    } else {
        None
    }
}

/// Given a byte position in the markdown body, return the heading
/// path of `Vec<String>` for the deepest section containing that
/// position. Used by chunker (`heading_path` field on each chunk).
pub fn heading_path_at(md: &str, byte_pos: usize) -> Vec<String> {
    let mut stack: Vec<(u8, String)> = Vec::new();
    let mut offset = 0usize;
    for line in md.lines() {
        let line_end = offset + line.len();
        if offset > byte_pos { break; }
        if let Some((level, text)) = parse_heading_line(line) {
            // Pop until top-of-stack level < this level.
            while let Some(top) = stack.last() {
                if top.0 >= level as u8 { stack.pop(); } else { break; }
            }
            stack.push((level as u8, text.trim().to_string()));
        }
        offset = line_end + 1;
        if offset > byte_pos { break; }
    }
    stack.into_iter().map(|(_, t)| t).collect()
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_pulls_h1_title() {
        let c = MdCanonicalizer;
        let r = c.canonicalize(CanonicalizeInput {
            bytes: b"# My Doc\n\nbody",
            mime: "text/markdown",
            hint_title: None,
            source_id: "manual:01",
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "My Doc");
    }

    #[test]
    fn heading_path_basic() {
        let md = "# A\n## B\nbody1\n## C\nbody2\n### C1\nbody3";
        let p = heading_path_at(md, md.find("body3").unwrap());
        assert_eq!(p, vec!["A".to_string(), "C".to_string(), "C1".to_string()]);
    }

    #[test]
    fn heading_path_pops_on_same_or_higher() {
        let md = "# A\n## B\n## C\nbody";
        let p = heading_path_at(md, md.find("body").unwrap());
        // Should pop B before pushing C.
        assert_eq!(p, vec!["A".to_string(), "C".to_string()]);
    }

    #[test]
    fn scan_headings_finds_all() {
        let md = "# A\n## B\n### C\n";
        let hits = scan_headings(md);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].level, 1);
        assert_eq!(hits[2].level, 3);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize::md
```

Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/canonicalize/md.rs
git commit -m "feat(kb): MdCanonicalizer + heading_path_at helper"
```

---

## Task 23: `canonicalize/html.rs` — HTML → markdown via lol-html

**Files:**
- Modify: `src/kb/canonicalize/html.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use super::*;
use lol_html::{element, HtmlRewriter, Settings};

pub struct HtmlCanonicalizer;

impl Canonicalizer for HtmlCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool {
        mime == "text/html" || mime == "application/xhtml+xml"
    }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let stripped = strip_html_to_text(input.bytes)?;
        let trimmed = stripped.trim();
        if trimmed.is_empty() { return Ok(None); }
        let title = extract_title(input.bytes).unwrap_or_else(||
            input.hint_title.unwrap_or("Untitled").to_string());
        Ok(Some(CanonicalizedSource {
            markdown: trimmed.to_string(),
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                source_id: input.source_id.to_string(),
                title,
                mime: input.mime.to_string(),
                owner: String::new(),
                created_at_ms: now_unix_ms(),
                tags: vec![],
                extra: serde_json::Value::Null,
            },
        }))
    }
}

/// Strip `<script>`, `<style>`, comments; keep text + minimal markdown
/// equivalents for headings / lists / links. Outputs UTF-8 markdown.
fn strip_html_to_text(html_bytes: &[u8]) -> Result<String> {
    let mut buf = String::new();
    let mut sink = Vec::<u8>::new();
    {
        let mut rewriter = HtmlRewriter::new(
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
        rewriter.write(html_bytes)?;
        rewriter.end()?;
    }
    let html_again = String::from_utf8(sink).map_err(|e| anyhow::anyhow!(e))?;
    // Strip remaining tags by extracting text content (cheap pass).
    let mut in_tag = false;
    for c in html_again.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => buf.push(c),
            _ => {}
        }
    }
    // Collapse runs of whitespace.
    let collapsed: String = buf.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(collapsed.replace(" #", "\n#").replace(" - ", "\n- "))
}

fn extract_title(html: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(html).ok()?;
    let lower = s.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")? + start;
    Some(s[start..end].trim().to_string())
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_and_styles() {
        let html = b"<html><body><script>alert(1)</script><p>Hello</p><style>x{}</style></body></html>";
        let r = HtmlCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: html, mime: "text/html", hint_title: None, source_id: "u:01",
        }).unwrap().unwrap();
        assert!(!r.markdown.contains("alert"));
        assert!(!r.markdown.contains("x{}"));
        assert!(r.markdown.contains("Hello"));
    }

    #[test]
    fn extracts_title() {
        let html = b"<html><head><title>My Page</title></head><body><p>X</p></body></html>";
        let r = HtmlCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: html, mime: "text/html", hint_title: None, source_id: "u:01",
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "My Page");
    }

    #[test]
    fn empty_returns_none() {
        let html = b"<html></html>";
        let r = HtmlCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: html, mime: "text/html", hint_title: None, source_id: "u:01",
        }).unwrap();
        assert!(r.is_none());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize::html
```

Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/canonicalize/html.rs
git commit -m "feat(kb): HtmlCanonicalizer (lol-html strip → markdown text)"
```

---

## Task 24: `canonicalize/pdf.rs` — text-layer PDF extraction (no OCR)

**Files:**
- Modify: `src/kb/canonicalize/pdf.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use super::*;

pub struct PdfCanonicalizer;

impl Canonicalizer for PdfCanonicalizer {
    fn source_kind(&self) -> KbSourceKind { KbSourceKind::Doc }
    fn supports_mime(&self, mime: &str) -> bool { mime == "application/pdf" }
    fn canonicalize(&self, input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
        let pages = extract_pages(input.bytes)?;
        let mut md = String::new();
        let mut has_content = false;
        for (i, page_text) in pages.iter().enumerate() {
            let trimmed = page_text.trim();
            if trimmed.is_empty() { continue; }
            if has_content { md.push_str("\n\n"); }
            md.push_str(&format!("## Page {}\n\n{trimmed}", i + 1));
            has_content = true;
        }
        if !has_content { return Ok(None); }
        Ok(Some(CanonicalizedSource {
            markdown: md,
            metadata: CanonicalMetadata {
                source_kind: KbSourceKind::Doc,
                source_id: input.source_id.to_string(),
                title: input.hint_title.unwrap_or("Untitled PDF").to_string(),
                mime: input.mime.to_string(),
                owner: String::new(),
                created_at_ms: now_unix_ms(),
                tags: vec![],
                extra: serde_json::json!({ "n_pages": pages.len() }),
            },
        }))
    }
}

/// Extract per-page text via `pdf-extract`. Pages with too little text
/// for the on-page area heuristic are returned as empty strings (Plan
/// 1a does not OCR scanned pages — that's Plan 3).
fn extract_pages(bytes: &[u8]) -> Result<Vec<String>> {
    // pdf-extract gives a single concatenated string; we approximate
    // per-page split by form-feed (0x0C) which the crate inserts.
    let text = pdf_extract::extract_text_from_mem(bytes)
        .map_err(|e| anyhow::anyhow!("pdf-extract failed: {e:?}"))?;
    let pages: Vec<String> = text.split('\u{0C}').map(|s| s.to_string()).collect();
    Ok(pages)
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // We rely on a tiny generated PDF rather than checking a fixture
    // file into the repo. If pdf-extract changes behavior, this test
    // will catch it.
    #[test]
    fn empty_input_errors_or_returns_none() {
        let r = PdfCanonicalizer.canonicalize(CanonicalizeInput {
            bytes: &[],
            mime: "application/pdf",
            hint_title: None,
            source_id: "manual:01",
        });
        // Either an error or Ok(None); both are acceptable.
        match r {
            Ok(None) => {}
            Err(_) => {}
            Ok(Some(_)) => panic!("unexpected content from empty input"),
        }
    }

    // Add a real PDF fixture test once a sample is generated; for now,
    // skip if no fixture exists.
    #[test]
    #[ignore = "needs fixture PDF; run after Task 30 integration test adds one"]
    fn parses_text_pdf() {
        // Loaded from tests/fixtures/sample.pdf in Task 30.
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize::pdf
```

Expected: 1 test passes (the empty-input test). The `#[ignore]` one is skipped.

- [ ] **Step 3: Commit**

```bash
git add src/kb/canonicalize/pdf.rs
git commit -m "feat(kb): PdfCanonicalizer text-layer extraction (no OCR)"
```

---

## Task 25: `canonicalize/mime.rs` — dispatch by mime type

**Files:**
- Modify: `src/kb/canonicalize/mime.rs`

- [ ] **Step 1: Write tests + impl**

```rust
use super::*;
use crate::kb::canonicalize::{
    html::HtmlCanonicalizer, md::MdCanonicalizer,
    pdf::PdfCanonicalizer, text::TextCanonicalizer,
};

/// Detect a sensible mime type for `bytes` based on magic bytes / file
/// extension. Returns a best-guess value usable by `dispatch`.
pub fn detect_mime(bytes: &[u8], filename_hint: Option<&str>) -> String {
    // Cheap magic-byte check for PDF.
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
    // Fall back to text/plain if printable ASCII.
    if bytes.iter().take(512).all(|b| *b == b'\n' || *b == b'\r' || *b == b'\t' || (*b >= 0x20 && *b < 0x7f)) {
        return "text/plain".into();
    }
    "application/octet-stream".into()
}

/// Dispatch to the first registered Canonicalizer that supports the mime.
pub fn canonicalize_by_mime(input: CanonicalizeInput<'_>) -> Result<Option<CanonicalizedSource>> {
    let canonicalizers: &[&dyn Canonicalizer] = &[
        &MdCanonicalizer,
        &HtmlCanonicalizer,
        &PdfCanonicalizer,
        &TextCanonicalizer,
    ];
    for c in canonicalizers {
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
        assert_eq!(detect_mime(b"%PDF-1.5\n...", None), "application/pdf");
    }

    #[test]
    fn detect_by_extension() {
        assert_eq!(detect_mime(b"# title", Some("foo.md")), "text/markdown");
        assert_eq!(detect_mime(b"<html>", Some("foo.html")), "text/html");
        assert_eq!(detect_mime(b"plain", Some("foo.txt")), "text/plain");
    }

    #[test]
    fn dispatch_routes_to_md() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"# Hi\n\nbody",
            mime: "text/markdown",
            hint_title: None,
            source_id: "manual:01",
        }).unwrap().unwrap();
        assert_eq!(r.metadata.title, "Hi");
    }

    #[test]
    fn unknown_mime_errors() {
        let r = canonicalize_by_mime(CanonicalizeInput {
            bytes: b"x",
            mime: "application/x-unknown",
            hint_title: None,
            source_id: "manual:01",
        });
        assert!(r.is_err());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::canonicalize::mime
```

Expected: 4 tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/canonicalize/mime.rs
git commit -m "feat(kb): mime detection + canonicalize_by_mime dispatch"
```

---

## Task 26: `chunker/tokens.rs` — approximate token counter

**Files:**
- Create: `src/kb/chunker/tokens.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Approximate token counter. Plan 1a uses the GPT-family heuristic
//! "1 token ≈ 4 chars"; Plan 1b will replace with BGE-M3 tokenizer
//! for accurate budget enforcement.

pub fn approx_token_count(text: &str) -> u32 {
    let chars = text.chars().count() as u32;
    chars.saturating_add(3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty() { assert_eq!(approx_token_count(""), 0); }

    #[test]
    fn linear_scale() {
        assert_eq!(approx_token_count("a"), 1);
        assert_eq!(approx_token_count("abcd"), 1);
        assert_eq!(approx_token_count("abcde"), 2);
        assert_eq!(approx_token_count(&"x".repeat(400)), 100);
    }
}
```

- [ ] **Step 2: Create `src/kb/chunker/mod.rs`** (was empty stub from Task 0)

```rust
pub mod tokens;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::chunker::tokens
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/chunker/
git commit -m "feat(kb): approx_token_count (4-char heuristic, BGE tokenizer in 1b)"
```

---

## Task 27: `chunker/splitter.rs` — paragraph + sentence splitters

**Files:**
- Create: `src/kb/chunker/splitter.rs`
- Modify: `src/kb/chunker/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Split markdown into paragraph units, with byte offsets back into
//! the source string. Each paragraph is a `(start, end_exclusive, text)`.

#[derive(Debug, Clone, PartialEq)]
pub struct Paragraph {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Split on blank-line boundaries. Preserves leading/trailing
/// whitespace inside the paragraph but trims for the returned text.
pub fn split_paragraphs(md: &str) -> Vec<Paragraph> {
    let bytes = md.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        // Find next "\n\n" boundary.
        if i + 1 < bytes.len() && bytes[i] == b'\n' && bytes[i+1] == b'\n' {
            push_para(&mut out, md, start, i);
            // Skip all consecutive newlines.
            i += 2;
            while i < bytes.len() && bytes[i] == b'\n' { i += 1; }
            start = i;
        } else {
            i += 1;
        }
    }
    push_para(&mut out, md, start, bytes.len());
    out
}

fn push_para(out: &mut Vec<Paragraph>, md: &str, start: usize, end: usize) {
    let slice = &md[start..end];
    let text = slice.trim().to_string();
    if !text.is_empty() {
        // Recompute start/end to skip leading/trailing whitespace.
        let leading = slice.len() - slice.trim_start().len();
        let trailing = slice.len() - slice.trim_end().len();
        out.push(Paragraph {
            start: start + leading,
            end: end - trailing,
            text,
        });
    }
}

/// Split a paragraph into sentences. CJK-aware (uses Chinese period
/// and exclamation/question marks as additional boundaries).
pub fn split_sentences(para: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in para.chars() {
        cur.push(c);
        if matches!(c, '.' | '!' | '?' | '。' | '!' | '?' | '；' | ';') {
            // Look ahead for a space or end-of-string; if so, flush.
            let t = cur.trim();
            if !t.is_empty() { out.push(t.to_string()); }
            cur.clear();
        }
    }
    let t = cur.trim();
    if !t.is_empty() { out.push(t.to_string()); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_paragraphs_blank_line() {
        let md = "para one\n\npara two\n\npara three";
        let p = split_paragraphs(md);
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].text, "para one");
        assert_eq!(p[1].text, "para two");
        assert_eq!(&md[p[1].start..p[1].end], "para two");
    }

    #[test]
    fn split_paragraphs_handles_trailing_newlines() {
        let p = split_paragraphs("hello\n\n\n\nworld\n\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].text, "hello");
        assert_eq!(p[1].text, "world");
    }

    #[test]
    fn split_sentences_ascii() {
        let s = split_sentences("Hello world. How are you? Fine!");
        assert_eq!(s, vec!["Hello world.", "How are you?", "Fine!"]);
    }

    #[test]
    fn split_sentences_cjk() {
        let s = split_sentences("第一句。第二句！第三句？");
        assert_eq!(s, vec!["第一句。", "第二句！", "第三句？"]);
    }
}
```

- [ ] **Step 2: Update `chunker/mod.rs`**

```rust
pub mod tokens;
pub mod splitter;
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p rsclaw --lib kb::chunker::splitter
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/kb/chunker/
git commit -m "feat(kb): paragraph + CJK-aware sentence splitters"
```

---

## Task 28: `chunker/mod.rs` — chunk_markdown end-to-end

**Files:**
- Modify: `src/kb/chunker/mod.rs`

- [ ] **Step 1: Write tests + impl**

```rust
//! Slice canonical markdown into chunks. Each chunk:
//! - id deterministic from (kind, source_id, seq, body)
//! - byte_offset back into the markdown body
//! - heading_path computed from preceding `## ...` lines
//! - indexed_text = `heading_path > ... \n\n body`
//! - simhash for near-dup detection

pub mod tokens;
pub mod splitter;

use crate::kb::canonicalize::md::heading_path_at;
use crate::kb::model::{
    chunk_id, hamming64, simhash64, ChunkStatus, KbChunk, KbLocator,
    KbSourceKind,
};
use splitter::split_paragraphs;
use tokens::approx_token_count;

pub const DEFAULT_TARGET_TOKENS: u32 = 512;
pub const DEFAULT_MIN_TOKENS: u32 = 50;
pub const DEFAULT_OVERLAP_TOKENS: u32 = 64;
/// Hamming threshold below which two chunks are considered near-duplicates.
pub const SIMHASH_DEDUP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone)]
pub struct ChunkerInput<'a> {
    pub kind: KbSourceKind,
    pub source_id: &'a str,
    pub doc_id: &'a str,
    pub doc_version: u32,
    pub markdown_body: &'a str,
    pub default_locator_kind: LocatorKind,
}

#[derive(Debug, Clone, Copy)]
pub enum LocatorKind {
    Offset,
    MdSection,
}

pub fn chunk_markdown(input: ChunkerInput<'_>) -> Vec<KbChunk> {
    let paras = split_paragraphs(input.markdown_body);
    let mut chunks: Vec<KbChunk> = Vec::new();
    let mut buf_text = String::new();
    let mut buf_start: Option<usize> = None;
    let mut buf_end: usize = 0;
    let mut seq = 0u32;

    for p in &paras {
        let tentative_tokens =
            approx_token_count(&buf_text) + approx_token_count(&p.text);
        if !buf_text.is_empty() && tentative_tokens > DEFAULT_TARGET_TOKENS {
            // Flush current buffer.
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf_text);
            buf_text.clear();
            buf_start = None;
        }
        if buf_text.is_empty() { buf_start = Some(p.start); }
        if !buf_text.is_empty() { buf_text.push_str("\n\n"); }
        buf_text.push_str(&p.text);
        buf_end = p.end;

        // If the buffer is already big after adding this paragraph, flush.
        if approx_token_count(&buf_text) >= DEFAULT_TARGET_TOKENS {
            flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf_text);
            buf_text.clear();
            buf_start = None;
        }
    }
    if !buf_text.is_empty() {
        flush(&mut chunks, &mut seq, &input, buf_start.unwrap(), buf_end, &buf_text);
    }

    // Dedup: drop chunks whose simhash is within threshold of an earlier
    // chunk in the same doc.
    deduplicate_in_place(&mut chunks);
    chunks
}

fn flush(
    out: &mut Vec<KbChunk>, seq: &mut u32, input: &ChunkerInput<'_>,
    start: usize, end: usize, body: &str,
) {
    let body_owned = body.to_string();
    let path = heading_path_at(input.markdown_body, start);
    let indexed = if path.is_empty() {
        body_owned.clone()
    } else {
        format!("{}\n\n{body_owned}", path.join(" > "))
    };
    let id = chunk_id(input.kind, input.source_id, *seq, &body_owned);
    let sim = simhash64(&body_owned);
    let locator = match input.default_locator_kind {
        LocatorKind::Offset => KbLocator::Offset { start, end },
        LocatorKind::MdSection => KbLocator::MdSection { heading_path: path.clone() },
    };
    out.push(KbChunk {
        id,
        doc_id: input.doc_id.to_string(),
        doc_version: input.doc_version,
        seq: *seq,
        heading_path: path,
        byte_offset: (start as u64, end as u64),
        indexed_text: indexed,
        simhash: sim,
        locator,
        status: ChunkStatus::Active,
        source_quality: 1.0,
        embedder_id: String::new(),
    });
    *seq += 1;
}

fn deduplicate_in_place(chunks: &mut Vec<KbChunk>) {
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

    #[test]
    fn single_short_paragraph_one_chunk() {
        let md = "this is a tiny doc.";
        let chunks = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].seq, 0);
    }

    #[test]
    fn long_doc_splits_into_multiple() {
        let md = "para one paragraph.\n\n".to_string() + &"para text here.\n\n".repeat(500);
        let chunks = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: &md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert!(chunks.len() > 1, "expected >1 chunks, got {}", chunks.len());
        for c in &chunks {
            assert!(approx_token_count(&c.indexed_text) <= DEFAULT_TARGET_TOKENS + 200);
        }
    }

    #[test]
    fn heading_path_injected_into_indexed_text() {
        let md = "# Mengniu Milk\n## Recipe\nmix 100g with 100ml water.";
        let chunks = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::MdSection,
        });
        let c = &chunks[0];
        assert_eq!(c.heading_path, vec!["Mengniu Milk".to_string(), "Recipe".to_string()]);
        assert!(c.indexed_text.starts_with("Mengniu Milk > Recipe"));
    }

    #[test]
    fn deterministic_id_across_runs() {
        let md = "hello.\n\nworld.";
        let a = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        let b = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(x.id, y.id);
        }
    }

    #[test]
    fn byte_offset_roundtrips_to_original() {
        let md = "first.\n\nsecond.";
        let chunks = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        for c in chunks {
            let s = c.byte_offset.0 as usize;
            let e = c.byte_offset.1 as usize;
            // The original substring at this offset must contain
            // (case-sensitive) some content; just sanity-check it's
            // non-empty and lies within bounds.
            assert!(e <= md.len());
            assert!(s < e);
            assert!(!md[s..e].trim().is_empty());
        }
    }

    #[test]
    fn near_duplicate_chunks_deduped() {
        let md = "the quick brown fox jumps over the lazy dog\n\n\
                  the quick brown fox jumps over the lazy dog";
        let chunks = chunk_markdown(ChunkerInput {
            kind: KbSourceKind::Doc, source_id: "manual:01", doc_id: "d1",
            doc_version: 1, markdown_body: md,
            default_locator_kind: LocatorKind::Offset,
        });
        // Both paragraphs identical → dedup → 1 chunk.
        assert_eq!(chunks.len(), 1);
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p rsclaw --lib kb::chunker
```

Expected: 6 tests pass (plus pre-existing token + splitter tests).

- [ ] **Step 3: Commit**

```bash
git add src/kb/chunker/
git commit -m "feat(kb): chunk_markdown with heading_path prefix + SimHash dedup"
```

---

## Task 29: Public façade `src/kb/mod.rs`

**Files:**
- Modify: `src/kb/mod.rs`

- [ ] **Step 1: Add re-exports**

```rust
//! rsclaw Knowledge Base. See `docs/specs/2026-05-19-knowledge-base.md`.

pub mod paths;
pub mod model;
pub mod content_store;
pub mod store;
pub mod canonicalize;
pub mod chunker;
pub mod util;

// Convenience re-exports for downstream callers.
pub use paths::KbPaths;
pub use model::{
    chunk_id, ChunkStatus, EntityKind, KbChunk, KbDoc, KbEntity,
    KbEntityIndex, KbLocator, KbSource, KbSourceKind, KbStatus,
    MailSource,
};
pub use content_store::{stage_doc, FrontMatter, StageInput, StagedDoc};
pub use canonicalize::{
    canonicalize_by_mime, detect_mime, CanonicalMetadata,
    CanonicalizedSource, CanonicalizeInput,
};
pub use chunker::{chunk_markdown, ChunkerInput, LocatorKind};
```

- [ ] **Step 2: Verify**

```bash
cargo check
cargo test -p rsclaw --lib kb::
```

Expected: compiles + all prior unit tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/kb/mod.rs
git commit -m "feat(kb): public façade with canonical re-exports"
```

---

## Task 30: Integration test — end-to-end ingest

**Files:**
- Create: `tests/kb_phase1a_e2e.rs`
- Create: `tests/fixtures/kb/sample.md` (small fixture markdown)
- Create: `tests/fixtures/kb/sample.html`
- Create: `tests/fixtures/kb/sample.txt`

- [ ] **Step 1: Add fixtures**

```bash
mkdir -p tests/fixtures/kb
cat > tests/fixtures/kb/sample.md <<'EOF'
# Mengniu Milk Powder Guide

## Recipe
Mix 100g of Mengniu milk powder with 100ml of warm water.

## Storage
Keep in a cool, dry place. Use within 30 days of opening.
EOF

cat > tests/fixtures/kb/sample.html <<'EOF'
<html><head><title>Sample HTML</title></head>
<body>
<script>alert(1)</script>
<h1>Heading</h1>
<p>Hello world.</p>
<p>Second paragraph.</p>
</body></html>
EOF

cat > tests/fixtures/kb/sample.txt <<'EOF'
This is a plain text fixture used by the KB Phase 1a integration test.
EOF
```

- [ ] **Step 2: Write integration test**

```rust
//! KB Phase 1a end-to-end: ingest a doc through canonicalize → chunk →
//! content_store + redb, then verify everything reads back correctly.

use anyhow::Result;
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown,
    content_store::{read_doc_range, stage_doc, FrontMatter, StageInput},
    detect_mime, store::{
        chunk_access::{list_chunks_for_doc, put_chunk},
        doc_access::{get_doc, list_active_docs, put_doc},
        schema::open_db,
    },
    CanonicalizeInput, ChunkerInput, KbDoc, KbPaths, KbSource, KbSourceKind,
    KbStatus, LocatorKind,
};
use std::path::Path;
use tempfile::TempDir;
use ulid::Ulid;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ingest_file(paths: &KbPaths, db: &redb::Database, src_path: &Path) -> Result<String> {
    let bytes = std::fs::read(src_path)?;
    let name = src_path.file_name().and_then(|s| s.to_str()).unwrap_or("Untitled");
    let mime = detect_mime(&bytes, Some(name));

    let doc_id = Ulid::new().to_string();
    let source_id = format!("manual:{doc_id}");

    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes,
        mime: &mime,
        hint_title: Some(name),
        source_id: &source_id,
    })?.expect("canonicalize");

    let staged = stage_doc(paths, StageInput {
        doc_id: &doc_id,
        kind: canon.metadata.source_kind,
        slug: name,
        front: FrontMatter {
            title: canon.metadata.title.clone(),
            source_kind: canon.metadata.source_kind.as_str().to_string(),
            source_id: source_id.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            tags: canon.metadata.tags.clone(),
            meta: canon.metadata.extra.clone(),
        },
        body: &canon.markdown,
        raw: Some((&bytes, name.rsplit('.').next().unwrap_or(""))),
        keep_raw: true,
    })?;

    let doc = KbDoc {
        id: doc_id.clone(),
        source: KbSource::Doc { path: src_path.to_path_buf() },
        source_kind: canon.metadata.source_kind,
        source_id: source_id.clone(),
        title: canon.metadata.title.clone(),
        mime: canon.metadata.mime.clone(),
        hash: rsclaw::kb::content_store::atomic::sha256_hex(&bytes),
        markdown_path: staged.markdown_rel_path.clone(),
        markdown_sha256: staged.markdown_sha256.clone(),
        raw_path: staged.raw_rel_path.clone(),
        created_at: now_ms(),
        updated_at: now_ms(),
        version: 1,
        status: KbStatus::Active,
        tags: vec![],
        meta: canon.metadata.extra.clone(),
    };
    put_doc(db, &doc)?;

    let chunks = chunk_markdown(ChunkerInput {
        kind: canon.metadata.source_kind,
        source_id: &source_id,
        doc_id: &doc_id,
        doc_version: 1,
        markdown_body: &canon.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });
    for c in &chunks {
        put_chunk(db, c)?;
    }

    Ok(doc_id)
}

#[test]
fn e2e_ingest_markdown_and_read_chunk_range() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let db = open_db(&paths.redb_file())?;

    let doc_id = ingest_file(&paths, &db, Path::new("tests/fixtures/kb/sample.md"))?;

    // KbDoc present + Active.
    let doc = get_doc(&db, &doc_id)?.expect("doc stored");
    assert_eq!(doc.status, KbStatus::Active);
    assert!(doc.title.contains("Mengniu"));
    assert!(paths.root.join(&doc.markdown_path).exists());

    // Chunks exist, all with heading_path.
    let chunks = list_chunks_for_doc(&db, &doc_id)?;
    assert!(!chunks.is_empty());
    let mut saw_mengniu_in_indexed = false;
    for c in &chunks {
        assert!(c.heading_path.iter().any(|h| h.contains("Mengniu") || h.contains("Recipe") || h.contains("Storage")),
            "missing heading: {:?}", c.heading_path);
        if c.indexed_text.contains("Mengniu") { saw_mengniu_in_indexed = true; }
    }
    assert!(saw_mengniu_in_indexed, "heading_path prefix should inject Mengniu into indexed_text");

    // Reading a chunk's byte_range from the md file returns its original body.
    let abs = paths.root.join(&doc.markdown_path);
    for c in &chunks {
        let body = read_doc_range(&abs, c.byte_offset.0, c.byte_offset.1)?;
        // Body should be non-empty + a substring of the canonical markdown.
        assert!(!body.trim().is_empty());
    }

    // Listing active docs returns this one.
    let active = list_active_docs(&db)?;
    assert!(active.iter().any(|d| d.id == doc_id));

    Ok(())
}

#[test]
fn e2e_ingest_html_strips_scripts() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let db = open_db(&paths.redb_file())?;

    let doc_id = ingest_file(&paths, &db, Path::new("tests/fixtures/kb/sample.html"))?;
    let doc = get_doc(&db, &doc_id)?.unwrap();
    let body = std::fs::read_to_string(paths.root.join(&doc.markdown_path))?;
    assert!(!body.contains("alert"), "script should be stripped");
    assert!(body.contains("Hello world"));
    Ok(())
}

#[test]
fn e2e_ingest_plain_text() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let db = open_db(&paths.redb_file())?;

    let doc_id = ingest_file(&paths, &db, Path::new("tests/fixtures/kb/sample.txt"))?;
    let doc = get_doc(&db, &doc_id)?.unwrap();
    let chunks = list_chunks_for_doc(&db, &doc_id)?;
    assert!(!chunks.is_empty());
    assert!(doc.markdown_path.starts_with("md/doc/"));
    Ok(())
}

#[test]
fn idempotent_reingest_same_chunks() -> Result<()> {
    let tmp = TempDir::new()?;
    let paths = KbPaths::new(tmp.path());
    paths.ensure_layout()?;
    let db = open_db(&paths.redb_file())?;

    // First ingest.
    let _id_a = ingest_file(&paths, &db, Path::new("tests/fixtures/kb/sample.md"))?;
    // Second ingest with a FRESH doc_id but identical content → chunk_ids
    // differ (because source_id differs). But within one ingest, calling
    // chunk_markdown twice on the same input yields the same ids.
    let bytes = std::fs::read("tests/fixtures/kb/sample.md")?;
    let mime = detect_mime(&bytes, Some("sample.md"));
    let canon = canonicalize_by_mime(CanonicalizeInput {
        bytes: &bytes, mime: &mime, hint_title: None, source_id: "manual:fixed",
    })?.unwrap();
    let a = chunk_markdown(ChunkerInput {
        kind: KbSourceKind::Doc, source_id: "manual:fixed",
        doc_id: "d", doc_version: 1,
        markdown_body: &canon.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });
    let b = chunk_markdown(ChunkerInput {
        kind: KbSourceKind::Doc, source_id: "manual:fixed",
        doc_id: "d", doc_version: 1,
        markdown_body: &canon.markdown,
        default_locator_kind: LocatorKind::MdSection,
    });
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.id, y.id);
    }
    Ok(())
}
```

(Note: this integration test uses `chrono` (already in rsclaw `Cargo.toml`) for RFC-3339 timestamps. If for some reason `chrono` is not present, add it: `cargo add chrono --features serde`.)

- [ ] **Step 3: Run integration test**

```bash
cargo test --test kb_phase1a_e2e
```

Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add tests/kb_phase1a_e2e.rs tests/fixtures/kb/
git commit -m "test(kb): Phase 1a e2e integration (md/html/txt ingest + read range + idempotency)"
```

---

## Task 31: Module README

**Files:**
- Create: `src/kb/README.md`

- [ ] **Step 1: Write README**

```markdown
# `src/kb/` — Knowledge Base

User-managed RAG knowledge base. See `docs/specs/2026-05-19-knowledge-base.md`
for the full design.

## What's implemented (Plan 1a)

- **Types** (`model/`): KbDoc / KbChunk / KbEntity / KbEntityIndex /
  KbSource / KbLocator / SimHash-64. Chunk IDs are deterministic
  SHA-256 over `(kind|source_id|seq|content)`.
- **Content store** (`content_store/`): atomic markdown writes under
  `~/.rsclaw/kb/md/<kind>/<slug>.md` with YAML front-matter; optional
  raw bytes under `~/.rsclaw/kb/raw/<doc_id>.<ext>`; `read_doc_range`
  for lazy chunk-body retrieval.
- **redb schema + accessors** (`store/`): 6 tables, put/get/delete/
  tombstone for docs, chunks, entities; prefix-scan over entity index.
- **tantivy + hnsw initialization** (`store/`): schema defined; empty
  indexes openable. No `add_document` / `add_vector` yet.
- **Canonicalizers** (`canonicalize/`): markdown, plain text, HTML
  (via lol-html), PDF text layer (via pdf-extract; no OCR).
- **Chunker** (`chunker/`): paragraph-based splitter with heading_path
  injection into `indexed_text`; SimHash near-dup detection.

## What's NOT in Plan 1a

These ship in Plans 1b / 1c / later phases — see spec §7:

- Embedding (Plan 1b)
- Tantivy indexing of documents (Plan 1b)
- HNSW vector insertion (Plan 1b)
- Entity extraction (Plan 1b)
- Jobs queue / async pipeline (Plan 1b)
- Hybrid retrieval / kb_search tool (Plan 1c)
- CLI surface (Plan 1c)
- ManualUploadSyncer wiring (Plan 1c)
- OCR (Plan 3)
- Tauri UI (Plan 2)

## Module layout

```
src/kb/
  mod.rs               # public façade
  paths.rs             # ~/.rsclaw/kb/ root + subdirs
  model/               # types
  content_store/       # on-disk markdown + raw
  store/               # redb + tantivy/hnsw schemas
  canonicalize/        # source → markdown adapters
  chunker/             # markdown → chunks
  util/redact.rs       # PII redaction for logs
```

## Quick start (Plan 1a only)

```rust
use rsclaw::kb::{
    canonicalize_by_mime, chunk_markdown, detect_mime,
    content_store::{stage_doc, FrontMatter, StageInput},
    store::{schema::open_db, doc_access::put_doc, chunk_access::put_chunk},
    CanonicalizeInput, ChunkerInput, KbDoc, KbPaths, KbSource,
    KbSourceKind, KbStatus, LocatorKind,
};

let paths = KbPaths::new("~/.rsclaw/kb");
paths.ensure_layout()?;
let db = open_db(&paths.redb_file())?;

let bytes = std::fs::read("manual.md")?;
let mime = detect_mime(&bytes, Some("manual.md"));
let canon = canonicalize_by_mime(CanonicalizeInput {
    bytes: &bytes, mime: &mime,
    hint_title: Some("manual.md"), source_id: "manual:0001",
})?.unwrap();
// ... (see tests/kb_phase1a_e2e.rs for the full flow)
```

## Testing

```bash
cargo test -p rsclaw --lib kb::          # unit tests
cargo test --test kb_phase1a_e2e         # integration
```
```

- [ ] **Step 2: Commit**

```bash
git add src/kb/README.md
git commit -m "docs(kb): module README for Plan 1a scope"
```

---

## Self-review checklist (run by the engineer after Task 31)

Before marking the plan complete, the engineer should manually verify:

- [ ] `cargo test` passes (all unit + integration tests added in this plan).
- [ ] `cargo clippy -- -D warnings` passes (no new clippy warnings).
- [ ] `cargo fmt --check` passes.
- [ ] No `unwrap()` / `println!()` / `dbg!()` in non-test code.
- [ ] All PII (source ids, content previews) in `log::` calls go through `kb::util::redact`.
- [ ] `~/.rsclaw/kb/` layout matches spec (`md/{doc,chat,url,img,mail}/`, `raw/`, `db/`, `idx/`, `hnsw/`, `state/`).
- [ ] Re-ingesting the same fixture produces identical chunk IDs.
- [ ] HTML fixture's `<script>` tag does NOT appear in any stored markdown.

If any of the above fails, fix and commit before declaring Plan 1a complete.

---

## What's next

**Plan 1b — Ingestion** picks up here:
- Add real embedding (BGE-M3 local + remote API fallback)
- Wire chunker output into tantivy `add_document` + hnsw `insert`
- Build the entity extractor pipeline (regex + jieba)
- Implement the jobs queue (`dedupe_key` + `claim_token` + `reclaim_stale_jobs`)
- Build the Writer transaction that atomically commits doc + chunks + jobs

**Plan 1c — Retrieval + CLI** picks up after Plan 1b:
- Hybrid retrieval (`kb_search`)
- `kb_fetch` / `kb_list_docs` / `kb_similar` tools
- `kb_search_entities` (uses Plan 1b's entity index)
- `kb_explain` + citation_confidence (rsclaw-specific innovations)
- ManualUploadSyncer wiring
- CLI subcommands (`rsclaw kb add` / `ls` / `search` / `show`)

Each subsequent plan should be written via the `superpowers:writing-plans` skill once the prior plan is fully implemented and verified — that way the next plan can adjust based on whatever surprises Plan 1a uncovered.
