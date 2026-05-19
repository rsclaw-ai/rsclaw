//! Line-based text utilities used by the artifact preview pipeline.
//!
//! ANSI escape stripping, line normalisation, adjacent dedup, head/tail
//! summarisation, simple pluralisation. Char-count approximation (not
//! grapheme-aware) — good enough for line-based summarisation.

use std::sync::LazyLock;

use regex::Regex;

// ---- ANSI escape stripping --------------------------------------------------

static ANSI_CSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B\[[0-?]*[ -/]*[@-~]").expect("ansi csi"));
static ANSI_OSC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B\][^\x07\x1B]*(?:\x07|\x1B\\)").expect("ansi osc"));
static ANSI_CSI_INCOMPLETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B\[[0-?]*[ -/]*$").expect("ansi csi incomplete"));
static ANSI_OSC_INCOMPLETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B\][^\x07\x1B]*$").expect("ansi osc incomplete"));
static ANSI_SINGLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B[@-_]").expect("ansi single"));

/// Strip ANSI escape sequences (CSI, OSC, single-byte introducers).
pub fn strip_ansi(text: &str) -> String {
    let s = ANSI_OSC.replace_all(text, "");
    let s = ANSI_CSI.replace_all(&s, "");
    let s = ANSI_OSC_INCOMPLETE.replace_all(&s, "");
    let s = ANSI_CSI_INCOMPLETE.replace_all(&s, "");
    let s = ANSI_SINGLE.replace_all(&s, "");
    s.replace('\x1B', "")
}

// ---- Line normalisation -----------------------------------------------------

/// Normalise CRLF to LF and trim trailing whitespace per line.
pub fn normalize_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .split('\n')
        .map(|line| line.trim_end().to_owned())
        .collect()
}

pub fn trim_empty_edges(lines: &[String]) -> Vec<String> {
    let mut start = 0;
    let mut end = lines.len();
    while start < end && lines[start].trim().is_empty() {
        start += 1;
    }
    while end > start && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines[start..end].to_vec()
}

pub fn dedupe_adjacent(lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        if out.last().map(|s| s.as_str()) != Some(line.as_str()) {
            out.push(line.clone());
        }
    }
    out
}

/// Keep first `head` and last `tail` lines, with a single omission marker
/// in between. Returns the input unchanged when nothing would be omitted.
pub fn head_tail(lines: &[String], head: u32, tail: u32) -> Vec<String> {
    let head = head as usize;
    let tail = tail as usize;
    if head == 0 && tail == 0 {
        return lines.to_vec();
    }
    if lines.len() <= head + tail {
        return lines.to_vec();
    }
    let omitted = lines.len() - head - tail;
    let mut out = Vec::with_capacity(head + 1 + tail);
    out.extend_from_slice(&lines[..head]);
    out.push(format!("... {omitted} lines omitted ..."));
    out.extend_from_slice(&lines[lines.len() - tail..]);
    out
}

// ---- Pluralisation ----------------------------------------------------------

/// English pluralisation matching upstream behaviour (count + noun) with
/// `passed/failed/skipped` suffix exemption.
pub fn pluralize(count: u32, noun: &str) -> String {
    if noun.ends_with("passed") || noun.ends_with("failed") || noun.ends_with("skipped") {
        return format!("{count} {noun}");
    }
    if count == 1 {
        return format!("{count} {noun}");
    }
    let ends_with_sxz = noun
        .chars()
        .last()
        .map(|c| matches!(c, 's' | 'x' | 'z'))
        .unwrap_or(false);
    let ends_with_sh_or_ch = noun.ends_with("sh") || noun.ends_with("ch");
    if ends_with_sxz || ends_with_sh_or_ch {
        return format!("{count} {noun}es");
    }
    if noun.ends_with('y') {
        let prev = noun.chars().rev().nth(1);
        let prev_is_consonant = prev.map(|c| !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')).unwrap_or(false);
        if prev_is_consonant {
            return format!("{count} {}ies", &noun[..noun.len() - 1]);
        }
    }
    format!("{count} {noun}s")
}

// ---- Char count -------------------------------------------------------------

pub fn count_text_chars(text: &str) -> usize {
    text.chars().count()
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_strip() {
        assert_eq!(strip_ansi("\x1B[31mred\x1B[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn normalize_crlf() {
        let v = normalize_lines("a\r\nb \r\nc");
        assert_eq!(v, vec!["a", "b", "c"]);
    }

    #[test]
    fn dedupe_keeps_first() {
        let v = dedupe_adjacent(&["a", "a", "b", "b", "a"].iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(v, vec!["a", "b", "a"]);
    }

    #[test]
    fn head_tail_omits() {
        let lines: Vec<String> = (1..=10).map(|i| i.to_string()).collect();
        let out = head_tail(&lines, 2, 2);
        assert_eq!(out, vec!["1", "2", "... 6 lines omitted ...", "9", "10"]);
    }

    #[test]
    fn head_tail_passthrough() {
        let lines: Vec<String> = (1..=4).map(|i| i.to_string()).collect();
        let out = head_tail(&lines, 2, 2);
        assert_eq!(out, lines);
    }

    #[test]
    fn pluralize_basics() {
        assert_eq!(pluralize(1, "file"), "1 file");
        assert_eq!(pluralize(3, "file"), "3 files");
        assert_eq!(pluralize(2, "match"), "2 matches");
        assert_eq!(pluralize(2, "error"), "2 errors");
        assert_eq!(pluralize(2, "entry"), "2 entries");
        assert_eq!(pluralize(2, "passed"), "2 passed");
    }
}
