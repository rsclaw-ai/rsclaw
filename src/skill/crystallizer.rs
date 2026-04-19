//! Skill crystallization: distill clusters of related Core memories into
//! reusable SKILL.md files.
//!
//! When a memory document reaches Core tier (accessed 10+ times, importance
//! >= 0.8), this module checks whether it forms a cluster with other related
//! Core memories.  If the cluster is large enough, an LLM prompt is built to
//! distill them into a `SKILL.md` file that can be loaded by the skill
//! subsystem.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::agent::memory::{MemDocTier, MemoryDoc, MemoryStore};

/// Minimum number of related Core memories to trigger crystallization.
const MIN_CLUSTER_SIZE: usize = 3;

/// Cosine similarity threshold for "related" memories.
const CLUSTER_SIMILARITY: f32 = 0.75;

/// Find a cluster of related Core-tier memories around the given document.
///
/// Returns `None` if the cluster (including the source doc) is smaller than
/// [`MIN_CLUSTER_SIZE`].  All returned docs share the same scope, are Core
/// tier, and have not yet been tagged `"crystallized"`.
pub fn find_cluster(
    store: &MemoryStore,
    doc_id: &str,
    scope: &str,
) -> Result<Option<Vec<MemoryDoc>>> {
    let source = store
        .get_sync(doc_id)
        .context("source doc not found in store")?
        .clone();

    // The source itself must qualify.
    if source.tier != MemDocTier::Core
        || source.scope != scope
        || source.tags.contains(&"crystallized".to_string())
    {
        return Ok(None);
    }

    let neighbours =
        store.find_near_duplicates(doc_id, Some(scope), CLUSTER_SIMILARITY)?;

    let mut cluster: Vec<MemoryDoc> = neighbours
        .into_iter()
        .filter(|(doc, _sim)| {
            doc.tier == MemDocTier::Core
                && doc.scope == scope
                && !doc.tags.contains(&"crystallized".to_string())
        })
        .map(|(doc, _sim)| doc)
        .collect();

    // Include the source document itself.
    cluster.insert(0, source);

    if cluster.len() < MIN_CLUSTER_SIZE {
        return Ok(None);
    }

    Ok(Some(cluster))
}

/// Build an LLM prompt that asks the model to distill the given cluster of
/// memory documents into a standard-compliant `SKILL.md` file.
///
/// The output follows the Anthropic skill-creator standard:
/// - YAML frontmatter with `name` and a "pushy" `description` that states
///   both what the skill does and when it should trigger.
/// - Imperative Markdown body (under 500 lines) covering the workflow in
///   enough detail that an agent can execute it without further context.
/// - Optionally references `scripts/` or `references/` bundled resources when
///   the cluster content implies reusable scripts or large reference material.
pub fn build_distill_prompt(cluster: &[MemoryDoc]) -> String {
    let mut prompt = String::with_capacity(8192);

    prompt.push_str(
        "You are a skill-engineering expert. Below are related memory documents \
         from an AI agent's long-term memory store. Distill them into a single \
         SKILL.md file following the Anthropic skill-creator standard.\n\n\
         \
         ## SKILL.md Standard\n\
         \
         **Frontmatter** (required fields):\n\
         ```yaml\n\
         ---\n\
         name: skill-name-in-kebab-case\n\
         description: >\n\
           What the skill does AND when to invoke it. Be slightly pushy so the\n\
           agent does not undertrigger. Example: \"How to do X. Use this skill\n\
           whenever the user asks about X, Y, or Z, even if not phrased explicitly.\"\n\
         ---\n\
         ```\n\n\
         \
         **Body** (Markdown, imperative language, under 500 lines):\n\
         - Use numbered steps or headers to structure the workflow.\n\
         - Explain *why* each step matters, not just *what* to do.\n\
         - Include a short example (Input / Output) where it helps.\n\
         - If the skill needs a reusable helper script, note it as:\n\
           `See scripts/helper.py — run with: python scripts/helper.py <args>`\n\
           (do NOT write the script here; the caller will create it separately)\n\
         - If the skill references large external docs, note them as:\n\
           `See references/guide.md for detailed field descriptions`\n\n\
         \
         **Rules**:\n\
         - Do not invent information beyond what the memory documents contain.\n\
         - Merge overlapping facts; prefer the most-accessed version.\n\
         - Use imperative voice: \"Check the config\", not \"You should check\".\n\
         - Avoid ALL-CAPS MUST/NEVER; explain reasoning instead.\n\
         - Keep total length under 300 lines unless complexity demands more.\n\n\
         \
         === MEMORY DOCUMENTS ===\n\n",
    );

    for (i, doc) in cluster.iter().enumerate() {
        prompt.push_str(&format!(
            "--- Memory {} (access_count={}) ---\nKind: {}\nText:\n{}\n\n",
            i + 1,
            doc.access_count,
            doc.kind,
            doc.text,
        ));
    }

    prompt.push_str(
        "=== END OF MEMORIES ===\n\n\
         Produce ONLY the SKILL.md content — frontmatter + body. \
         No explanation, no commentary outside the file.",
    );

    prompt
}

/// Write a `SKILL.md` file into the skills directory under the given slug.
///
/// Creates `{skills_dir}/{slug}/SKILL.md` and returns the full path to the
/// written file.
pub fn write_skill(skills_dir: &Path, slug: &str, content: &str) -> Result<PathBuf> {
    let dir = skills_dir.join(slug);
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create skill directory: {}", dir.display()))?;

    let path = dir.join("SKILL.md");
    fs::write(&path, content)
        .with_context(|| format!("failed to write SKILL.md at {}", path.display()))?;

    Ok(path)
}

/// Convert a human-readable name into a valid skill slug.
///
/// Lowercases the input, replaces non-alphanumeric characters with hyphens,
/// collapses consecutive hyphens, and trims leading/trailing hyphens.
///
/// # Examples
///
/// ```
/// # use rsclaw::skill::crystallizer::slugify;
/// assert_eq!(slugify("Web Search Pattern"), "web-search-pattern");
/// assert_eq!(slugify("  LLM--Retry  Logic! "), "llm-retry-logic");
/// ```
pub fn slugify(name: &str) -> String {
    let lower = name.to_lowercase();
    let slug: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens and trim.
    let mut result = String::with_capacity(slug.len());
    let mut prev_hyphen = true; // start true to trim leading hyphens
    for ch in slug.chars() {
        if ch == '-' {
            if !prev_hyphen {
                result.push('-');
            }
            prev_hyphen = true;
        } else {
            result.push(ch);
            prev_hyphen = false;
        }
    }

    // Trim trailing hyphen.
    if result.ends_with('-') {
        result.pop();
    }

    if result.is_empty() {
        "unnamed-skill".to_owned()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Web Search Pattern"), "web-search-pattern");
    }

    #[test]
    fn slugify_special_chars() {
        assert_eq!(slugify("  LLM--Retry  Logic! "), "llm-retry-logic");
    }

    #[test]
    fn slugify_already_clean() {
        assert_eq!(slugify("hello-world"), "hello-world");
    }

    #[test]
    fn slugify_empty() {
        assert_eq!(slugify(""), "unnamed-skill");
    }
}
