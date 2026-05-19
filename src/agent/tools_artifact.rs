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

        let lines: Vec<&str> = full.lines().collect();
        let total = lines.len();
        let selected: String = if mode == "full" {
            full
        } else if let Some(n) = mode.strip_prefix("head:").and_then(|s| s.parse::<usize>().ok()) {
            lines.iter().take(n).cloned().collect::<Vec<_>>().join("\n")
        } else if let Some(n) = mode.strip_prefix("tail:").and_then(|s| s.parse::<usize>().ok()) {
            lines.iter().rev().take(n).rev().cloned().collect::<Vec<_>>().join("\n")
        } else if let Some(range) = mode.strip_prefix("lines:") {
            let (a, b) = range
                .split_once('-')
                .ok_or_else(|| anyhow!("read_artifact: `lines:A-B` malformed: `{range}`"))?;
            let a: usize = a.parse().map_err(|_| anyhow!("read_artifact: bad start line `{a}`"))?;
            let b: usize = b.parse().map_err(|_| anyhow!("read_artifact: bad end line `{b}`"))?;
            if a == 0 || b < a {
                return Err(anyhow!(
                    "read_artifact: lines:A-B must satisfy 1 ≤ A ≤ B, got {a}-{b}"
                ));
            }
            let lo = a.saturating_sub(1);
            let hi = b.min(total);
            lines[lo..hi].join("\n")
        } else if let Some(pattern) = mode.strip_prefix("grep:") {
            let re = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|e| anyhow!("read_artifact: grep pattern invalid: {e}"))?;
            lines
                .iter()
                .filter(|l| re.is_match(l))
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            return Err(anyhow!(
                "read_artifact: unknown mode `{mode}`. Use full | head:N | tail:N | lines:A-B | grep:PATTERN"
            ));
        };

        Ok(json!({
            "tool_result_id": id.as_str(),
            "mode": mode,
            "total_lines": total,
            "returned_chars": selected.chars().count(),
            "content": selected,
        }))
    }
}
