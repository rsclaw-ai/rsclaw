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

use anyhow::{anyhow, Result};
use regex::RegexBuilder;
use serde_json::{json, Value};

use crate::artifact::{default_store, ArtifactId};

use super::runtime::{AgentRuntime, RunContext};

/// Apply `mode` to `full` text and return the selected slice.
///
/// Factored out from the tool handler so unit tests can hit the parser
/// without standing up a `RunContext`. Modes:
/// - `full`         — entire text (returns `full` clone)
/// - `stat`         — size summary only, no content (kept as `Ok("")` here;
///                    the handler attaches structured fields to the
///                    response Value)
/// - `head:N`       — first N lines (N=0 → empty)
/// - `tail:N`       — last N lines (N=0 → empty)
/// - `lines:A-B`    — 1-indexed inclusive range, clamped to `[1, total]`
/// - `grep:PATTERN` — case-insensitive regex over lines
pub(crate) fn apply_mode(full: &str, mode: &str) -> Result<String> {
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

impl AgentRuntime {
    pub(crate) async fn tool_read_artifact(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        let id_str = args["tool_result_id"]
            .as_str()
            .or_else(|| args["id"].as_str())
            .ok_or_else(|| anyhow!("read_artifact: `tool_result_id` required"))?;
        let id = ArtifactId::parse(id_str)?;

        let mode = args["mode"].as_str().unwrap_or("full");
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

        let mut out = json!({
            "tool_result_id": id.as_str(),
            "mode": mode,
            "total_lines": total_lines,
            "returned_chars": selected.chars().count(),
            "content": selected,
        });
        if mode == "stat" {
            // Cheap size summary so the LLM can decide whether to commit
            // to head/tail/lines/grep without paying for content.
            out["byte_size"] = json!(full.len());
            out["char_count"] = json!(full.chars().count());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        (1..=5).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn full_returns_everything() {
        assert_eq!(apply_mode(&sample(), "full").unwrap(), sample());
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
        assert_eq!(apply_mode(&sample(), "lines:2-4").unwrap(), "line2\nline3\nline4");
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
}
