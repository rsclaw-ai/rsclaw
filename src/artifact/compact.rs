//! Unified tool-result compaction pipeline.
//!
//! Every tool path that produces large output funnels through here. The
//! algorithm is the same regardless of tool: write full output to an
//! artifact, return an envelope with a head+tail preview and the artifact id.
//!
//! Compared with the per-tool rule library this replaces: simpler, lossless
//! (full data preserved), schema-stable across tools, zero maintenance.

use serde_json::{json, Value};

use crate::artifact::store::ArtifactStore;
use crate::artifact::text::{head_tail_with_marker, normalize_lines, strip_ansi};

/// Tool output below this many chars is returned verbatim — compaction noise
/// (envelope fields, omission marker) costs more than the savings.
pub const ARTIFACT_THRESHOLD_CHARS: usize = 4_000;

/// Preview shape sent back to the LLM in place of the full payload.
///
/// `head_lines` + `tail_lines` are the line-based summary; `max_chars` is
/// the hard char cap that prevents pathological inputs (one giant line,
/// long unwrapped paragraphs) from inflating the preview beyond budget.
/// The cap is also our LLM-token budget proxy: 25k chars ≈ 10k tokens for
/// English, ≈ 15k tokens for CJK — both safely below any practical
/// per-tool-result context budget.
#[derive(Debug, Clone, Copy)]
pub struct PreviewBudget {
    pub head_lines: u32,
    pub tail_lines: u32,
    pub max_chars: usize,
}

impl PreviewBudget {
    /// Default for exec / git / json / most tools. Small preview, LLM
    /// calls `read_artifact` when it wants more.
    pub const DEFAULT: Self = Self {
        head_lines: 40,
        tail_lines: 20,
        max_chars: 2_400,
    };

    /// Wider preview for web_fetch / web_browser — articles follow an
    /// inverted-pyramid structure (lede + structure first, refs/footer
    /// last) so a fat head pays off, and an agent typically wants enough
    /// context to answer without a second tool call.
    pub const WEB: Self = Self {
        head_lines: 200,
        tail_lines: 40,
        max_chars: 25_000,
    };
}

impl Default for PreviewBudget {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Compact a single text payload. Returns the rewritten text and (when
/// artifact was written) the new id.
///
/// Behavior:
/// - tiny input (≤ `ARTIFACT_THRESHOLD_CHARS`): pass through, no artifact
/// - large input: write full text to artifact (no on-disk size cap),
///   return head+tail preview with a marker pointing at the artifact id
pub fn compact_text(
    store: &ArtifactStore,
    session_key: &str,
    text: &str,
    budget: PreviewBudget,
) -> (String, Option<String>) {
    if text.chars().count() <= ARTIFACT_THRESHOLD_CHARS {
        return (text.to_owned(), None);
    }
    let id = match store.write(session_key, text) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "artifact store write failed; returning raw text");
            return (text.to_owned(), None);
        }
    };
    let lines = normalize_lines(&strip_ansi(text));
    // Build the preview with the artifact-aware marker in one pass.
    // Previously this called head_tail() and then `replacen`'d the
    // generic marker — fragile, because content lines that happened
    // to match the literal `"... N lines omitted ..."` (or that got
    // mangled by strip_ansi) made the replacement silently no-op and
    // the LLM never saw the read_artifact handle.
    let id_str = id.as_str().to_owned();
    let kept = head_tail_with_marker(
        &lines,
        budget.head_lines,
        budget.tail_lines,
        |omitted| {
            format!(
                "... {omitted} lines omitted — call read_artifact(tool_result_id=\"{id_str}\") for full output ..."
            )
        },
    );
    let preview = kept.join("\n");

    // Char-cap fallback. Two cases this protects against:
    //   1. input is one giant line (minified JSON, base64) — line-based
    //      head/tail can't shrink it
    //   2. lots of long unwrapped lines (e.g. WEB budget = 240 lines, but
    //      a wrapped article may run 300 chars/line → 72k chars preview)
    // Without this we'd exceed the LLM token budget per tool result.
    let preview = char_cap(&preview, budget.max_chars, id.as_str());
    (preview, Some(id.0))
}

fn char_cap(s: &str, max: usize, id: &str) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    // Marker is fixed-template; reserve its chars from the budget so the
    // returned string respects `max`.
    let marker = format!(
        "\n... output truncated — call read_artifact(tool_result_id=\"{id}\") for full output ...\n"
    );
    let marker_len = marker.chars().count();
    let body = max.saturating_sub(marker_len);
    let head_n = body * 7 / 10;
    let tail_n = body - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().rev().take(tail_n).collect::<String>().chars().rev().collect();
    format!("{head}{marker}{tail}")
}

/// Compact a JSON tool_result `Value`. Walks the value, replacing any large
/// string field (`stdout`, `stderr`, `text`, `content`, `messages_text`,
/// `output`) with a compacted preview and attaching `_tool_result_id` +
/// `_raw_chars` metadata to the outer object.
///
/// For non-object roots (rare), serializes the whole thing to JSON text and
/// compacts that.
pub fn compact_value(
    store: &ArtifactStore,
    session_key: &str,
    mut value: Value,
    budget: PreviewBudget,
) -> Value {
    // Object root: compact each known-heavy field individually so the JSON
    // schema stays stable. Only fields we know are text-shaped.
    if let Value::Object(map) = &mut value {
        let heavy_keys = ["stdout", "stderr", "text", "content", "messages_text", "output"];
        let mut any_compacted = false;
        let mut compacted_raw: usize = 0;
        let mut artifact_ids: Vec<(String, String)> = Vec::new();
        for k in heavy_keys.iter() {
            if let Some(field) = map.get(*k) {
                if let Some(s) = field.as_str() {
                    let raw = s.chars().count();
                    if raw > ARTIFACT_THRESHOLD_CHARS {
                        // `_raw_chars` only counts fields that crossed the
                        // threshold — a 5KB stdout + a 10-char text shouldn't
                        // report 5010 raw chars when only stdout was written.
                        compacted_raw += raw;
                        let (preview, id) = compact_text(store, session_key, s, budget);
                        if let Some(id) = id {
                            artifact_ids.push((k.to_string(), id));
                            any_compacted = true;
                        }
                        map.insert(k.to_string(), Value::String(preview));
                    }
                }
            }
        }
        if any_compacted {
            // Attach metadata. If multiple fields were compacted, expose
            // each id alongside the field name it represents.
            if artifact_ids.len() == 1 {
                map.insert(
                    "_tool_result_id".to_string(),
                    Value::String(artifact_ids[0].1.clone()),
                );
            } else {
                let mut by_field = serde_json::Map::new();
                for (field, id) in &artifact_ids {
                    by_field.insert(field.clone(), Value::String(id.clone()));
                }
                map.insert(
                    "_tool_result_ids".to_string(),
                    Value::Object(by_field),
                );
            }
            map.insert("_truncated".to_string(), Value::Bool(true));
            map.insert(
                "_raw_chars".to_string(),
                Value::Number(serde_json::Number::from(compacted_raw)),
            );
            map.insert(
                "_hint".to_string(),
                Value::String(
                    "Output exceeded inline budget. Use read_artifact(tool_result_id=..., mode=full|head:N|tail:N|lines:A-B|grep:PAT) to see full content.".to_string()
                ),
            );
        }
        return Value::Object(std::mem::take(map));
    }

    // String root (some tools return a bare string): treat as text.
    if let Value::String(s) = &value {
        if s.chars().count() > ARTIFACT_THRESHOLD_CHARS {
            let (preview, id) = compact_text(store, session_key, s, budget);
            return match id {
                Some(id) => json!({
                    "text": preview,
                    "_tool_result_id": id,
                    "_truncated": true,
                    "_raw_chars": s.chars().count(),
                    "_hint": "Use read_artifact to see full content.",
                }),
                None => Value::String(preview),
            };
        }
    }

    // Other shapes (array, number, bool, null) pass through.
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> (tempfile::TempDir, ArtifactStore) {
        let tmp = tempdir().unwrap();
        let s = ArtifactStore::at(tmp.path().to_path_buf());
        (tmp, s)
    }

    #[test]
    fn tiny_text_passes_through() {
        let (_t, s) = store();
        let (out, id) = compact_text(&s, "sess", "small", PreviewBudget::DEFAULT);
        assert_eq!(out, "small");
        assert!(id.is_none());
    }

    #[test]
    fn large_text_gets_artifact_and_preview() {
        let (_t, s) = store();
        let big = (1..=500).map(|i| format!("line_{i}")).collect::<Vec<_>>().join("\n");
        let (preview, id) = compact_text(&s, "sess", &big, PreviewBudget::DEFAULT);
        let id = id.expect("artifact written");
        assert!(preview.contains("line_1"));
        assert!(preview.contains("line_500"));
        assert!(preview.contains("read_artifact"));
        assert!(preview.contains(&id));
        // Full text recoverable from store.
        let full = s.read("sess", &crate::artifact::ArtifactId(id)).unwrap();
        assert_eq!(full, big);
    }

    #[test]
    fn web_budget_keeps_more_lines_than_default() {
        let (_t, s) = store();
        // 500 short lines: default keeps 40+20=60, web keeps 200+40=240.
        let big = (1..=500).map(|i| format!("line_{i:03}")).collect::<Vec<_>>().join("\n");
        let (default_preview, _) = compact_text(&s, "sess", &big, PreviewBudget::DEFAULT);
        let (web_preview, _) = compact_text(&s, "sess", &big, PreviewBudget::WEB);
        let default_lines = default_preview.lines().count();
        let web_lines = web_preview.lines().count();
        assert!(
            web_lines > default_lines + 100,
            "web should keep many more lines: default={default_lines}, web={web_lines}"
        );
        // Both still mention the artifact id.
        assert!(default_preview.contains("read_artifact"));
        assert!(web_preview.contains("read_artifact"));
    }

    #[test]
    fn web_preview_respects_max_chars_cap() {
        let (_t, s) = store();
        // 500 unwrapped lines × 200 chars each = ~100KB — way over the web
        // 25k char cap. line-based head/tail alone would produce 240 × 200
        // = ~48KB; the char cap must clip it.
        let big = (1..=500)
            .map(|i| format!("{i:04}: {}", "x".repeat(196)))
            .collect::<Vec<_>>()
            .join("\n");
        let (preview, id) = compact_text(&s, "sess", &big, PreviewBudget::WEB);
        assert!(id.is_some());
        let preview_chars = preview.chars().count();
        assert!(
            preview_chars <= PreviewBudget::WEB.max_chars + 50,
            "preview {preview_chars} chars exceeded WEB cap {}",
            PreviewBudget::WEB.max_chars
        );
    }

    #[test]
    fn artifact_on_disk_has_no_size_cap() {
        let (_t, s) = store();
        // 200KB of content — well over any preview budget. The artifact
        // file on disk must hold the FULL payload regardless of preview size.
        let big = "x".repeat(200_000);
        let (_preview, id) = compact_text(&s, "sess", &big, PreviewBudget::DEFAULT);
        let id = id.expect("artifact written");
        let full = s.read("sess", &crate::artifact::ArtifactId(id)).unwrap();
        assert_eq!(full.len(), 200_000);
    }

    #[test]
    fn json_object_compacts_stdout_field() {
        let (_t, s) = store();
        let big = "x".repeat(5_000);
        let value = json!({
            "exit_code": 0,
            "stdout": big,
            "stderr": "",
        });
        let out = compact_value(&s, "sess", value, PreviewBudget::DEFAULT);
        let obj = out.as_object().unwrap();
        assert_eq!(obj["_truncated"], json!(true));
        assert!(obj["_tool_result_id"].is_string());
        assert!(obj["stdout"].as_str().unwrap().len() < 5_000);
        assert_eq!(obj["exit_code"], json!(0));
    }

    #[test]
    fn json_object_small_passes_through() {
        let (_t, s) = store();
        let value = json!({"exit_code": 0, "stdout": "hi"});
        let out = compact_value(&s, "sess", value.clone(), PreviewBudget::DEFAULT);
        assert_eq!(out, value);
    }
}
