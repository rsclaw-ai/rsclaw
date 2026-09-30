//! CJK-aware tantivy tokenizer. The implementation lives in
//! `rsclaw_store::cjk` so the memory BM25 index (in `rsclaw-store`) and the
//! KB share one tokenizer and one jieba dictionary; re-exported here for the
//! existing `crate::index::cjk` paths.

pub use rsclaw_store::cjk::{CJK_TOKENIZER, JiebaTokenizer, query_terms};
