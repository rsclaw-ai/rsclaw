//! Convert [`FullTrace`] records into ShareGPT-style JSONL for SFT training.
//!
//! Output schema (one JSON object per line):
//!
//! ```jsonc
//! {
//!   "trace_id": "...",
//!   "timestamp": 1700000000,
//!   "model": "claude-opus-4-7",
//!   "tools": [ ... OpenAI-style function array ... ],
//!   "conversations": [
//!     { "from": "system", "value": "..." },
//!     { "from": "human",  "value": "..." },
//!     { "from": "gpt",    "value": "<think>...</think>" },
//!     { "from": "function_call",
//!       "value": { "name": "...", "arguments": { ... }, "id": "call_1" } },
//!     { "from": "observation",
//!       "value": "...", "tool_call_id": "call_1", "is_error": false },
//!     { "from": "gpt",    "value": "..." }
//!   ]
//! }
//! ```
//!
//! Compatible with LLaMA-Factory and ms-swift sharegpt readers; function_call
//! and observation roles are the de-facto extension for tool-use SFT data.
//!
//! Out of scope here (handled in later modules): PII scrubbing, quality
//! gating, dedup. This module is a pure structural converter.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::agent::trace_capture::{FullTrace, TraceStep};

/// Convert one [`FullTrace`] into a ShareGPT JSON object.
///
/// Preserves step order. Each [`TraceStep`] maps to a single conversation
/// entry; an extra leading `system` entry carries the trace's system prompt
/// when non-empty.
pub fn trace_to_sharegpt(trace: &FullTrace) -> Value {
    let mut conversations: Vec<Value> = Vec::with_capacity(trace.steps.len() + 1);
    if !trace.system_prompt.is_empty() {
        conversations.push(json!({
            "from": "system",
            "value": trace.system_prompt,
        }));
    }
    for step in &trace.steps {
        conversations.push(step_to_entry(step));
    }
    json!({
        "trace_id": trace.trace_id,
        "timestamp": trace.timestamp,
        "model": trace.model,
        "tools": trace.tools_schema,
        "conversations": conversations,
    })
}

fn step_to_entry(step: &TraceStep) -> Value {
    match step {
        TraceStep::User { content } => json!({
            "from": "human",
            "value": content,
        }),
        TraceStep::AssistantThinking { content } => json!({
            "from": "gpt",
            "value": format!("<think>{content}</think>"),
        }),
        TraceStep::AssistantToolCall {
            name,
            args,
            call_id,
        } => json!({
            "from": "function_call",
            "value": {
                "name": name,
                "arguments": args,
                "id": call_id,
            },
        }),
        TraceStep::ToolResult {
            call_id,
            content,
            is_error,
        } => json!({
            "from": "observation",
            "value": content,
            "tool_call_id": call_id,
            "is_error": is_error,
        }),
        TraceStep::AssistantText { content } => json!({
            "from": "gpt",
            "value": content,
        }),
    }
}

/// Append a batch of traces to a JSONL file, one trace per line. Creates
/// the file if missing; existing content is preserved (caller controls
/// rotation and sharding).
pub fn write_sharegpt_jsonl(path: &Path, traces: &[FullTrace]) -> Result<()> {
    let file = File::options()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open jsonl file for append: {}", path.display()))?;
    let mut w = BufWriter::new(file);
    for trace in traces {
        let line = serde_json::to_string(&trace_to_sharegpt(trace))
            .with_context(|| format!("serialize trace {}", trace.trace_id))?;
        w.write_all(line.as_bytes())
            .with_context(|| format!("write trace {}", trace.trace_id))?;
        w.write_all(b"\n")
            .with_context(|| format!("write newline after trace {}", trace.trace_id))?;
    }
    w.flush().context("flush jsonl buffer")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::NamedTempFile;

    fn full_fixture() -> FullTrace {
        let mut t = FullTrace::new(
            "t1",
            "claude-opus-4-7",
            "you are rsclaw",
            json!([{"name": "weather"}]),
        );
        t.push_user("北京天气如何");
        t.push_thinking("need to call weather tool");
        t.push_tool_call("weather", json!({"city": "北京"}), "call_1");
        t.push_tool_result("call_1", r#"{"temp": 22}"#, false);
        t.push_assistant_text("北京 22 度");
        t
    }

    #[test]
    fn includes_system_prompt_when_present() {
        let t = full_fixture();
        let v = trace_to_sharegpt(&t);
        let conv = v["conversations"].as_array().expect("array");
        assert_eq!(conv[0]["from"], "system");
        assert_eq!(conv[0]["value"], "you are rsclaw");
    }

    #[test]
    fn omits_system_entry_when_empty() {
        let mut t = FullTrace::new("t2", "m", "", json!([]));
        t.push_user("hi");
        let v = trace_to_sharegpt(&t);
        let conv = v["conversations"].as_array().expect("array");
        assert_eq!(conv[0]["from"], "human");
    }

    #[test]
    fn thinking_wrapped_with_think_tags() {
        let t = full_fixture();
        let v = trace_to_sharegpt(&t);
        let conv = v["conversations"].as_array().expect("array");
        let thinking = conv
            .iter()
            .find(|e| e["value"].as_str().is_some_and(|s| s.contains("<think>")))
            .expect("thinking entry");
        assert_eq!(thinking["from"], "gpt");
        assert_eq!(thinking["value"], "<think>need to call weather tool</think>");
    }

    #[test]
    fn tool_call_preserves_raw_args() {
        let t = full_fixture();
        let v = trace_to_sharegpt(&t);
        let conv = v["conversations"].as_array().expect("array");
        let fc = conv
            .iter()
            .find(|e| e["from"] == "function_call")
            .expect("function_call entry");
        assert_eq!(fc["value"]["name"], "weather");
        assert_eq!(fc["value"]["arguments"], json!({"city": "北京"}));
        assert_eq!(fc["value"]["id"], "call_1");
    }

    #[test]
    fn observation_carries_call_id_and_error_flag() {
        let mut t = FullTrace::new("t3", "m", "", json!([]));
        t.push_tool_result("c9", "denied", true);
        let v = trace_to_sharegpt(&t);
        let conv = v["conversations"].as_array().expect("array");
        let obs = &conv[0];
        assert_eq!(obs["from"], "observation");
        assert_eq!(obs["tool_call_id"], "c9");
        assert_eq!(obs["is_error"], true);
        assert_eq!(obs["value"], "denied");
    }

    #[test]
    fn top_level_fields_present() {
        let t = full_fixture();
        let v = trace_to_sharegpt(&t);
        assert_eq!(v["trace_id"], "t1");
        assert_eq!(v["model"], "claude-opus-4-7");
        assert_eq!(v["tools"], json!([{"name": "weather"}]));
        assert!(v["timestamp"].is_i64());
    }

    #[test]
    fn write_jsonl_appends_one_line_per_trace() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_owned();
        let traces = vec![full_fixture(), full_fixture()];
        write_sharegpt_jsonl(&path, &traces).expect("write");
        let content = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: Value = serde_json::from_str(line).expect("each line is valid JSON");
            assert_eq!(v["trace_id"], "t1");
        }
    }

    #[test]
    fn write_jsonl_appends_to_existing_file() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_owned();
        write_sharegpt_jsonl(&path, &[full_fixture()]).expect("first write");
        write_sharegpt_jsonl(&path, &[full_fixture()]).expect("second write");
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content.lines().count(), 2);
    }
}
