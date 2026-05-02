//! Workflow-style skill crystallization.
//!
//! Distills a single hard turn (many tool calls, errors recovered, long
//! duration) into a SKILL.md that captures the *workflow* — what to do,
//! what gotchas to expect, how to recover. Complements the cluster-based
//! `crystallizer` module which captures *facts* from repeated recall.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::agent::turn_metrics::TurnMetrics;
use crate::provider::registry::ProviderRegistry;

/// Build the LLM prompt for workflow distillation. Receives the raw turn
/// transcript so the model can extract the steps, errors, and recovery
/// pattern directly without us pre-summarizing.
pub fn build_workflow_prompt(user_text: &str, reply_text: &str, metrics: &TurnMetrics) -> String {
    let mut prompt = String::with_capacity(8192);

    prompt.push_str(
        "You are a process-engineering expert. Below is a single agent turn that \
         succeeded but required multiple tool calls and possibly recovered from \
         failures along the way. Distill the workflow into a SKILL.md so a \
         future agent can do this faster.\n\n\
         \
         ## SKILL.md format\n\
         \
         **Frontmatter** (required):\n\
         ```yaml\n\
         ---\n\
         name: short-kebab-case-slug\n\
         description: >\n\
           When to invoke this skill. Be slightly pushy: \"Use this whenever the\n\
           user asks for X, especially if it involves Y.\"\n\
         ---\n\
         ```\n\
         \n\
         **Body** (Markdown, imperative voice):\n\
         - `## Trigger` — concrete user-request shapes this skill matches.\n\
         - `## Steps` — numbered workflow with reasoning per step.\n\
         - `## Pitfalls` — failures observed during the original run + how the\n\
           agent recovered. This is the highest-value section — it codifies the\n\
           hard-won lessons. List actual tool errors; don't invent generic ones.\n\
         - Keep total length under 200 lines unless complexity demands more.\n\n\
         \
         === ORIGINAL USER REQUEST ===\n",
    );
    prompt.push_str(user_text);
    prompt.push_str("\n\n=== TOOL CALL SEQUENCE ===\n\n");

    for (i, entry) in metrics.tool_log.iter().enumerate() {
        prompt.push_str(&format!(
            "{}. {} {}\n   args: {}\n   result: {}\n",
            i + 1,
            if entry.is_error { "[ERROR]" } else { "[OK]" },
            entry.name,
            entry.args_summary,
            entry.result_summary,
        ));
    }

    prompt.push_str(&format!(
        "\n=== METRICS ===\n\
         - tool_calls={}, distinct_tools={}, tool_errors={}\n\
         - same_call_streak_max={}, duration_secs={:.1}\n\
         - difficulty_score={:.2}\n",
        metrics.tool_calls,
        metrics.distinct_tools.len(),
        metrics.tool_errors,
        metrics.same_call_streak_max,
        metrics.duration_secs(),
        metrics.difficulty_score(),
    ));

    prompt.push_str("\n=== FINAL AGENT REPLY ===\n");
    prompt.push_str(reply_text);

    prompt.push_str(
        "\n\n=== INSTRUCTIONS ===\n\
         Produce ONLY the SKILL.md content (frontmatter + body). \
         No commentary outside the file. \
         Focus the Pitfalls section on actual errors from the tool log — \
         not generic warnings. If there were no errors, write a short \
         note: 'Pitfalls: none observed during the original run.'\n",
    );

    prompt
}

/// Crystallize one hard-won turn into a SKILL.md.
///
/// Mirrors the cluster-path `crystallize_one` but takes a
/// [`TurnMetrics`] + transcript instead of a memory cluster. Reuses the
/// shared infrastructure: `acquire_distill_permit`, `validate_skill_md`,
/// `extract_skill_slug`, `write_skill`. Returns the path written, or
/// `Ok(None)` if any non-fatal early-out kicks in.
pub async fn crystallize_workflow(
    user_text: &str,
    reply_text: &str,
    metrics: &TurnMetrics,
    signature: u64,
    providers: &Arc<ProviderRegistry>,
    flash_model: &str,
    skills_dir: &Path,
) -> Result<Option<PathBuf>> {
    if !crate::agent::evolution::evolution_config().enabled {
        return Ok(None);
    }
    if flash_model.is_empty() {
        tracing::debug!("workflow distill: no flash model resolved, skipping");
        return Ok(None);
    }

    let prompt = build_workflow_prompt(user_text, reply_text, metrics);

    let (provider_name, model_id) = providers.resolve_model(flash_model);
    let provider_arc = match providers.get(provider_name) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                provider = provider_name,
                "workflow distill: provider not registered: {e:#}"
            );
            return Ok(None);
        }
    };

    let _permit = match crate::skill::crystallizer::acquire_distill_permit().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("workflow distill: failed to acquire permit: {e:#}");
            return Ok(None);
        }
    };

    let skill_md = match crate::skill::crystallizer::distill_with_llm(
        &prompt,
        provider_arc,
        model_id.to_owned(),
    )
    .await
    {
        Ok(md) => md,
        Err(e) => {
            tracing::warn!("workflow distill: LLM call failed: {e:#}");
            return Ok(None);
        }
    };

    if let Err(e) = crate::skill::crystallizer::validate_skill_md(&skill_md) {
        tracing::warn!("workflow distill: invalid SKILL.md output: {e:#}");
        return Ok(None);
    }

    let fallback = format!("flow-{signature:08x}");
    let raw_slug = crate::skill::crystallizer::extract_skill_slug(&skill_md, &fallback);
    let slug = if raw_slug.starts_with("flow-") {
        format!("auto-{raw_slug}")
    } else {
        format!("auto-flow-{raw_slug}")
    };
    let path = crate::skill::crystallizer::write_skill(skills_dir, &slug, &skill_md)
        .with_context(|| format!("write_skill failed for slug '{slug}'"))?;

    tracing::info!(
        ?path,
        slug = %slug,
        difficulty = metrics.difficulty_score(),
        tool_calls = metrics.tool_calls,
        tool_errors = metrics.tool_errors,
        "workflow crystallized into skill"
    );
    Ok(Some(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::turn_metrics::TurnMetrics;

    #[test]
    fn prompt_includes_tool_log() {
        let mut m = TurnMetrics::new();
        m.record_tool("read_file", "{}".into(), "ok".into(), false);
        m.record_tool(
            "execute_command",
            r#"{"cmd":"ls"}"#.into(),
            "permission denied".into(),
            true,
        );
        let p = build_workflow_prompt("list files", "done", &m);
        assert!(p.contains("read_file"));
        assert!(p.contains("execute_command"));
        assert!(p.contains("[ERROR]"));
        assert!(p.contains("[OK]"));
        assert!(p.contains("permission denied"));
    }

    #[test]
    fn prompt_mentions_metrics() {
        let mut m = TurnMetrics::new();
        m.record_tool("a", "{}".into(), "ok".into(), false);
        m.record_tool("b", "{}".into(), "ok".into(), false);
        m.tool_errors = 1;
        let p = build_workflow_prompt("test", "done", &m);
        assert!(p.contains("difficulty_score="));
        assert!(p.contains("tool_calls=2"));
    }
}
