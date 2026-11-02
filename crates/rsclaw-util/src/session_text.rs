//! Display helpers for persisted session messages.
//!
//! User messages are stored with a context prefix injected by the agent
//! runtime (it stays in storage: it timestamps history and is part of the
//! KV-cache prefix):
//!
//! ```text
//! Now: 2026-10-06 07:23 Mon CST
//! [Session started: 2026-10-06 07:23 Monday, CST, via feishu]   <- first message only
//! <original user text>
//! ```
//!
//! These helpers recover the original text for display and derive short
//! human-readable session titles from it. All truncation is char-based so
//! CJK text never panics.

/// Max chars of an auto-derived title (including the trailing ellipsis).
pub const AUTO_TITLE_MAX_CHARS: usize = 40;

/// Max chars of a user-supplied title.
pub const USER_TITLE_MAX_CHARS: usize = 80;

const NOW_PREFIX: &str = "Now: ";
const SESSION_STARTED_PREFIX: &str = "[Session started: ";

/// Split a persisted user message into `(sent_at, original_text)`.
///
/// Strips the runtime-injected `Now: ...` line and/or the
/// `[Session started: ...]` line (in either order, CRLF tolerated).
/// `sent_at` is `"YYYY-MM-DD HH:MM"` taken from the `Now:` line, falling back
/// to the session-start line; `None` when neither prefix is present. Text
/// without a recognised prefix is returned unchanged.
pub fn strip_injected_context(text: &str) -> (Option<String>, &str) {
    let mut rest = text;
    let mut now_at: Option<String> = None;
    let mut started_at: Option<String> = None;
    let mut seen_now = false;
    let mut seen_started = false;

    for _ in 0..2 {
        let (line, after) = match rest.find('\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        let line = line.strip_suffix('\r').unwrap_or(line);
        if !seen_now && let Some(body) = line.strip_prefix(NOW_PREFIX) {
            let Some(ts) = parse_timestamp(body) else {
                break;
            };
            now_at = Some(ts);
            seen_now = true;
            rest = after;
        } else if !seen_started
            && line.ends_with(']')
            && let Some(body) = line.strip_prefix(SESSION_STARTED_PREFIX)
        {
            started_at = parse_timestamp(body);
            seen_started = true;
            rest = after;
        } else {
            break;
        }
    }
    (now_at.or(started_at), rest)
}

/// Parse the leading `YYYY-MM-DD HH:MM` of a prefix line body. Returns the
/// date alone when the time token is missing or malformed, and `None` when
/// the body does not start with a date.
fn parse_timestamp(body: &str) -> Option<String> {
    let mut tokens = body.split_whitespace();
    let date = tokens.next()?;
    if !is_date(date) {
        return None;
    }
    match tokens.next().map(|t| t.trim_end_matches(',')) {
        Some(time) if is_time(time) => Some(format!("{date} {time}")),
        _ => Some(date.to_owned()),
    }
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

fn is_time(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 5
        && b.iter().enumerate().all(|(i, c)| match i {
            2 => *c == b':',
            _ => c.is_ascii_digit(),
        })
}

/// Derive a short session title from the ORIGINAL user text (already
/// stripped of the injected context prefix).
///
/// Takes the first non-empty line after removing attachment markers such as
/// `[file:/path]`, collapses whitespace and caps the result at
/// [`AUTO_TITLE_MAX_CHARS`] chars (with a trailing `…` when cut). Returns
/// `None` for slash commands (text starting with `/`) and for text with no
/// usable content.
pub fn derive_title(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('/') {
        return None;
    }
    let cleaned = remove_attachment_markers(trimmed);
    let line = cleaned
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|l| !l.is_empty())?;
    Some(ellipsize(&line, AUTO_TITLE_MAX_CHARS))
}

/// Bracketed attachment markers removed from titles, e.g. `[file:/a.pdf]`,
/// `[file: report.pdf]` (Discord/Slack), `[image:...]`.
const MARKER_PREFIXES: &[&str] = &[
    "[file:",
    "[image:",
    "[audio:",
    "[video:",
    "[voice:",
    "[attachment:",
];

fn remove_attachment_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let candidate = &rest[start..];
        let is_marker = MARKER_PREFIXES.iter().any(|p| {
            candidate
                .get(..p.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(p))
        });
        match (is_marker, candidate.find(']')) {
            (true, Some(end)) => {
                out.push_str(&rest[..start]);
                out.push(' ');
                rest = &candidate[end + 1..];
            }
            _ => {
                out.push_str(&rest[..start + 1]);
                rest = &rest[start + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Cap `s` at `max_chars` chars; when cut, the last kept char is replaced by
/// `…` so the result never exceeds `max_chars`. Char-based (CJK safe).
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Return the first `max_chars` chars of `s` (char-based, CJK safe).
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Plain text of a stored message JSON: `content` as a string, or the
/// `text` fields of an array of parts joined by newlines. Empty when the
/// message carries no text.
pub fn message_text(message: &serde_json::Value) -> String {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Clean a title produced by an LLM: first non-empty line, without a
/// leading `Title:` label, surrounding quotes/brackets, trailing
/// punctuation, or an inline `<think>` block. Capped at
/// [`AUTO_TITLE_MAX_CHARS`] chars. `None` when nothing usable remains.
pub fn sanitize_llm_title(raw: &str) -> Option<String> {
    let raw = match raw.rfind("</think>") {
        Some(i) => &raw[i + "</think>".len()..],
        None => raw,
    };
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut s = line;
    for label in ["title:", "标题:", "标题："] {
        if s.len() >= label.len()
            && s.get(..label.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(label))
        {
            s = s[label.len()..].trim_start();
        }
    }
    const WRAP: &[char] = &[
        '"', '\'', '`', '“', '”', '‘', '’', '「', '」', '『', '』', '《', '》', '*', '#',
    ];
    const TRAIL: &[char] = &[
        '.', '。', '!', '！', '?', '？', ',', '，', ':', '：', ';', '；', '、', '…',
    ];
    let s = s
        .trim_matches(|c: char| WRAP.contains(&c) || c.is_whitespace())
        .trim_end_matches(|c: char| TRAIL.contains(&c) || c.is_whitespace());
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.is_empty() {
        return None;
    }
    Some(ellipsize(&s, AUTO_TITLE_MAX_CHARS))
}

/// Case-insensitive search for `needle` in `haystack`; on a hit returns a
/// whitespace-collapsed snippet of about `window` chars around it, with `…`
/// marking cut ends. Char-based (CJK safe). `None` when there is no hit or
/// the needle is empty.
pub fn match_snippet(haystack: &str, needle: &str, window: usize) -> Option<String> {
    let hay: Vec<char> = haystack.split_whitespace().collect::<Vec<_>>().join(" ").chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let needle_folded: Vec<char> = needle.iter().map(|c| fold(*c)).collect();
    let pos = (0..=hay.len() - needle.len())
        .find(|&i| (0..needle.len()).all(|j| fold(hay[i + j]) == needle_folded[j]))?;
    let context = window.saturating_sub(needle.len());
    let before = context / 2;
    let start = pos.saturating_sub(before);
    let end = (start + needle.len().max(window)).min(hay.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&hay[start..end]);
    if end < hay.len() {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_both_prefix_lines() {
        let text = "Now: 2026-10-06 07:23 Mon CST\n[Session started: 2026-10-06 07:23 Monday, CST, via feishu]\n帮我查一下天气";
        let (sent_at, body) = strip_injected_context(text);
        assert_eq!(sent_at.as_deref(), Some("2026-10-06 07:23"));
        assert_eq!(body, "帮我查一下天气");
    }

    #[test]
    fn strip_now_line_only() {
        let (sent_at, body) =
            strip_injected_context("Now: 2026-10-06 21:05 Tue +08:00\nhello\nworld");
        assert_eq!(sent_at.as_deref(), Some("2026-10-06 21:05"));
        assert_eq!(body, "hello\nworld");
    }

    #[test]
    fn strip_session_started_only_and_crlf() {
        let text = "[Session started: 2026-01-02 03:04 Friday, UTC, via telegram]\r\nhi there";
        let (sent_at, body) = strip_injected_context(text);
        assert_eq!(sent_at.as_deref(), Some("2026-01-02 03:04"));
        assert_eq!(body, "hi there");

        let text = "Now: 2026-10-06 07:23 Mon CST\r\n[Session started: 2026-10-05 07:23 Sunday, CST, via ws]\r\nx";
        let (sent_at, body) = strip_injected_context(text);
        assert_eq!(sent_at.as_deref(), Some("2026-10-06 07:23"));
        assert_eq!(body, "x");
    }

    #[test]
    fn strip_leaves_plain_text_untouched() {
        let (sent_at, body) = strip_injected_context("Now: let's talk\nabout it");
        assert!(sent_at.is_none());
        assert_eq!(body, "Now: let's talk\nabout it");
        let (sent_at, body) = strip_injected_context("just text");
        assert!(sent_at.is_none());
        assert_eq!(body, "just text");
    }

    #[test]
    fn strip_prefix_without_body() {
        let (sent_at, body) = strip_injected_context("Now: 2026-10-06 07:23 Mon CST");
        assert_eq!(sent_at.as_deref(), Some("2026-10-06 07:23"));
        assert_eq!(body, "");
    }

    #[test]
    fn derive_title_basic_and_slash() {
        assert_eq!(derive_title("  hello   world \nsecond").as_deref(), Some("hello world"));
        assert_eq!(derive_title("/new"), None);
        assert_eq!(derive_title("  /model gpt"), None);
        assert_eq!(derive_title("   \n  "), None);
    }

    #[test]
    fn derive_title_removes_file_markers() {
        assert_eq!(
            derive_title("[file:/tmp/a.pdf]\n总结这个文件").as_deref(),
            Some("总结这个文件")
        );
        assert_eq!(
            derive_title("see [file: report.pdf] please").as_deref(),
            Some("see please")
        );
        assert_eq!(derive_title("[file:/tmp/a.png]"), None);
        assert_eq!(derive_title("[note] keep").as_deref(), Some("[note] keep"));
    }

    #[test]
    fn derive_title_truncates_cjk_by_chars() {
        let long: String = "中".repeat(50);
        let title = derive_title(&long).expect("title");
        assert_eq!(title.chars().count(), AUTO_TITLE_MAX_CHARS);
        assert!(title.ends_with('…'));
        let exact: String = "字".repeat(AUTO_TITLE_MAX_CHARS);
        assert_eq!(derive_title(&exact).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn derive_title_from_stripped_prefix() {
        let text = "Now: 2026-10-06 07:23 Mon CST\n[Session started: 2026-10-06 07:23 Monday, CST, via feishu]\n明天北京天气怎么样？";
        let (_, body) = strip_injected_context(text);
        assert_eq!(derive_title(body).as_deref(), Some("明天北京天气怎么样？"));
    }

    #[test]
    fn sanitize_llm_title_cleans_output() {
        assert_eq!(sanitize_llm_title("\"北京天气查询。\"").as_deref(), Some("北京天气查询"));
        assert_eq!(
            sanitize_llm_title("Title: Rust borrow checker help.").as_deref(),
            Some("Rust borrow checker help")
        );
        assert_eq!(
            sanitize_llm_title("<think>hmm</think>\n「周报整理」").as_deref(),
            Some("周报整理")
        );
        assert_eq!(sanitize_llm_title("  \n "), None);
    }

    #[test]
    fn match_snippet_is_case_insensitive_and_cjk_safe() {
        let text = format!("{}找到关键词Rust了{}", "前".repeat(40), "后".repeat(40));
        let snip = match_snippet(&text, "rust", 60).expect("hit");
        assert!(snip.contains("Rust"));
        assert!(snip.starts_with('…') && snip.ends_with('…'));
        assert!(snip.chars().count() <= 62);
        assert!(match_snippet("hello", "xyz", 60).is_none());
        assert_eq!(match_snippet("Hello World", "WORLD", 60).as_deref(), Some("Hello World"));
    }

    #[test]
    fn message_text_handles_parts() {
        let v = serde_json::json!({"role":"user","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]});
        assert_eq!(message_text(&v), "a\nb");
        let v = serde_json::json!({"role":"user","content":"x"});
        assert_eq!(message_text(&v), "x");
        assert_eq!(message_text(&serde_json::json!({})), "");
    }

    #[test]
    fn truncate_and_ellipsize_chars() {
        assert_eq!(truncate_chars("中文字符", 2), "中文");
        assert_eq!(truncate_chars("ab", 5), "ab");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        assert_eq!(ellipsize("abc", 4), "abc");
    }
}
