//! Line-based text utilities used by the artifact preview pipeline.
//!
//! ANSI escape stripping, line normalisation, head/tail summarisation.
//! Char-count approximation (not grapheme-aware) — good enough for
//! line-based summarisation.

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

/// Keep first `head` and last `tail` lines, with a single omission marker
/// in between. Returns the input unchanged when nothing would be omitted.
pub fn head_tail(lines: &[String], head: u32, tail: u32) -> Vec<String> {
    let head = head as usize;
    let tail = tail as usize;
    if head == 0 && tail == 0 {
        return lines.to_vec();
    }
    // Require at least 3 omitted lines before we bother with a marker.
    // Smaller savings produce noise like `... 1 lines omitted ...` whose
    // text overhead is longer than the row it replaced.
    if lines.len() <= head + tail + 3 {
        return lines.to_vec();
    }
    let omitted = lines.len() - head - tail;
    let mut out = Vec::with_capacity(head + 1 + tail);
    out.extend_from_slice(&lines[..head]);
    out.push(format!("... {omitted} lines omitted ..."));
    out.extend_from_slice(&lines[lines.len() - tail..]);
    out
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
}
