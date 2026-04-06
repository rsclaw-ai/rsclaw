//! Integration tests for the chunker module (`rsclaw::channel::chunker`).
//!
//! Tests code-fence protection, break preferences, platform limits,
//! boundary conditions, and Unicode handling.

use rsclaw::channel::chunker::{
    chunk_text, platform_chunk_limit, BreakPreference, ChunkConfig, DEFAULT_CHUNK_LIMIT,
};

fn cfg(max: usize) -> ChunkConfig {
    ChunkConfig {
        max_chars: max,
        min_chars: 1,
        break_preference: BreakPreference::Paragraph,
    }
}

// ---------------------------------------------------------------------------
// Code-fence protection
// ---------------------------------------------------------------------------

#[test]
fn fence_not_split_inside_code_block() {
    // A code block that must be split should close the fence at the split
    // point and reopen it in the next chunk.
    let text = "```\nline1\nline2\nline3\nline4\nline5\n```";
    let chunks = chunk_text(text, &cfg(25));
    assert!(
        chunks.len() >= 2,
        "text should be split into multiple chunks, got {}",
        chunks.len()
    );
    // First chunk must close the fence it inherited.
    assert!(
        chunks[0].contains("```"),
        "first chunk must contain fence markers"
    );
    // Second chunk must reopen the fence.
    assert!(
        chunks[1].starts_with("```"),
        "second chunk must reopen the fence: {:?}",
        chunks[1]
    );
}

#[test]
fn fence_reopened_on_forced_split() {
    // Long code inside a fence forces a split; verify fence close/reopen.
    let code_lines: String = (0..50).map(|i| format!("let x{i} = {i};\n")).collect();
    let text = format!("```\n{code_lines}```");
    let chunks = chunk_text(&text, &cfg(100));
    assert!(
        chunks.len() >= 2,
        "expected multiple chunks for long code block"
    );
    // Every intermediate chunk should end with ``` (close) and the next should
    // start with ``` (reopen).
    for i in 0..chunks.len() - 1 {
        assert!(
            chunks[i].trim_end().ends_with("```"),
            "chunk {i} should close the fence: {:?}",
            chunks[i]
        );
        assert!(
            chunks[i + 1].starts_with("```"),
            "chunk {} should reopen the fence: {:?}",
            i + 1,
            chunks[i + 1]
        );
    }
}

#[test]
fn fence_with_language_tag_preserved() {
    // Language tag (e.g. "typescript") must be preserved when reopening.
    let code_lines: String = (0..30).map(|i| format!("const x{i} = {i};\n")).collect();
    let text = format!("```typescript\n{code_lines}```");
    let chunks = chunk_text(&text, &cfg(120));
    assert!(
        chunks.len() >= 2,
        "expected multiple chunks, got {}",
        chunks.len()
    );
    // The reopened fence in chunk 2+ should have the language tag.
    assert!(
        chunks[1].starts_with("```typescript"),
        "reopened fence should preserve language tag: {:?}",
        chunks[1]
    );
}

// ---------------------------------------------------------------------------
// Break preferences
// ---------------------------------------------------------------------------

#[test]
fn break_preference_paragraph_first() {
    // When a paragraph break (\n\n) exists, split there before \n.
    let text = format!("{}\n\n{}", "a".repeat(40), "b".repeat(40));
    let chunks = chunk_text(&text, &cfg(50));
    assert_eq!(chunks.len(), 2, "should split at paragraph boundary");
    // The split happens after \n\n, so the first chunk should contain the
    // paragraph separator and the second chunk should start with "b"s.
    assert!(
        chunks[0].contains("\n\n"),
        "first chunk should contain paragraph break: {:?}",
        chunks[0]
    );
    assert!(
        chunks[1].starts_with('b'),
        "second chunk should start with 'b' content: {:?}",
        chunks[1]
    );
}

#[test]
fn break_preference_newline_when_no_paragraph() {
    // No \n\n present, so it should split at \n.
    let text = format!("{}\n{}", "a".repeat(40), "b".repeat(40));
    let chunks = chunk_text(&text, &cfg(50));
    assert!(
        chunks.len() >= 2,
        "should split into at least 2 chunks, got {}",
        chunks.len()
    );
}

#[test]
fn break_preference_sentence() {
    // No newlines, but sentence endings (". ") are present.
    let text = format!("{}. {}", "a".repeat(30), "b".repeat(30));
    let chunks = chunk_text(&text, &cfg(40));
    assert!(
        chunks.len() >= 2,
        "should split at sentence boundary, got {} chunks",
        chunks.len()
    );
}

#[test]
fn break_preference_whitespace() {
    // No newlines or sentences, but spaces exist.
    let text = format!("{} {}", "a".repeat(30), "b".repeat(30));
    let chunks = chunk_text(&text, &cfg(40));
    assert!(
        chunks.len() >= 2,
        "should split at whitespace, got {} chunks",
        chunks.len()
    );
}

#[test]
fn break_hard_when_no_whitespace() {
    // No whitespace at all forces a hard split.
    let text = "x".repeat(200);
    let chunks = chunk_text(&text, &cfg(50));
    assert!(
        chunks.len() >= 4,
        "200 chars / 50 limit should yield 4+ chunks, got {}",
        chunks.len()
    );
    for (i, chunk) in chunks.iter().enumerate() {
        assert!(
            chunk.chars().count() <= 50,
            "chunk {i} exceeds limit: {} chars",
            chunk.chars().count()
        );
    }
}

// ---------------------------------------------------------------------------
// Platform limits
// ---------------------------------------------------------------------------

#[test]
fn all_platform_limits_correct() {
    let expected = [
        ("telegram", 4096),
        ("whatsapp", 4000),
        ("discord", 2000),
        ("slack", 3000),
        ("wecom", 4096),
        ("mattermost", 4000),
        ("feishu", 4000),
        ("dingtalk", 20_000),
        ("qq", 4096),
        ("line", 5000),
        ("zalo", 2000),
        ("matrix", 10000),
        // Unknown channels get the default.
        ("cli", DEFAULT_CHUNK_LIMIT),
        ("signal", DEFAULT_CHUNK_LIMIT),
        ("custom", DEFAULT_CHUNK_LIMIT),
    ];
    for (channel, limit) in expected {
        assert_eq!(
            platform_chunk_limit(channel),
            limit,
            "wrong limit for channel '{channel}'"
        );
    }
}

// ---------------------------------------------------------------------------
// Boundary conditions
// ---------------------------------------------------------------------------

#[test]
fn chunk_exact_limit_no_split() {
    let text = "a".repeat(100);
    let chunks = chunk_text(&text, &cfg(100));
    assert_eq!(chunks.len(), 1, "text exactly at limit should not be split");
    assert_eq!(chunks[0], text);
}

#[test]
fn chunk_one_char_over_limit() {
    let text = "a".repeat(101);
    let chunks = chunk_text(&text, &cfg(100));
    assert_eq!(
        chunks.len(),
        2,
        "text one char over limit should produce 2 chunks"
    );
}

// ---------------------------------------------------------------------------
// Unicode / CJK
// ---------------------------------------------------------------------------

#[test]
fn chunk_unicode_cjk() {
    // Chinese characters are multi-byte; splitting must not break mid-character.
    let text = "中".repeat(200);
    let chunks = chunk_text(&text, &cfg(50));
    assert!(chunks.len() >= 4, "CJK text should be chunked");
    for (i, chunk) in chunks.iter().enumerate() {
        // Every chunk must be valid UTF-8 (it is, since it's a String).
        // Also ensure no partial characters.
        assert!(
            chunk.chars().count() <= 50,
            "chunk {i} exceeds 50 chars: {}",
            chunk.chars().count()
        );
    }
}

// ---------------------------------------------------------------------------
// Content preservation
// ---------------------------------------------------------------------------

#[test]
fn chunk_preserves_total_content() {
    // Plain text (no fences) should rejoin exactly.
    let text: String = (0..300).map(|i| format!("word{i} ")).collect();
    let chunks = chunk_text(&text, &cfg(100));
    let rejoined: String = chunks.join("");
    assert_eq!(
        rejoined, text,
        "rejoined chunks should equal original for plain text"
    );
}
