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
use crate::artifact::text::{head_tail, normalize_lines, strip_ansi};

/// Tool output below this many chars is returned verbatim — compaction noise
/// (envelope fields, omission marker) costs more than the savings.
pub const ARTIFACT_THRESHOLD_CHARS: usize = 4_000;

const PREVIEW_HEAD_LINES: u32 = 40;
const PREVIEW_TAIL_LINES: u32 = 20;

/// Hard cap on preview size in chars. Some tool outputs are one giant line
/// (minified JSON, base64, dumped DB rows) so line-based head/tail can't
/// shrink them — this is the fallback that ensures we never blow the budget.
const PREVIEW_MAX_CHARS: usize = 2_400;

/// Compact a single text payload. Returns the rewritten text and (when
/// artifact was written) the new id.
///
/// Behavior:
/// - tiny input (≤ `ARTIFACT_THRESHOLD_CHARS`): pass through, no artifact
/// - large input: write full text to artifact, return head+tail preview with
///   a marker pointing at the artifact id
pub fn compact_text(
    store: &ArtifactStore,
    session_key: &str,
    text: &str,
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
    let total = lines.len();
    let kept = head_tail(&lines, PREVIEW_HEAD_LINES, PREVIEW_TAIL_LINES);
    let preview = kept.join("\n");

    // Replace the line-omission marker with one that points at the artifact id.
    let omitted = total
        .saturating_sub(PREVIEW_HEAD_LINES as usize)
        .saturating_sub(PREVIEW_TAIL_LINES as usize);
    let preview = preview.replacen(
        &format!("... {omitted} lines omitted ..."),
        &format!(
            "... {omitted} lines omitted — call read_artifact(tool_result_id=\"{}\") for full output ...",
            id.as_str()
        ),
        1,
    );

    // Char-cap fallback: when input is one giant line, line-based head/tail
    // can't shrink it. Char-slice with explicit marker so we never exceed
    // the preview budget regardless of input shape.
    let preview = char_cap(&preview, PREVIEW_MAX_CHARS, id.as_str());
    (preview, Some(id.0))
}

fn char_cap(s: &str, max: usize, id: &str) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let head_n = max * 7 / 10;
    let tail_n = max - head_n;
    let head: String = s.chars().take(head_n).collect();
    let tail: String = s.chars().rev().take(tail_n).collect::<String>().chars().rev().collect();
    format!(
        "{head}\n... output truncated — call read_artifact(tool_result_id=\"{id}\") for full output ...\n{tail}"
    )
}

/// Compact a JSON tool_result `Value`. Walks the value, replacing any large
/// string field (`stdout`, `stderr`, `text`, `content`, `messages_text`)
/// with a compacted preview and attaching `_tool_result_id` + `_raw_chars`
/// metadata to the outer object.
///
/// For non-object roots (rare), serializes the whole thing to JSON text and
/// compacts that.
pub fn compact_value(store: &ArtifactStore, session_key: &str, mut value: Value) -> Value {
    // Object root: compact each known-heavy field individually so the JSON
    // schema stays stable. Only fields we know are text-shaped.
    if let Value::Object(map) = &mut value {
        let heavy_keys = ["stdout", "stderr", "text", "content", "messages_text", "output"];
        let mut any_compacted = false;
        let mut total_raw: usize = 0;
        let mut artifact_ids: Vec<(String, String)> = Vec::new();
        for k in heavy_keys.iter() {
            if let Some(field) = map.get(*k) {
                if let Some(s) = field.as_str() {
                    let raw = s.chars().count();
                    total_raw += raw;
                    if raw > ARTIFACT_THRESHOLD_CHARS {
                        let (preview, id) = compact_text(store, session_key, s);
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
                Value::Number(serde_json::Number::from(total_raw)),
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
            let (preview, id) = compact_text(store, session_key, s);
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
        let (out, id) = compact_text(&s, "sess", "small");
        assert_eq!(out, "small");
        assert!(id.is_none());
    }

    #[test]
    fn large_text_gets_artifact_and_preview() {
        let (_t, s) = store();
        let big = (1..=500).map(|i| format!("line_{i}")).collect::<Vec<_>>().join("\n");
        let (preview, id) = compact_text(&s, "sess", &big);
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
    fn json_object_compacts_stdout_field() {
        let (_t, s) = store();
        let big = "x".repeat(5_000);
        let value = json!({
            "exit_code": 0,
            "stdout": big,
            "stderr": "",
        });
        let out = compact_value(&s, "sess", value);
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
        let out = compact_value(&s, "sess", value.clone());
        assert_eq!(out, value);
    }
}
