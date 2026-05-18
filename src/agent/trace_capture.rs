//! Lossless per-turn trace capture for SFT data export.
//!
//! Distinct from [`crate::agent::turn_metrics::TurnMetrics`]: that module
//! tracks difficulty scoring and feeds SKILL.md crystallization with lossy
//! summaries. This one preserves the full ordered step sequence (user input,
//! model thinking, tool calls with raw args, tool results, final reply) so
//! `sft_exporter` can emit ShareGPT samples without information loss.
//!
//! Captured opt-in per session so runtime can decide when to pay the memory
//! cost.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One step in an agent turn. Variants are serialized with a `kind` tag so
/// JSONL lines remain self-describing for downstream consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TraceStep {
    /// User-supplied input that opened or extended the turn.
    User { content: String },
    /// Model's `<think>` content emitted before any tool call or final
    /// reply. Captured separately so SFT can teach the structured thinking
    /// pattern explicitly.
    AssistantThinking { content: String },
    /// A tool invocation requested by the model. `args` keeps the raw JSON
    /// the model produced; do not summarize.
    AssistantToolCall {
        name: String,
        args: Value,
        call_id: String,
    },
    /// Result returned to the model from a tool. `content` is the verbatim
    /// payload the model saw next.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Final natural-language reply emitted to the user.
    AssistantText { content: String },
}

/// Complete record of one agent turn suitable for SFT export.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FullTrace {
    /// Stable identifier for cross-referencing with logs and dedup.
    pub trace_id: String,
    /// Unix seconds when the trace was opened. Best-effort; 0 if the system
    /// clock is unavailable.
    pub timestamp: i64,
    /// Model id used to produce this trace (teacher in distill mode,
    /// student in eval mode).
    pub model: String,
    /// System prompt that was active when the turn started.
    pub system_prompt: String,
    /// Tool schemas sent to the model — full OpenAI-style function array.
    pub tools_schema: Value,
    /// Steps in execution order. Lossless.
    pub steps: Vec<TraceStep>,
}

impl FullTrace {
    /// Open a new trace with an empty step list. The caller pushes steps
    /// in execution order as the agent loop progresses.
    pub fn new(
        trace_id: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        tools_schema: Value,
    ) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            trace_id: trace_id.into(),
            timestamp,
            model: model.into(),
            system_prompt: system_prompt.into(),
            tools_schema,
            steps: Vec::new(),
        }
    }

    /// Append a user-input step.
    pub fn push_user(&mut self, content: impl Into<String>) {
        self.steps.push(TraceStep::User {
            content: content.into(),
        });
    }

    /// Append an assistant thinking step.
    pub fn push_thinking(&mut self, content: impl Into<String>) {
        self.steps.push(TraceStep::AssistantThinking {
            content: content.into(),
        });
    }

    /// Append a tool call step. `args` is the raw JSON the model produced.
    pub fn push_tool_call(
        &mut self,
        name: impl Into<String>,
        args: Value,
        call_id: impl Into<String>,
    ) {
        self.steps.push(TraceStep::AssistantToolCall {
            name: name.into(),
            args,
            call_id: call_id.into(),
        });
    }

    /// Append a tool result step.
    pub fn push_tool_result(
        &mut self,
        call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) {
        self.steps.push(TraceStep::ToolResult {
            call_id: call_id.into(),
            content: content.into(),
            is_error,
        });
    }

    /// Append a final assistant-text step.
    pub fn push_assistant_text(&mut self, content: impl Into<String>) {
        self.steps.push(TraceStep::AssistantText {
            content: content.into(),
        });
    }

    /// Number of recorded steps.
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// True if at least one step is a tool call. Used by quality gates to
    /// filter out trivial turns that contribute nothing to tool-use SFT.
    pub fn has_tool_calls(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, TraceStep::AssistantToolCall { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> FullTrace {
        FullTrace::new("t1", "claude-opus-4-7", "you are rsclaw", json!([]))
    }

    #[test]
    fn push_and_count_steps_in_order() {
        let mut t = fixture();
        t.push_user("hello");
        t.push_thinking("user wants a greeting");
        t.push_tool_call("greet", json!({"name": "world"}), "call_1");
        t.push_tool_result("call_1", "hi world", false);
        t.push_assistant_text("done");
        assert_eq!(t.step_count(), 5);
        assert!(t.has_tool_calls());
        // Order preserved.
        assert!(matches!(t.steps[0], TraceStep::User { .. }));
        assert!(matches!(t.steps[4], TraceStep::AssistantText { .. }));
    }

    #[test]
    fn empty_trace_has_no_tool_calls() {
        let t = fixture();
        assert!(!t.has_tool_calls());
        assert_eq!(t.step_count(), 0);
    }

    #[test]
    fn round_trip_serde_is_lossless() {
        let mut t = fixture();
        t.push_user("u");
        t.push_thinking("thinking");
        t.push_tool_call("search", json!({"q": "weather"}), "c1");
        t.push_tool_result("c1", r#"{"temp": 20}"#, false);
        t.push_assistant_text("it is 20 degrees");
        let s = serde_json::to_string(&t).expect("serialize");
        let back: FullTrace = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, t);
    }

    #[test]
    fn tool_result_error_flag_preserved() {
        let mut t = fixture();
        t.push_tool_result("c1", "permission denied", true);
        match &t.steps[0] {
            TraceStep::ToolResult { is_error, content, .. } => {
                assert!(*is_error);
                assert_eq!(content, "permission denied");
            }
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn raw_args_are_not_summarized() {
        let mut t = fixture();
        let args = json!({"deeply": {"nested": {"value": 42}}});
        t.push_tool_call("x", args.clone(), "c");
        match &t.steps[0] {
            TraceStep::AssistantToolCall { args: stored, .. } => {
                assert_eq!(stored, &args);
            }
            _ => panic!("expected AssistantToolCall"),
        }
    }

    #[test]
    fn tagged_serialization_uses_snake_case_kind() {
        let mut t = fixture();
        t.push_user("hi");
        let s = serde_json::to_string(&t.steps[0]).expect("serialize");
        assert!(s.contains(r#""kind":"user""#), "got: {s}");
    }
}
