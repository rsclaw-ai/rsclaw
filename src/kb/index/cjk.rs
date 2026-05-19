//! CJK-aware tantivy tokenizer backed by jieba-rs.
//!
//! tantivy's default analyzer splits on whitespace + lowercases, which
//! tokenises Chinese text into a single giant "word" per sentence —
//! BM25 then can't match any partial query. This module exposes
//! `JiebaTokenizer` (registered under the name `cjk`) that pipes the
//! input through `jieba.cut_for_search` and emits one token per
//! segment with byte offsets.
//!
//! Whitespace-only and ASCII text round-trip through jieba unchanged
//! (jieba's segmenter is unicode-aware and falls back to character
//! groups for Latin runs), so the tokenizer is safe to use for the
//! whole corpus rather than only Chinese docs.

use jieba_rs::Jieba;
use std::sync::Arc;
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

#[derive(Clone)]
pub struct JiebaTokenizer {
    jieba: Arc<Jieba>,
}

impl JiebaTokenizer {
    pub fn new() -> Self {
        Self {
            jieba: Arc::new(Jieba::new()),
        }
    }
}

impl Default for JiebaTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct JiebaTokenStream {
    tokens: Vec<Token>,
    cursor: usize,
}

impl TokenStream for JiebaTokenStream {
    fn advance(&mut self) -> bool {
        if self.cursor >= self.tokens.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.tokens[self.cursor - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.cursor - 1]
    }
}

impl Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = JiebaTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        // `cut_for_search` returns smaller-grained segments suitable
        // for search recall (longer compound words are split into
        // their parts as well).
        let segs = self.jieba.cut_for_search(text, true);
        let mut tokens = Vec::with_capacity(segs.len());
        let mut cursor_bytes: usize = 0;
        let mut position: usize = 0;
        for seg in segs {
            // Skip whitespace-only segments (jieba returns them as
            // separate tokens; they pollute the index).
            let trimmed = seg.trim();
            if trimmed.is_empty() {
                // Still advance the byte cursor so subsequent token
                // offsets remain correct.
                if let Some(found) = text[cursor_bytes..].find(seg) {
                    cursor_bytes += found + seg.len();
                }
                continue;
            }
            // Find the segment in the source (jieba returns &str
            // slices borrowed from `text`, but we can't always trust
            // pointer arithmetic across Cow boundaries — search from
            // the current cursor to keep offsets right).
            let offset_from = match text[cursor_bytes..].find(seg) {
                Some(pos) => cursor_bytes + pos,
                None => cursor_bytes,
            };
            let offset_to = offset_from + seg.len();
            cursor_bytes = offset_to;
            tokens.push(Token {
                offset_from,
                offset_to,
                position,
                text: seg.to_lowercase(),
                position_length: 1,
            });
            position += 1;
        }
        JiebaTokenStream { tokens, cursor: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(text: &str) -> Vec<String> {
        let mut tk = JiebaTokenizer::new();
        let mut stream = tk.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn english_lowercased() {
        let tokens = tokenize("The Quick Brown Fox");
        // jieba treats Latin words as separate; "The" → "the" etc.
        assert!(tokens.iter().any(|t| t == "quick"));
        assert!(tokens.iter().any(|t| t == "brown"));
        assert!(!tokens.iter().any(|t| t == "Brown"), "should be lowercased");
    }

    #[test]
    fn chinese_splits_into_words() {
        // 蒙牛 + 奶粉 + 冲泡 + 指南 (or similar segmentation)
        let tokens = tokenize("蒙牛奶粉冲泡指南");
        assert!(
            tokens.iter().any(|t| t == "蒙牛"),
            "expected 蒙牛 as a token, got: {tokens:?}"
        );
        assert!(
            tokens.iter().any(|t| t == "奶粉"),
            "expected 奶粉 as a token, got: {tokens:?}"
        );
    }

    #[test]
    fn mixed_zh_en() {
        let tokens = tokenize("Apple 苹果 brand 品牌");
        assert!(tokens.iter().any(|t| t == "apple"));
        assert!(tokens.iter().any(|t| t == "苹果"));
        assert!(tokens.iter().any(|t| t == "brand"));
        assert!(tokens.iter().any(|t| t == "品牌"));
    }

    #[test]
    fn empty_input() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn whitespace_only() {
        let tokens = tokenize("   \n\t  ");
        assert!(tokens.is_empty(), "whitespace should not produce tokens, got: {tokens:?}");
    }
}
