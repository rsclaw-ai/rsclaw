//! Sparse-text cache backed by tantivy 0.22. Source of truth is the
//! markdown body on disk (`md/*.md` referenced by `KbChunk.byte_offset`);
//! tantivy holds an inverted index keyed on `chunk_id`.

use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result};
use tantivy::{
    Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term,
    collector::TopDocs,
    directory::MmapDirectory,
    doc,
    query::QueryParser,
    schema::{
        Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions, Value,
    },
};

use crate::kb::{index::cjk::JiebaTokenizer, store::KbStore};

const CJK_TOKENIZER: &str = "cjk";

const WRITER_HEAP_BYTES: usize = 50_000_000;

pub struct TantivyIndex {
    index: Index,
    schema: TantivySchema,
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
}

struct TantivySchema {
    chunk_id: Field,
    doc_id: Field,
    indexed_text: Field,
}

impl TantivyIndex {
    /// Open or create an index at `path`. Path is `idx/tantivy/` under
    /// `KbPaths::root` by convention.
    pub fn open_or_create(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("create_dir_all {}", path.display()))?;
        let dir = MmapDirectory::open(path)
            .with_context(|| format!("MmapDirectory::open {}", path.display()))?;
        let mut sb = Schema::builder();
        let chunk_id = sb.add_text_field("chunk_id", STRING | STORED);
        let doc_id = sb.add_text_field("doc_id", STRING | STORED);
        // indexed_text uses the jieba-backed CJK tokenizer so Chinese
        // queries can match into the BM25 index. Registered below.
        let text_opts = TextOptions::default().set_stored().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CJK_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let indexed_text = sb.add_text_field("indexed_text", text_opts);
        let schema_obj = sb.build();

        let index = if Index::exists(&dir)? {
            Index::open_in_dir(path).with_context(|| "open existing tantivy")?
        } else {
            Index::create_in_dir(path, schema_obj.clone()).with_context(|| "create tantivy")?
        };
        // Register the CJK tokenizer on every open (not persisted in
        // the index metadata; must re-register after each restart).
        index
            .tokenizers()
            .register(CJK_TOKENIZER, JiebaTokenizer::new());
        let writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            schema: TantivySchema {
                chunk_id,
                doc_id,
                indexed_text,
            },
            writer: Mutex::new(writer),
            reader,
        })
    }

    /// Replace any existing entry for `chunk_id`, then add the new one.
    /// Caller must call `commit()` to flush.
    pub fn upsert(&self, chunk_id: &str, doc_id: &str, indexed_text: &str) -> Result<()> {
        let w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let term = Term::from_field_text(self.schema.chunk_id, chunk_id);
        w.delete_term(term);
        let d: TantivyDocument = doc!(
            self.schema.chunk_id => chunk_id,
            self.schema.doc_id => doc_id,
            self.schema.indexed_text => indexed_text,
        );
        w.add_document(d)?;
        Ok(())
    }

    pub fn commit(&self) -> Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        w.commit()?;
        drop(w);
        // Force reader reload so subsequent searches see the new docs
        // immediately (OnCommitWithDelay has a small lag we don't want
        // in tests + tight ingest loops).
        self.reader.reload()?;
        Ok(())
    }

    /// BM25 search. Returns `(chunk_id, score)` pairs sorted by
    /// descending score. Malformed queries return empty rather than
    /// errors so a typo in user input doesn't propagate.
    pub fn search(&self, query: &str, k: usize) -> Result<Vec<(String, f32)>> {
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.schema.indexed_text]);
        let q = match parser.parse_query(query) {
            Ok(q) => q,
            Err(_) => return Ok(Vec::new()),
        };
        let top = searcher.search(&q, &TopDocs::with_limit(k))?;
        let mut out = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let d: TantivyDocument = searcher.doc(addr)?;
            if let Some(v) = d.get_first(self.schema.chunk_id) {
                if let Some(s) = v.as_str() {
                    out.push((s.to_string(), score));
                }
            }
        }
        Ok(out)
    }

    /// Rebuild from redb. Drops all existing docs then re-adds every
    /// `KbChunk`.
    pub fn rebuild(&self, store: &KbStore) -> Result<()> {
        {
            let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            w.delete_all_documents()?;
            w.commit()?;
        }
        let rtx = store.begin_read()?;
        use redb::ReadableTable;
        let tbl = rtx.open_table(crate::kb::store::schema::KB_CHUNKS)?;
        let mut n = 0;
        for entry in tbl.iter()? {
            let (_, v) = entry?;
            let c: crate::kb::model::KbChunk = crate::kb::store::codec::decode(v.value())?;
            self.upsert(&c.id, &c.doc_id, &c.indexed_text)?;
            n += 1;
        }
        self.commit()?;
        tracing::info!(n, "kb tantivy: rebuild complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn fresh() -> (TempDir, TantivyIndex) {
        let tmp = TempDir::new().unwrap();
        let idx = TantivyIndex::open_or_create(&tmp.path().join("idx")).unwrap();
        (tmp, idx)
    }

    #[test]
    fn upsert_then_search_finds_match() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "the quick brown fox jumps over the lazy dog")
            .unwrap();
        idx.upsert("c2", "d1", "completely unrelated text about cats")
            .unwrap();
        idx.commit().unwrap();
        let hits = idx.search("brown fox", 5).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "c1");
    }

    #[test]
    fn upsert_replaces_previous() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "original text mentioning apples")
            .unwrap();
        idx.commit().unwrap();
        idx.upsert("c1", "d1", "rewritten text mentioning oranges")
            .unwrap();
        idx.commit().unwrap();
        assert!(
            idx.search("apples", 5).unwrap().is_empty(),
            "old version still indexed"
        );
        let hits = idx.search("oranges", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "c1");
    }

    #[test]
    fn malformed_query_returns_empty() {
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "hello").unwrap();
        idx.commit().unwrap();
        // Tantivy's QueryParser is tolerant; truly malformed inputs
        // pass through fine. This test just confirms we don't crash.
        let _ = idx.search("(((", 5).unwrap();
    }

    #[test]
    fn search_empty_returns_empty() {
        let (_tmp, idx) = fresh();
        let hits = idx.search("anything", 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn chinese_query_matches_chinese_doc() {
        // The default whitespace analyzer can't split CJK; jieba does.
        let (_tmp, idx) = fresh();
        idx.upsert("c1", "d1", "蒙牛奶粉冲泡指南：建议比例 1:7")
            .unwrap();
        idx.upsert("c2", "d1", "伊利酸奶发酵过程详解").unwrap();
        idx.commit().unwrap();
        let hits = idx.search("蒙牛", 5).unwrap();
        assert_eq!(hits.len(), 1, "expected 1 hit for 蒙牛, got {hits:?}");
        assert_eq!(hits[0].0, "c1");
        let hits = idx.search("奶粉", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "c1");
        let hits = idx.search("酸奶", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "c2");
    }
}
