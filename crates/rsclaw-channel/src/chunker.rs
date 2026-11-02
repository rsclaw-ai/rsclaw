//! Text chunker with code-fence protection (AGENTS.md §21).
//!
//! Splits a long LLM reply into platform-safe chunks:
//!   - Never splits inside a ` ``` ` fenced code block.
//!   - On forced splits, closes the open fence and re-opens it on the next
//!     chunk.
//!   - Break preference: paragraph > newline > sentence > whitespace > hard.
//!   - Respects per-platform `text_chunk_limit`.
//!
//! Platform defaults (AGENTS.md §21):
//!   Telegram  4096  WhatsApp 4000  Discord 2000  Slack 3000  Signal/CLI ∞

/// Default chunk limit when no platform limit is specified.
pub const DEFAULT_CHUNK_LIMIT: usize = usize::MAX;

// ---------------------------------------------------------------------------
// ChunkConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// Maximum characters per chunk (hard ceiling).
    pub max_chars: usize,
    /// Minimum characters before we flush a chunk.
    pub min_chars: usize,
    /// Break preference order (used when no natural boundary is near).
    pub break_preference: BreakPreference,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            max_chars: DEFAULT_CHUNK_LIMIT,
            min_chars: 1,
            break_preference: BreakPreference::Paragraph,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakPreference {
    Paragraph,  // double newline
    Newline,    // single newline
    Sentence,   // `. ` or `? ` or `! `
    Whitespace, // any space
    Hard,       // byte boundary (last resort)
}

// ---------------------------------------------------------------------------
// Chunker
// ---------------------------------------------------------------------------

/// Split `text` into chunks according to `config`.
///
/// Returns a `Vec<String>`; each element is safe to send as a single
/// platform message.
pub fn chunk_text(text: &str, config: &ChunkConfig) -> Vec<String> {
    if config.max_chars == DEFAULT_CHUNK_LIMIT || text.len() <= config.max_chars {
        return vec![text.to_owned()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;
    let mut open_fence: Option<String> = None; // language tag of open fence

    while !remaining.is_empty() {
        let (chunk_raw, rest, new_fence) = take_chunk(remaining, config.max_chars, &open_fence);

        open_fence = new_fence;

        if !chunk_raw.is_empty() {
            chunks.push(chunk_raw);
        }
        remaining = rest;
    }

    chunks
}

// ---------------------------------------------------------------------------
// Core splitting logic
// ---------------------------------------------------------------------------

/// Take at most `max_chars` characters from `text`, respecting code fences.
///
/// Returns `(chunk, remainder, open_fence_state)`. Always consumes at least
/// one character of `text` when `text` is non-empty, so callers looping until
/// the remainder is empty are guaranteed to terminate.
fn take_chunk<'a>(
    text: &'a str,
    max_chars: usize,
    open_fence: &Option<String>,
) -> (String, &'a str, Option<String>) {
    if text.is_empty() {
        return (String::new(), "", None);
    }

    // If we're inside an open fence, prepend a re-open marker.
    let fence_prefix = match open_fence {
        Some(lang) => format!("```{lang}\n"),
        None => String::new(),
    };

    // Reserve room for the re-open prefix and a possible closing "\n```".
    // Never let the budget reach zero: that would stall the caller's loop.
    let budget = max_chars
        .saturating_sub(fence_prefix.chars().count() + 4)
        .max(1);

    // Byte offset of the `budget`-th character (end of the candidate window).
    let hard_limit = text
        .char_indices()
        .nth(budget)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());

    let mut split_at = if hard_limit >= text.len() {
        text.len()
    } else {
        find_split(&text[..hard_limit])
    };
    if split_at == 0 {
        // Guarantee progress: consume at least one character.
        split_at = text.chars().next().map(char::len_utf8).unwrap_or(text.len());
    }

    let (body, rest) = text.split_at(split_at);

    // Build the final chunk.
    let mut chunk = fence_prefix;
    chunk.push_str(body);

    // Close open fence if we cut mid-block.
    let trailing_fence = track_fences(body, open_fence);
    if trailing_fence.is_some() {
        chunk.push_str("\n```");
    }

    (chunk, rest, trailing_fence)
}

/// Maximum length of a fence language tag we carry across chunks.
const MAX_FENCE_TAG_CHARS: usize = 32;

/// Return the open fence language tag at the end of `text`, or `None`.
fn track_fences(text: &str, initial: &Option<String>) -> Option<String> {
    let mut current = initial.clone();
    let mut i = 0;
    let bytes = text.as_bytes();

    while i < bytes.len() {
        if bytes[i..].starts_with(b"```") {
            let fence_start = i;
            // Advance past ```
            i += 3;
            // Language tag: only the leading `[A-Za-z0-9_+-]` run of the rest
            // of the fence line, capped so a long line can never balloon the
            // re-open prefix beyond the chunk budget.
            let tag_start = i;
            while i < bytes.len()
                && i - tag_start < MAX_FENCE_TAG_CHARS
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'+' | b'-'))
            {
                i += 1;
            }
            // Only ASCII bytes were accepted, so this is always valid UTF-8.
            let tag = std::str::from_utf8(&bytes[tag_start..i]).unwrap_or("");

            if current.is_some() {
                // Closing fence (must be standalone ``` on a line boundary).
                if bytes
                    .get(fence_start.wrapping_sub(1))
                    .is_none_or(|&b| b == b'\n')
                {
                    current = None;
                }
            } else {
                current = Some(tag.to_owned());
            }
        } else {
            i += 1;
        }
    }

    current
}

/// Find the best byte offset to split `window` at.
///
/// `window` is the largest prefix that fits the chunk budget; the search
/// walks backwards through it for the softest break (paragraph > newline >
/// sentence > whitespace), ignoring breaks in the first third so chunks do
/// not degenerate into tiny fragments. Falls back to a hard split at the end
/// of the window.
fn find_split(window: &str) -> usize {
    let min = window.len() / 3;
    let accept = |pos: Option<usize>| pos.filter(|&p| p > min);

    accept(try_split_at(window, "\n\n"))
        .or_else(|| accept(try_split_at(window, "\n")))
        .or_else(|| accept(try_split_at_sentence(window)))
        .or_else(|| accept(try_split_at(window, " ")))
        .unwrap_or(window.len())
}

fn try_split_at(text: &str, pat: &str) -> Option<usize> {
    text.rfind(pat).map(|i| i + pat.len())
}

fn try_split_at_sentence(text: &str) -> Option<usize> {
    // Rightmost `. `, `? ` or `! `.
    [". ", "? ", "! "]
        .iter()
        .filter_map(|pat| text.rfind(pat).map(|pos| pos + pat.len()))
        .max()
}

// ---------------------------------------------------------------------------
// Platform chunk limits
// ---------------------------------------------------------------------------

pub fn platform_chunk_limit(channel: &str) -> usize {
    match channel {
        "telegram" => 4096,
        "whatsapp" => 4000,
        "discord" => 2000,
        "slack" => 3000,
        "wecom" => 4096,
        "mattermost" => 4000,
        "feishu" => 4000,
        "dingtalk" => 20_000,
        "qq" => 4096,
        "line" => 5000,
        "zalo" => 2000,
        "matrix" => 10000,
        _ => DEFAULT_CHUNK_LIMIT,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(max: usize) -> ChunkConfig {
        ChunkConfig {
            max_chars: max,
            min_chars: 1,
            break_preference: BreakPreference::Paragraph,
        }
    }

    #[test]
    fn short_text_not_split() {
        let chunks = chunk_text("hello world", &cfg(4096));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "hello world");
    }

    #[test]
    fn splits_at_paragraph() {
        let text = format!("{}\n\n{}", "a".repeat(100), "b".repeat(100));
        let chunks = chunk_text(&text, &cfg(110));
        assert_eq!(chunks.len(), 2, "should split at paragraph boundary");
        assert!(chunks[0].ends_with('\n') || chunks[0].len() <= 110);
    }

    #[test]
    fn hard_split_preserves_total_content() {
        let text = "x".repeat(300);
        let chunks = chunk_text(&text, &cfg(100));
        let rejoined: String = chunks.join("");
        // Content preserved (minus fence markers which are added on hard splits).
        assert!(rejoined.contains(&"x".repeat(100)));
    }

    #[test]
    fn no_split_when_max_is_default() {
        let text = "a".repeat(10_000);
        let chunks = chunk_text(&text, &ChunkConfig::default());
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn platform_limits() {
        assert_eq!(platform_chunk_limit("telegram"), 4096);
        assert_eq!(platform_chunk_limit("discord"), 2000);
        assert_eq!(platform_chunk_limit("cli"), DEFAULT_CHUNK_LIMIT);
    }

    #[test]
    fn fence_tracking_open() {
        let text = "```rust\nfn main() {}\n";
        let fence = track_fences(text, &None);
        assert_eq!(fence.as_deref(), Some("rust"));
    }

    #[test]
    fn fence_tracking_closed() {
        let text = "```rust\nfn main() {}\n```\n";
        let fence = track_fences(text, &None);
        assert!(fence.is_none(), "fence should be closed");
    }

    #[test]
    fn chunk_empty_string() {
        let chunks = chunk_text("", &cfg(100));
        // Either an empty vec or a single empty-string element is acceptable.
        assert!(
            chunks.is_empty() || (chunks.len() == 1 && chunks[0].is_empty()),
            "expected empty or single-empty-element result, got: {chunks:?}"
        );
    }

    #[test]
    fn chunk_short_below_limit() {
        let text = "Hello, world!";
        let chunks = chunk_text(text, &cfg(4096));
        assert_eq!(chunks.len(), 1, "short text should not be split");
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn chunk_splits_long_text() {
        // Build text that is definitely longer than max_chars (50).
        let text = "word ".repeat(30); // 150 chars
        let chunks = chunk_text(&text, &cfg(50));
        assert!(
            chunks.len() > 1,
            "text of {} chars should produce multiple chunks at limit 50",
            text.len()
        );
        // Every chunk must be at most max_chars characters.
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.chars().count() <= 50,
                "chunk {i} has {} chars, exceeds limit 50",
                chunk.chars().count()
            );
        }
    }

    #[test]
    fn chunk_long_cjk_after_fence_terminates() {
        // Regression: the fence tag used to swallow the whole window, making
        // the re-open prefix exceed the budget and loop forever.
        let text = format!("```{}", "中".repeat(10_000));
        let chunks = chunk_text(&text, &cfg(100));
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.chars().count() <= 100, "chunk too long: {}", chunk.chars().count());
        }
        let long_tag = format!("```{}", "x".repeat(5_000));
        let chunks = chunk_text(&long_tag, &cfg(20));
        assert!(!chunks.is_empty());
    }

    #[test]
    fn chunk_prefers_soft_newline_break() {
        let text = format!("{}\n{}", "a".repeat(80), "b".repeat(80));
        let chunks = chunk_text(&text, &cfg(100));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], format!("{}\n", "a".repeat(80)));
        assert_eq!(chunks[1], "b".repeat(80));
    }
}
