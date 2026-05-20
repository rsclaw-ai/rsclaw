//! CJK-aware tantivy tokenizer backed by jieba-rs.
//!
//! tantivy's default analyzer splits on whitespace + lowercases, which
//! tokenises Chinese text into a single giant "word" per sentence —
//! BM25 then can't match any partial query. This module exposes
//! `JiebaTokenizer` (registered under the name `cjk`) that pipes the
//! input through `jieba.tokenize(Search)` and emits one token per
//! segment with byte offsets.
//!
//! Whitespace-only and ASCII text round-trip through jieba unchanged
//! (jieba's segmenter is unicode-aware and falls back to character
//! groups for Latin runs), so the tokenizer is safe to use for the
//! whole corpus rather than only Chinese docs.

use jieba_rs::{Jieba, TokenizeMode};
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
        // `tokenize(Search)` returns smaller-grained, possibly overlapping
        // segments suitable for recall, and crucially gives each segment its
        // own (start, end) range. The previous `cut_for_search` + manual
        // `find()` loop tracked a single monotonic byte cursor, which
        // overshot `text.len()` on overlapping CJK sub-tokens and panicked.
        //
        // jieba reports offsets as char indices; tantivy wants byte offsets,
        // so map char index -> byte offset. `char_byte[i]` is the byte offset
        // of the i-th char; the final entry is `text.len()` so end offsets
        // resolve.
        let mut char_byte: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        char_byte.push(text.len());

        let segs = self.jieba.tokenize(text, TokenizeMode::Search, true);
        let mut tokens = Vec::with_capacity(segs.len());
        let mut position: usize = 0;
        for seg in segs {
            // Skip whitespace-only segments (jieba returns them as separate
            // tokens; they pollute the index).
            if seg.word.trim().is_empty() {
                continue;
            }
            tokens.push(Token {
                offset_from: char_byte[seg.start],
                offset_to: char_byte[seg.end],
                position,
                text: seg.word.to_lowercase(),
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
    fn mixed_cjk_ascii_multiline_does_not_panic() {
        // Regression: cut_for_search emits overlapping sub-tokens for CJK
        // compounds; the old monotonic-cursor offset loop overshot
        // text.len() and panicked on `text[cursor_bytes..]`.
        let text = "量子纠缠是量子力学中两个粒子相互关联的现象 quantum entanglement.\n\
                    The capital of France is Paris, a city famous for the Eiffel Tower.\n\
                    Rust ownership and borrowing prevent data races at compile time.";
        let tokens = tokenize(text);
        assert!(tokens.iter().any(|t| t == "量子"), "got: {tokens:?}");
        assert!(tokens.iter().any(|t| t == "quantum"), "got: {tokens:?}");
        assert!(tokens.iter().any(|t| t == "paris"), "got: {tokens:?}");
    }

    #[test]
    fn offsets_slice_back_to_token() {
        // Every token's byte offsets must be valid char boundaries within
        // the source, and the slice must equal the token's (lowercased) text.
        let text = "量子纠缠 quantum 力学 entanglement";
        let mut tk = JiebaTokenizer::new();
        let mut stream = tk.token_stream(text);
        while stream.advance() {
            let t = stream.token();
            assert!(t.offset_to <= text.len(), "offset_to {} > len {}", t.offset_to, text.len());
            assert!(text.is_char_boundary(t.offset_from), "from not char boundary: {}", t.offset_from);
            assert!(text.is_char_boundary(t.offset_to), "to not char boundary: {}", t.offset_to);
            assert_eq!(
                text[t.offset_from..t.offset_to].to_lowercase(),
                t.text,
                "slice must equal token text"
            );
        }
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
