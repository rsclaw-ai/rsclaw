//! Tool handler for `read_session_archive` — companion to `read_artifact`
//! but for the LLM's own conversation history rather than tool outputs.
//!
//! Every message ever appended to a session is mirrored under the
//! `archive:<session>:gen<n>:<seq>` prefix in redb and never deleted by
//! compaction. After a compaction the LLM sees `[head + summary + recent]`
//! and may need to dig back into the full pre-summary history — this tool
//! is how.
//!
//! Modes:
//! - `stat`         — totals + seq range + generations (zero content)
//! - `head:N`       — first N messages (oldest)
//! - `tail:N`       — last N messages (newest)
//! - `seq:A-B`      — 1-indexed inclusive seq range
//! - `grep:PAT`     — case-insensitive regex over each message's text
//!                    (substring works since literal patterns are valid
//!                    regex; alternation like `error|fail|warn` works)
//!
//! Large messages (> ARTIFACT_THRESHOLD_CHARS) get nested through the
//! artifact pipeline: each oversized hit is written to its own artifact
//! and returned with a `tool_result_id` instead of inline content. This
//! keeps the read_session_archive response itself bounded.

use anyhow::{anyhow, Result};
use regex::RegexBuilder;
use serde_json::{json, Value};

use crate::artifact::{compact_text, default_store, PreviewBudget, ARTIFACT_THRESHOLD_CHARS};

use super::runtime::{AgentRuntime, RunContext};

/// Per-mode cap on how many archive rows we return in one call. Bigger
/// modes (full grep results) get chopped here so the response itself
/// stays under reasonable token budget; LLM can re-call with a tighter
/// filter or specific seq range to drill down.
const ARCHIVE_RESULT_LIMIT: usize = 50;

/// Pull the human-readable text out of a message JSON. Tool calls /
/// tool_results may have structured content; we flatten to a string so
/// grep can match across all variants without LLM needing to know the
/// schema.
fn message_text(msg: &Value) -> String {
    if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
        return s.to_owned();
    }
    if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for p in parts {
            if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            } else if let Some(c) = p.get("content").and_then(|v| v.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(c);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    // Last resort — stringify the whole message so grep still has something to match.
    msg.to_string()
}

/// Render one archive entry as a result row. If its text payload is
/// large, nest it through the artifact pipeline so this response stays
/// bounded (per the artifact-style design).
fn render_entry(
    session_key: &str,
    seq: u64,
    generation: u32,
    msg: &Value,
) -> Value {
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let text = message_text(msg);
    let raw_chars = text.chars().count();

    if raw_chars > ARTIFACT_THRESHOLD_CHARS {
        // Nest through artifact pipeline — write full text to its own
        // artifact, return only a head/tail preview with a fresh
        // tool_result_id pointing at it. LLM can drill in via read_artifact.
        let (preview, id) = compact_text(default_store(), session_key, &text, PreviewBudget::DEFAULT);
        json!({
            "seq": seq,
            "generation": generation,
            "role": role,
            "content_preview": preview,
            "tool_result_id": id,
            "raw_chars": raw_chars,
        })
    } else {
        json!({
            "seq": seq,
            "generation": generation,
            "role": role,
            "content": text,
            "chars": raw_chars,
        })
    }
}

/// Apply `mode` to `rows` and return (selected_rows, optional_summary).
///
/// Pure function — factored out for unit tests so we can hit the parser
/// without standing up a redb instance.
pub(crate) fn apply_archive_mode(
    rows: &[(u64, u32, Value)],
    mode: &str,
) -> Result<(Vec<(u64, u32, Value)>, Option<String>)> {
    let total = rows.len();
    if mode == "stat" {
        return Ok((Vec::new(), Some("stat".to_string())));
    }
    if let Some(rest) = mode.strip_prefix("head:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_session_archive: bad head count `{rest}`"))?;
        return Ok((rows.iter().take(n).cloned().collect(), None));
    }
    if let Some(rest) = mode.strip_prefix("tail:") {
        let n: usize = rest
            .parse()
            .map_err(|_| anyhow!("read_session_archive: bad tail count `{rest}`"))?;
        let start = total.saturating_sub(n);
        return Ok((rows[start..].to_vec(), None));
    }
    if let Some(range) = mode.strip_prefix("seq:") {
        let (a, b) = range
            .split_once('-')
            .ok_or_else(|| anyhow!("read_session_archive: `seq:A-B` malformed: `{range}`"))?;
        let a: u64 = a
            .parse()
            .map_err(|_| anyhow!("read_session_archive: bad start seq `{a}`"))?;
        let b: u64 = b
            .parse()
            .map_err(|_| anyhow!("read_session_archive: bad end seq `{b}`"))?;
        if b < a {
            return Err(anyhow!(
                "read_session_archive: seq:A-B must satisfy A ≤ B, got {a}-{b}"
            ));
        }
        let selected: Vec<_> = rows
            .iter()
            .filter(|(s, _, _)| *s >= a && *s <= b)
            .cloned()
            .collect();
        return Ok((selected, None));
    }
    if let Some(pattern) = mode.strip_prefix("grep:") {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| anyhow!("read_session_archive: grep pattern invalid: {e}"))?;
        let matches: Vec<_> = rows
            .iter()
            .filter(|(_, _, m)| re.is_match(&message_text(m)))
            .take(ARCHIVE_RESULT_LIMIT)
            .cloned()
            .collect();
        return Ok((matches, None));
    }
    Err(anyhow!(
        "read_session_archive: unknown mode `{mode}`. Use stat | head:N | tail:N | seq:A-B | grep:PATTERN"
    ))
}

impl AgentRuntime {
    pub(crate) async fn tool_read_session_archive(
        &self,
        ctx: &RunContext,
        args: Value,
    ) -> Result<Value> {
        // Always operate on the caller's own session — never let the LLM
        // pass `session_key` to read a peer's archive. The argument is
        // intentionally absent from the tool schema.
        let session_key = ctx.session_key.clone();
        let mode = args["mode"].as_str().unwrap_or("stat");
        let generation = args["generation"].as_u64().map(|g| g as u32);

        let rows = self
            .store
            .db
            .archive_load(&session_key, generation)
            .map_err(|e| anyhow!("archive_load failed for `{session_key}`: {e}"))?;

        // `stat` is special — no row filtering, just summary numbers.
        if mode == "stat" {
            let stat = self
                .store
                .db
                .archive_stat(&session_key)
                .map_err(|e| anyhow!("archive_stat failed for `{session_key}`: {e}"))?;
            return Ok(json!({
                "session_key": session_key,
                "mode": "stat",
                "total_messages": stat.total_messages,
                "oldest_seq": stat.oldest_seq,
                "newest_seq": stat.newest_seq,
                "generations": stat.generations,
                "results": [],
            }));
        }

        let (selected, _summary) = apply_archive_mode(&rows, mode)?;
        let truncated = selected.len() >= ARCHIVE_RESULT_LIMIT
            && matches!(mode.strip_prefix("grep:"), Some(_));

        let results: Vec<Value> = selected
            .into_iter()
            .map(|(seq, generation, msg)| render_entry(&session_key, seq, generation, &msg))
            .collect();

        let mut out = json!({
            "session_key": session_key,
            "mode": mode,
            "total_archived": rows.len(),
            "returned": results.len(),
            "results": results,
        });
        if truncated {
            out["_truncated"] = json!(true);
            out["_hint"] = json!(format!(
                "grep returned the first {ARCHIVE_RESULT_LIMIT} matches; narrow the pattern or follow up with seq:A-B to scroll the rest."
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u64, role: &str, content: &str) -> (u64, u32, Value) {
        (
            seq,
            1,
            json!({ "role": role, "content": content }),
        )
    }

    fn sample() -> Vec<(u64, u32, Value)> {
        vec![
            row(1, "user", "hello agent"),
            row(2, "assistant", "Hi! How can I help today?"),
            row(3, "user", "what's the weather in Beijing?"),
            row(4, "assistant", "It's sunny, 18°C."),
            row(5, "user", "show me the recent error logs"),
            row(6, "assistant", "Found 3 errors and 2 warnings."),
            row(7, "user", "thanks"),
            row(8, "assistant", "you're welcome"),
        ]
    }

    #[test]
    fn head_takes_first_n() {
        let (out, _) = apply_archive_mode(&sample(), "head:2").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert_eq!(out[1].0, 2);
    }

    #[test]
    fn tail_takes_last_n() {
        let (out, _) = apply_archive_mode(&sample(), "tail:3").unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].0, 6);
        assert_eq!(out[2].0, 8);
    }

    #[test]
    fn tail_over_total_returns_all() {
        let (out, _) = apply_archive_mode(&sample(), "tail:99").unwrap();
        assert_eq!(out.len(), 8);
    }

    #[test]
    fn seq_range_inclusive() {
        let (out, _) = apply_archive_mode(&sample(), "seq:3-5").unwrap();
        assert_eq!(out.iter().map(|(s, _, _)| *s).collect::<Vec<_>>(), vec![3, 4, 5]);
    }

    #[test]
    fn seq_out_of_range_returns_empty_not_panic() {
        let (out, _) = apply_archive_mode(&sample(), "seq:100-200").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn seq_b_less_than_a_rejected() {
        assert!(apply_archive_mode(&sample(), "seq:5-3").is_err());
    }

    #[test]
    fn grep_substring_matches_case_insensitive() {
        let (out, _) = apply_archive_mode(&sample(), "grep:weather").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 3);
    }

    #[test]
    fn grep_alternation_works() {
        // "error|warning" should hit row 5 (mentions errors) AND row 6 (mentions warnings).
        let (out, _) = apply_archive_mode(&sample(), "grep:error|warning").unwrap();
        let seqs: Vec<u64> = out.iter().map(|(s, _, _)| *s).collect();
        assert!(seqs.contains(&5), "missing row 5: {seqs:?}");
        assert!(seqs.contains(&6), "missing row 6: {seqs:?}");
    }

    #[test]
    fn grep_bad_pattern_returns_error() {
        let err = apply_archive_mode(&sample(), "grep:[unclosed")
            .unwrap_err()
            .to_string();
        assert!(err.contains("grep pattern invalid"), "got: {err}");
    }

    #[test]
    fn stat_returns_no_rows() {
        let (out, summary) = apply_archive_mode(&sample(), "stat").unwrap();
        assert!(out.is_empty());
        assert_eq!(summary.as_deref(), Some("stat"));
    }

    #[test]
    fn unknown_mode_rejected() {
        let err = apply_archive_mode(&sample(), "fancy:thing")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown mode"), "got: {err}");
    }
}
