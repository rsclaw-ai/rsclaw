//! tantivy BM25 full-text search index (AGENTS.md S8).
//!
//! Used for:
//!   - Memory keyword search (`rsclaw memory search "<query>"`)
//!   - Skill / plugin discovery
//!   - Session transcript search
//!
//! Memory limits (AGENTS.md S18):
//!   Low tier  -> 15 MB writer heap
//!   Standard  -> 32 MB writer heap
//!   High      -> 64 MB writer heap

use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result};
use rsclaw_platform::MemoryTier;
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument,
    collector::TopDocs,
    doc,
    query::{BooleanQuery, Occur, Query, QueryParser, TermQuery},
    schema::{
        IndexRecordOption, STORED, STRING, Schema, SchemaBuilder, TextFieldIndexing, TextOptions,
        document::Value as _,
    },
};
use tracing::{debug, warn};

use crate::cjk::{CJK_TOKENIZER, JiebaTokenizer, query_terms};

// ---------------------------------------------------------------------------
// Schema field names
// ---------------------------------------------------------------------------
const FIELD_ID: &str = "id";
const FIELD_SCOPE: &str = "scope";
const FIELD_CONTENT: &str = "content";
const FIELD_KIND: &str = "kind";

// ---------------------------------------------------------------------------
// SearchIndex
// ---------------------------------------------------------------------------

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    /// Mutex-wrapped writer enables indexing from shared (Arc) references.
    writer: Mutex<IndexWriter>,
    schema: Schema,
}

impl SearchIndex {
    /// Open (or create) a tantivy index at `path`.
    pub fn open(path: &Path, tier: MemoryTier) -> Result<Self> {
        let writer_heap: usize = match tier {
            MemoryTier::Low => 15_000_000, // 15 MB
            MemoryTier::Standard => 32_000_000,
            MemoryTier::High => 64_000_000,
        };

        let mut builder = SchemaBuilder::new();
        builder.add_text_field(FIELD_ID, STRING | STORED);
        builder.add_text_field(FIELD_SCOPE, STRING | STORED);
        // jieba tokenizer: the default TEXT analyzer turns a CJK sentence
        // into ONE token, so Chinese memories were effectively unsearchable.
        let content_opts = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CJK_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        builder.add_text_field(FIELD_CONTENT, content_opts);
        builder.add_text_field(FIELD_KIND, STRING | STORED);
        let schema = builder.build();

        std::fs::create_dir_all(path)
            .with_context(|| format!("create index dir {}", path.display()))?;

        let open = |path: &Path| -> Result<tantivy::Result<Index>> {
            Ok(Index::open_or_create(
                tantivy::directory::MmapDirectory::open(path)
                    .with_context(|| format!("open mmap dir {}", path.display()))?,
                schema.clone(),
            ))
        };
        let index = match open(path)? {
            Ok(index) => index,
            // The index is a derived cache (memory docs are re-indexed from
            // redb at startup), so a schema change — e.g. the switch to the
            // CJK tokenizer — just recreates it.
            Err(tantivy::TantivyError::SchemaError(msg)) => {
                warn!(path = %path.display(), "search index schema changed ({msg}); recreating");
                std::fs::remove_dir_all(path)
                    .with_context(|| format!("remove stale index {}", path.display()))?;
                std::fs::create_dir_all(path)
                    .with_context(|| format!("create index dir {}", path.display()))?;
                open(path)?
                    .with_context(|| format!("recreate tantivy index at {}", path.display()))?
            }
            Err(e) => {
                return Err(e).with_context(|| format!("open tantivy index at {}", path.display()));
            }
        };
        // Tokenizers are not persisted in the index; register on every open,
        // before any writer tokenizes a document.
        index
            .tokenizers()
            .register(CJK_TOKENIZER, JiebaTokenizer::new());

        let writer = index.writer(writer_heap).context("create index writer")?;

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .context("create index reader")?;

        debug!(path = %path.display(), heap_mb = writer_heap / 1_000_000, "tantivy index opened");
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            schema,
        })
    }

    // -----------------------------------------------------------------------
    // Write
    // -----------------------------------------------------------------------

    /// Index a document. Overwrites any existing document with the same `id`.
    pub fn index_document(&self, doc: &IndexDoc) -> Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("search writer lock poisoned: {e}"))?;

        // Delete existing document with the same ID.
        let id_field = self.schema.get_field(FIELD_ID).expect("field");
        writer.delete_term(tantivy::Term::from_field_text(id_field, &doc.id));

        let scope_field = self.schema.get_field(FIELD_SCOPE).expect("field");
        let content_field = self.schema.get_field(FIELD_CONTENT).expect("field");
        let kind_field = self.schema.get_field(FIELD_KIND).expect("field");

        writer.add_document(doc!(
            id_field      => doc.id.as_str(),
            scope_field   => doc.scope.as_str(),
            content_field => doc.content.as_str(),
            kind_field    => doc.kind.as_str(),
        ))?;

        Ok(())
    }

    /// Delete a document by its ID.
    pub fn delete_document(&self, id: &str) -> Result<()> {
        let writer = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("search writer lock poisoned: {e}"))?;
        let id_field = self.schema.get_field(FIELD_ID).expect("field");
        writer.delete_term(tantivy::Term::from_field_text(id_field, id));
        Ok(())
    }

    /// Commit pending writes and reload the reader.
    pub fn commit(&self) -> Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("search writer lock poisoned: {e}"))?;
        writer.commit().context("tantivy commit")?;
        self.reader.reload().context("reload reader after commit")?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read
    // -----------------------------------------------------------------------

    /// Full-text BM25 search. Returns up to `limit` matching documents.
    pub fn search(&self, query: &str, scope: Option<&str>, limit: usize) -> Result<Vec<IndexDoc>> {
        let searcher = self.reader.searcher();

        let content_field = self.schema.get_field(FIELD_CONTENT).expect("field");

        // Natural-language input: OR of the jieba terms (same tokenizer as
        // indexing), so a question matches memories sharing any of its
        // words. QueryParser would make a multi-token CJK run a slop-0
        // phrase. Explicit `"quoted phrases"` keep QueryParser semantics.
        let parsed: Box<dyn Query> = if query.contains('"') {
            let parser = QueryParser::for_index(&self.index, vec![content_field]);
            parser
                .parse_query(query)
                .with_context(|| format!("parse query: {query}"))?
        } else {
            let clauses: Vec<(Occur, Box<dyn Query>)> = query_terms(query)
                .into_iter()
                .map(|t| {
                    let term = tantivy::Term::from_field_text(content_field, &t);
                    let q: Box<dyn Query> =
                        Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs));
                    (Occur::Should, q)
                })
                .collect();
            if clauses.is_empty() {
                return Ok(Vec::new());
            }
            Box::new(BooleanQuery::new(clauses))
        };

        let scan_limit = if scope.is_some() {
            limit.saturating_mul(8).max(limit).max(32)
        } else {
            limit
        };
        let top_docs = searcher
            .search(&parsed, &TopDocs::with_limit(scan_limit))
            .context("search")?;

        let id_field = self.schema.get_field(FIELD_ID).expect("field");
        let scope_fld = self.schema.get_field(FIELD_SCOPE).expect("field");
        let content_fld = self.schema.get_field(FIELD_CONTENT).expect("field");
        let kind_fld = self.schema.get_field(FIELD_KIND).expect("field");

        let mut results = Vec::with_capacity(top_docs.len());
        for (_score, doc_addr) in top_docs {
            let retrieved: TantivyDocument = searcher.doc(doc_addr).context("retrieve doc")?;

            let get_str = |field| {
                retrieved
                    .get_first(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            };

            let doc_scope = get_str(scope_fld);
            if let Some(wanted_scope) = scope
                && doc_scope != wanted_scope
            {
                continue;
            }

            results.push(IndexDoc {
                id: get_str(id_field),
                scope: doc_scope,
                content: get_str(content_fld),
                kind: get_str(kind_fld),
            });
            if results.len() >= limit {
                break;
            }
        }

        Ok(results)
    }

    /// Index a memory document in tantivy for BM25 search, then commit.
    ///
    /// Convenience for single-doc writers. Bulk paths (startup backfill)
    /// must call `index_document` per doc and `commit` once — a tantivy
    /// commit per doc is orders of magnitude slower.
    pub fn index_memory_doc(&self, id: &str, scope: &str, kind: &str, text: &str) -> Result<()> {
        self.index_document(&IndexDoc {
            id: id.to_owned(),
            scope: scope.to_owned(),
            content: text.to_owned(),
            kind: kind.to_owned(),
        })?;
        self.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IndexDoc
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IndexDoc {
    /// Unique stable ID (e.g. memory UUID, session key).
    pub id: String,
    /// Scope for multi-tenant filtering (e.g. "global", "agent:main").
    pub scope: String,
    /// Full text content to be indexed.
    pub content: String,
    /// Document kind tag ("memory", "session", "skill").
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_tmp() -> (SearchIndex, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let idx = SearchIndex::open(dir.path(), MemoryTier::Low).expect("open");
        (idx, dir)
    }

    #[test]
    fn index_and_search() {
        let (idx, _dir) = open_tmp();

        idx.index_document(&IndexDoc {
            id: "doc-1".to_owned(),
            scope: "global".to_owned(),
            content: "Rust is a systems programming language".to_owned(),
            kind: "memory".to_owned(),
        })
        .expect("index");

        idx.index_document(&IndexDoc {
            id: "doc-2".to_owned(),
            scope: "global".to_owned(),
            content: "Python is great for data science".to_owned(),
            kind: "memory".to_owned(),
        })
        .expect("index");

        idx.commit().expect("commit");

        let results = idx.search("Rust programming", None, 10).expect("search");
        assert!(!results.is_empty(), "should find Rust doc");
        assert_eq!(results[0].id, "doc-1");
    }

    #[test]
    fn delete_removes_from_results() {
        let (idx, _dir) = open_tmp();

        idx.index_document(&IndexDoc {
            id: "to-delete".to_owned(),
            scope: "global".to_owned(),
            content: "this document will be deleted".to_owned(),
            kind: "memory".to_owned(),
        })
        .expect("index");
        idx.commit().expect("commit");

        idx.delete_document("to-delete").expect("delete");
        idx.commit().expect("commit after delete");

        let results = idx.search("deleted", None, 10).expect("search");
        assert!(
            results.iter().all(|r| r.id != "to-delete"),
            "deleted doc should not appear"
        );
    }

    #[test]
    fn empty_index_returns_empty() {
        let (idx, _dir) = open_tmp();
        let results = idx.search("anything", None, 10).expect("search");
        assert!(results.is_empty());
    }

    #[test]
    fn search_filters_by_scope() {
        let (idx, _dir) = open_tmp();

        idx.index_document(&IndexDoc {
            id: "main-doc".to_owned(),
            scope: "agent:main".to_owned(),
            content: "sentinel lucky number 975318642".to_owned(),
            kind: "remember".to_owned(),
        })
        .expect("index main");
        idx.index_document(&IndexDoc {
            id: "other-doc".to_owned(),
            scope: "agent:other".to_owned(),
            content: "sentinel lucky number 111111111".to_owned(),
            kind: "remember".to_owned(),
        })
        .expect("index other");
        idx.commit().expect("commit");

        let results = idx
            .search("sentinel lucky number", Some("agent:main"), 10)
            .expect("search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "main-doc");
    }

    #[test]
    fn chinese_memory_matches_natural_question() {
        let (idx, _dir) = open_tmp();
        idx.index_document(&IndexDoc {
            id: "zh".to_owned(),
            scope: "global".to_owned(),
            content: "用户的手机号码是13800138000，住在杭州".to_owned(),
            kind: "memory".to_owned(),
        })
        .expect("index");
        idx.commit().expect("commit");
        let results = idx.search("我的手机号码是多少", None, 10).expect("search");
        assert_eq!(results.len(), 1, "CJK question should match: {results:?}");
        assert_eq!(results[0].id, "zh");
    }
}
