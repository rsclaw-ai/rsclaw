//! Tool handler for `read_artifact` — LLM-side companion to the artifact
//! store. When the runtime backstop replaces a large tool_result with a
//! preview + `tool_result_id`, the LLM uses this tool to fetch the full
//! content (or a slice of it).
//!
//! Modes:
//! - `full` (default) — return entire artifact text
//! - `head:N` — first N lines
//! - `tail:N` — last N lines
//! - `lines:A-B` — line range (1-indexed, inclusive)
//! - `grep:PATTERN` — lines matching regex (case-insensitive)

use anyhow::{Result, anyhow};
use regex::RegexBuilder;
use serde_json::{Value, json};

use super::runtime::{AgentRuntime, RunContext};
use crate::artifact::{ArtifactId, default_store};

/// Apply `mode` to `full` text and return the selected slice.
///
/// Factored out from the tool handler so unit tests can hit the parser
/// without standing up a `RunContext`. Modes:
/// - `full`         — entire text (returns `full` clone)
/// - `stat`         — size summary only, no content (kept as `Ok("")` here; the
///   handler attaches structured fields to the response Value)
/// - `head:N`       — first N lines (N=0 → empty)
/// - `tail:N`       — last N lines (N=0 → empty)
/// - `lines:A-B`    — 1-indexed inclusive range, clamped to `[1, total]`
/// - `grep:PATTERN` — case-insensitive regex over lines
pub(crate) fn apply_mode(full: &str, mode: &str) -> Result<String> {
    // Defensive trim against the v1 tool-call protocol's trailing-newline
    // leak (see read_session_archive::apply_archive_mode for the same guard).
    let mode = mode.trim();
    let lines: Vec<&str> = full.lines().collect();
    let total = lines.len();
    if mode == "full" {
        return Ok(full.to_owned());
    }
    if mode == "stat" {
        // Stat mode returns no content; the handler decorates the JSON
        // response with line/char/byte counts instead.
        return Ok(String::new());
    }
    if let Some(rest) = mode.strip_prefix("head:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad head count `{rest}`"))?;
        return Ok(lines.iter().take(n).copied().collect::<Vec<_>>().join("\n"));
    }
    if let Some(rest) = mode.strip_prefix("tail:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad tail count `{rest}`"))?;
        let start = total.saturating_sub(n);
        return Ok(lines[start..].join("\n"));
    }
    if let Some(range) = mode.strip_prefix("lines:") {
        let (a, b) = range
            .split_once('-')
            .ok_or_else(|| anyhow!("read_artifact: `lines:A-B` malformed: `{range}`"))?;
        let a: usize = a
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad start line `{a}`"))?;
        let b: usize = b
            .parse()
            .map_err(|_| anyhow!("read_artifact: bad end line `{b}`"))?;
        if a == 0 || b < a {
            return Err(anyhow!(
                "read_artifact: lines:A-B must satisfy 1 ≤ A ≤ B, got {a}-{b}"
            ));
        }
        // Clamp both endpoints so an LLM asking for lines:100-200 on a
        // 5-line file gets an empty slice instead of a panic.
        let lo = a.saturating_sub(1).min(total);
        let hi = b.min(total).max(lo);
        return Ok(lines[lo..hi].join("\n"));
    }
    if let Some(pattern) = mode.strip_prefix("grep:") {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| anyhow!("read_artifact: grep pattern invalid: {e}"))?;
        return Ok(lines
            .iter()
            .filter(|l| re.is_match(l))
            .copied()
            .collect::<Vec<_>>()
            .join("\n"));
    }
    Err(anyhow!(
        "read_artifact: unknown mode `{mode}`. Use full | head:N | tail:N | lines:A-B | grep:PATTERN"
    ))
}

/// Truncate `text` to the largest prefix of WHOLE lines whose estimated
/// token count stays within `budget`. Returns
/// `(page_text, lines_in_page, total_lines_in_text)`. When `text` already
/// fits, `page_text == text` and `lines_in_page == total`.
///
/// This is the per-turn pagination floor for `read_artifact`: a `mode=full`
/// (or any mode) result that would blow `max_per_turn_input_tokens` is
/// served one page at a time instead of dumped whole — lossless (the full
/// artifact stays on disk) and bounded (each page ≤ budget). The model
/// pages on via `lines:A-B` / `grep:`.
pub(crate) fn paginate_to_budget(text: &str, budget_tokens: usize) -> (String, usize, usize) {
    use crate::agent::context_mgr::estimate_tokens;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if estimate_tokens(text) <= budget_tokens {
        return (text.to_owned(), total, total);
    }
    let Some(&first) = lines.first() else {
        // Empty text already fits above; this is just defensive.
        return (String::new(), 0, 0);
    };
    // Pathological: the FIRST line alone busts the budget. Hard
    // char-truncate it into a single page so we never return an empty
    // or over-budget page. ~4 chars/token is the ASCII upper bound; CJK
    // comes in well under budget.
    if estimate_tokens(first) > budget_tokens {
        let cap_chars = budget_tokens.saturating_mul(4).max(1);
        let truncated: String = first.chars().take(cap_chars).collect();
        return (truncated, 1, total);
    }
    // First line fits — always include it, then greedily accumulate whole
    // lines until the next would bust the budget. Guarantees n ≥ 1.
    let mut acc = String::from(first);
    let mut used = estimate_tokens(first);
    let mut n = 1usize;
    for line in &lines[1..] {
        let line_tokens = estimate_tokens(line) + 1; // +1 for the rejoin '\n'
        if used + line_tokens > budget_tokens {
            break;
        }
        acc.push('\n');
        acc.push_str(line);
        used += line_tokens;
        n += 1;
    }
    (acc, n, total)
}

/// Split a tool-result `content` into `(body, trailing_handle_marker)`.
///
/// The runtime backstop appends a recovery marker to truncated tool
/// results, e.g. `"\n\n[truncated — call read_artifact(tool_result_id=
/// \"tr_…\") for full output]"`. The per-turn aggregate guard needs to
/// re-trim such results WITHOUT dropping that marker (dropping it would
/// turn lossless pagination into lossy truncation). Returns the body
/// before the marker plus the marker slice when one is present; otherwise
/// `(content, None)`.
pub(crate) fn split_artifact_marker(content: &str) -> (&str, Option<&str>) {
    if let Some(pos) = content.rfind("\n\n[") {
        let tail = &content[pos..];
        if tail.contains("read_artifact") {
            return (&content[..pos], Some(tail));
        }
    }
    (content, None)
}

/// 1-indexed artifact line where the `mode`-selected slice begins, when
/// the slice maps to a contiguous range (so a precise `lines:A-B` next-page
/// hint is possible). `None` for `grep` (non-contiguous) and `stat`.
fn selected_start_line(mode: &str, total_lines: usize) -> Option<usize> {
    let mode = mode.trim();
    if mode == "full" || mode.starts_with("head:") {
        return Some(1);
    }
    if let Some(rest) = mode.strip_prefix("lines:")
        && let Some((a, _)) = rest.split_once('-')
        && let Ok(a) = a.parse::<usize>()
    {
        return Some(a.max(1));
    }
    if let Some(rest) = mode.strip_prefix("tail:")
        && let Ok(n) = rest.parse::<usize>()
    {
        return Some(total_lines.saturating_sub(n) + 1);
    }
    None
}

impl AgentRuntime {
    pub(crate) async fn tool_read_artifact(&self, ctx: &RunContext, args: Value) -> Result<Value> {
        // Trim string args: the rsclaw v1 tool-call protocol leaks a trailing
        // newline into them (same root cause as read_session_archive `mode`
        // and the computer `action` arg). Untrimmed, `tool_result_id="tr_x\n"`
        // fails to resolve and `mode="grep:x\n"` matches nothing.
        let id_str = args["tool_result_id"]
            .as_str()
            .or_else(|| args["id"].as_str())
            .map(str::trim)
            .ok_or_else(|| anyhow!("read_artifact: `tool_result_id` required"))?;
        let id = ArtifactId::parse(id_str)?;

        let mode = args["mode"].as_str().unwrap_or("full").trim();
        let store = default_store();
        let full = store.read(&ctx.session_key, &id).map_err(|e| {
            anyhow!(
                "artifact `{}` not found in session `{}` ({e}). \
                 Sessions are independent — an id from another session won't resolve here.",
                id.as_str(),
                ctx.session_key
            )
        })?;

        let total_lines = full.lines().count();
        let selected = apply_mode(&full, mode)?;

        // stat returns no content — emit the size summary and return early
        // so pagination never touches it.
        if mode == "stat" {
            return Ok(json!({
                "tool_result_id": id.as_str(),
                "mode": mode,
                "total_lines": total_lines,
                "returned_chars": 0,
                "content": "",
                "byte_size": full.len(),
                "char_count": full.chars().count(),
            }));
        }

        // Per-turn input floor: a mode=full (or any) result that would blow
        // `max_per_turn_input_tokens` is paged instead of dumped whole.
        // Lossless (full artifact stays on disk) + bounded (page ≤ budget);
        // the model pages on via lines:A-B / grep:.
        let budget = self
            .live
            .agents
            .read()
            .await
            .defaults
            .max_per_turn_input_tokens
            .unwrap_or(5_000) as usize;
        let (page, page_lines, selected_lines) = paginate_to_budget(&selected, budget);
        let truncated = page_lines < selected_lines;

        let mut out = json!({
            "tool_result_id": id.as_str(),
            "mode": mode,
            "total_lines": total_lines,
            "returned_chars": page.chars().count(),
            "content": page,
        });
        if truncated {
            out["truncated"] = json!(true);
            out["returned_lines"] = json!(page_lines);
            out["selected_lines"] = json!(selected_lines);
            // Precise lines:A-B next-page hint when the slice is contiguous;
            // generic guidance for grep (non-contiguous).
            let next = match selected_start_line(mode, total_lines) {
                Some(start) => {
                    let next_start = start + page_lines;
                    format!(
                        "Returned lines {start}-{} of this slice (~{budget}-token page cap). \
                         Call read_artifact with mode=\"lines:{next_start}-{total_lines}\" for the \
                         next page, or grep:PATTERN to jump straight to the content you need.",
                        start + page_lines - 1
                    )
                }
                None => format!(
                    "Returned {page_lines} of {selected_lines} matching lines (~{budget}-token \
                     page cap). Narrow the grep:PATTERN or request a specific lines:A-B range."
                ),
            };
            out["next"] = json!(next);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        (1..=5)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn full_returns_everything() {
        assert_eq!(apply_mode(&sample(), "full").unwrap(), sample());
    }

    // Regression: v1 tool-call protocol leaks a trailing newline into the
    // mode arg; untrimmed it broke exact-match and grep regexes.
    #[test]
    fn mode_tolerates_trailing_newline() {
        assert_eq!(apply_mode(&sample(), "full\n").unwrap(), sample());
        assert_eq!(apply_mode(&sample(), "head:2\n").unwrap(), "line1\nline2");
        assert_eq!(apply_mode(&sample(), "grep:line3\n").unwrap(), "line3");
    }

    #[test]
    fn head_n_takes_first_n_lines() {
        assert_eq!(apply_mode(&sample(), "head:2").unwrap(), "line1\nline2");
    }

    #[test]
    fn head_zero_returns_empty() {
        assert_eq!(apply_mode(&sample(), "head:0").unwrap(), "");
    }

    #[test]
    fn tail_n_takes_last_n_lines() {
        assert_eq!(apply_mode(&sample(), "tail:2").unwrap(), "line4\nline5");
    }

    #[test]
    fn tail_over_total_returns_all() {
        assert_eq!(apply_mode(&sample(), "tail:99").unwrap(), sample());
    }

    #[test]
    fn lines_range_inclusive_one_indexed() {
        assert_eq!(
            apply_mode(&sample(), "lines:2-4").unwrap(),
            "line2\nline3\nline4"
        );
    }

    #[test]
    fn lines_out_of_range_clamps_no_panic() {
        // Regression: a=100, total=5 used to panic on `lines[99..5]` (start > end).
        let out = apply_mode(&sample(), "lines:100-200").unwrap();
        assert_eq!(out, "");
        let out = apply_mode(&sample(), "lines:3-200").unwrap();
        assert_eq!(out, "line3\nline4\nline5");
    }

    #[test]
    fn lines_invalid_ranges_rejected() {
        assert!(apply_mode(&sample(), "lines:0-3").is_err());
        assert!(apply_mode(&sample(), "lines:5-3").is_err());
        assert!(apply_mode(&sample(), "lines:abc").is_err());
    }

    #[test]
    fn grep_filters_case_insensitive() {
        let body = "INFO ok\nERROR bad\ninfo also ok\nWARN meh";
        let out = apply_mode(body, "grep:error").unwrap();
        assert_eq!(out, "ERROR bad");
        let out = apply_mode(body, "grep:^info").unwrap();
        assert_eq!(out, "INFO ok\ninfo also ok");
    }

    #[test]
    fn unknown_mode_rejected() {
        let err = apply_mode("x", "weirdo").unwrap_err().to_string();
        assert!(err.contains("unknown mode"), "got: {err}");
    }

    // -------- pagination (max_per_turn_input_tokens floor) --------

    #[test]
    fn paginate_under_budget_returns_whole() {
        let text = sample(); // 5 short lines, well under any real budget
        let (page, n, total) = paginate_to_budget(&text, 5_000);
        assert_eq!(page, text);
        assert_eq!(n, 5);
        assert_eq!(total, 5);
    }

    #[test]
    fn paginate_over_budget_returns_whole_lines_only() {
        // 200 lines of ~10 ASCII tokens each (~40 chars) ≈ 2000 tokens total.
        // A 100-token budget should return the first handful of WHOLE lines.
        let text = (1..=200)
            .map(|i| format!("line{i} aaaa bbbb cccc dddd eeee ffff gggg"))
            .collect::<Vec<_>>()
            .join("\n");
        let total_lines = text.lines().count();
        let (page, n, total) = paginate_to_budget(&text, 100);
        assert_eq!(total, total_lines);
        assert!(n > 0 && n < total_lines, "expected a partial page, got {n}/{total_lines}");
        // Page must be a whole-line prefix (no mid-line cut) and within budget.
        assert!(text.starts_with(&page));
        assert!(page.ends_with(|c: char| c != '\n'));
        assert!(
            crate::agent::context_mgr::estimate_tokens(&page) <= 100 + 20,
            "page should be ~within budget"
        );
    }

    #[test]
    fn paginate_single_giant_line_hard_truncates() {
        // One line that alone busts the budget — must still return exactly
        // one (char-truncated) line rather than an empty or over-budget page.
        let giant = "x".repeat(100_000); // ~25k tokens on one line
        let (page, n, _total) = paginate_to_budget(&giant, 50);
        assert_eq!(n, 1);
        assert!(page.len() < giant.len(), "giant line must be truncated");
        assert!(!page.is_empty());
    }

    #[test]
    fn split_artifact_marker_extracts_trailing_handle() {
        let content = "some preview body\nline 2\n\n[truncated — call read_artifact(tool_result_id=\"tr_abc\") for full output]";
        let (body, marker) = split_artifact_marker(content);
        assert_eq!(body, "some preview body\nline 2");
        assert!(marker.unwrap().contains("read_artifact"));
        assert!(marker.unwrap().contains("tr_abc"));
    }

    #[test]
    fn split_artifact_marker_none_when_no_handle() {
        let content = "plain tool result with no artifact handle\n\n[just a note]";
        let (body, marker) = split_artifact_marker(content);
        // The trailing bracket block doesn't mention read_artifact → not a handle.
        assert_eq!(body, content);
        assert!(marker.is_none());
    }

    #[test]
    fn split_artifact_marker_plain_text() {
        let (body, marker) = split_artifact_marker("just some output");
        assert_eq!(body, "just some output");
        assert!(marker.is_none());
    }

    #[test]
    fn selected_start_line_maps_contiguous_modes() {
        assert_eq!(selected_start_line("full", 100), Some(1));
        assert_eq!(selected_start_line("head:20", 100), Some(1));
        assert_eq!(selected_start_line("lines:30-90", 100), Some(30));
        assert_eq!(selected_start_line("tail:10", 100), Some(91));
        // grep / stat are non-contiguous → no precise next-range.
        assert_eq!(selected_start_line("grep:foo", 100), None);
        assert_eq!(selected_start_line("stat", 100), None);
    }
}
