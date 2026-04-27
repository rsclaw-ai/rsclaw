//! Skill crystallization: distill clusters of related Core memories into
//! reusable SKILL.md files.
//!
//! When a memory document reaches Core tier (accessed 10+ times, importance
//! >= 0.8), this module checks whether it forms a cluster with other related
//! Core memories.  If the cluster is large enough, an LLM prompt is built to
//! distill them into a `SKILL.md` file that can be loaded by the skill
//! subsystem.

use std::collections::HashSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt as _;
use tokio::sync::Semaphore;

use crate::agent::memory::{MemDocTier, MemoryDoc, MemoryStore};
use crate::provider::{
    LlmProvider, LlmRequest, Message, MessageContent, Role, StreamEvent,
};

// ---------------------------------------------------------------------------
// Process-wide concurrency control
// ---------------------------------------------------------------------------

/// Single-permit semaphore: only one crystallization LLM call runs at a time
/// across the whole process. Distillation is rare and expensive; serializing
/// avoids surprise concurrent burns when multiple turns finish at once.
fn distill_lock() -> &'static Semaphore {
    static LOCK: OnceLock<Semaphore> = OnceLock::new();
    LOCK.get_or_init(|| Semaphore::new(1))
}

/// Set of cluster fingerprints currently being distilled. Two simultaneous
/// turns that recall an overlapping cluster would otherwise both spawn an
/// LLM call against the same set of memories.
fn inflight_clusters() -> &'static Mutex<HashSet<u64>> {
    static SET: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    SET.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Hash a cluster down to a u64 fingerprint, order-independent.
///
/// Sorting makes the hash invariant under doc ordering, so two turns that
/// retrieve the same Core docs in different orders produce the same key.
pub fn cluster_fingerprint(doc_ids: &[String]) -> u64 {
    let mut sorted: Vec<&str> = doc_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hasher = DefaultHasher::new();
    for id in &sorted {
        id.hash(&mut hasher);
    }
    hasher.finish()
}

/// RAII guard that holds an in-flight cluster fingerprint and releases it on
/// drop. Returned by [`try_claim_cluster`] when the caller wins the race;
/// returns `None` if the fingerprint is already being processed.
pub struct ClusterGuard {
    fingerprint: u64,
}

impl Drop for ClusterGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = inflight_clusters().lock() {
            set.remove(&self.fingerprint);
        }
    }
}

/// Try to claim ownership of a cluster fingerprint. Returns a guard on success;
/// returns `None` if another task is already processing the same fingerprint.
pub fn try_claim_cluster(doc_ids: &[String]) -> Option<ClusterGuard> {
    let fingerprint = cluster_fingerprint(doc_ids);
    let mut set = inflight_clusters().lock().ok()?;
    if set.insert(fingerprint) {
        Some(ClusterGuard { fingerprint })
    } else {
        None
    }
}

/// Acquire the global distillation permit. Held for the duration of the LLM
/// call to serialize crystallization across the process.
pub async fn acquire_distill_permit() -> Result<tokio::sync::SemaphorePermit<'static>> {
    distill_lock()
        .acquire()
        .await
        .map_err(|e| anyhow!("distill semaphore closed: {e}"))
}

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
/// Creates `{skills_dir}/{slug}/SKILL.md`. If a SKILL.md already exists at
/// that path (a different crystallization run picked the same frontmatter
/// `name:`), append `-2`, `-3`, ... to the slug until a free slot is found,
/// instead of silently overwriting. Returns the full path written.
pub fn write_skill(skills_dir: &Path, slug: &str, content: &str) -> Result<PathBuf> {
    // Probe for a non-colliding slug. Cap at 99 to avoid runaway loops if
    // someone seeds 100+ identically-named skills (something is very wrong
    // by then anyway).
    let mut attempt = 1u32;
    let (dir, final_slug) = loop {
        let candidate_slug = if attempt == 1 {
            slug.to_owned()
        } else {
            format!("{slug}-{attempt}")
        };
        let candidate_dir = skills_dir.join(&candidate_slug);
        if !candidate_dir.join("SKILL.md").exists() {
            break (candidate_dir, candidate_slug);
        }
        attempt += 1;
        if attempt > 99 {
            bail!("write_skill: too many slug collisions for '{slug}'");
        }
    };

    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create skill directory: {}", dir.display()))?;

    let path = dir.join("SKILL.md");
    fs::write(&path, content)
        .with_context(|| format!("failed to write SKILL.md at {}", path.display()))?;

    if final_slug != slug {
        tracing::info!(
            requested = slug,
            actual = %final_slug,
            "skill slug collided, suffix appended"
        );
    }

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

/// Distill a cluster prompt into SKILL.md content via a one-shot LLM call.
///
/// Sends `prompt` as a single user message to the given provider and returns
/// the assistant text accumulated from the stream. Tool-call events are
/// ignored (the request carries no tools). Returns an error on stream
/// failure or empty output so the caller can decide whether to fall back.
pub async fn distill_with_llm(
    prompt: &str,
    provider: Arc<dyn LlmProvider>,
    model: String,
) -> Result<String> {
    let req = LlmRequest {
        model,
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(prompt.to_owned()),
        }],
        tools: Vec::new(),
        system: None,
        max_tokens: Some(4096),
        temperature: Some(0.3),
        frequency_penalty: None,
        thinking_budget: None,
        kv_cache_mode: 0,
        session_key: None,
    };

    let mut stream = provider
        .stream(req)
        .await
        .context("distill: provider stream failed")?;

    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta(d)) => output.push_str(&d),
            Ok(StreamEvent::ReasoningDelta(_)) => {}
            Ok(StreamEvent::Done { .. }) => break,
            Ok(StreamEvent::Error(msg)) => bail!("distill: provider error: {msg}"),
            Ok(StreamEvent::ToolCall { .. }) => {} // no tools requested; ignore
            Err(e) => return Err(anyhow!("distill stream error: {e:#}")),
        }
    }

    if output.trim().is_empty() {
        bail!("distill: empty output from LLM");
    }
    Ok(output)
}

/// Validate that an LLM-produced SKILL.md string is well-formed enough to
/// be loaded by the skill registry.
///
/// Crystallization runs the LLM unattended, so a malformed reply (missing
/// frontmatter, empty fields, plain prose without YAML) would land a broken
/// skill on disk. This check enforces the minimum the loader needs:
///
/// - Starts with a `---` frontmatter fence and has a closing `---`.
/// - Frontmatter parses as YAML.
/// - `name:` is present and non-empty.
/// - `description:` is present and non-empty (skill-creator standard
///   requires it; an empty one renders the skill invisible to the agent).
///
/// Returns the parsed name+description on success so the caller can re-use
/// them without re-parsing.
pub fn validate_skill_md(content: &str) -> Result<(String, String)> {
    let trimmed = content.trim_start();
    let rest = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("SKILL.md must start with '---' frontmatter fence"))?;

    let close_idx = rest
        .find("\n---")
        .ok_or_else(|| anyhow!("SKILL.md frontmatter has no closing '---'"))?;
    let fm = &rest[..close_idx];

    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(fm)
        .map_err(|e| anyhow!("SKILL.md frontmatter YAML invalid: {e}"))?;

    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("SKILL.md frontmatter missing non-empty 'name'"))?
        .to_owned();

    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("SKILL.md frontmatter missing non-empty 'description'"))?
        .to_owned();

    Ok((name, description))
}

/// Extract a slug from the `name:` field of a SKILL.md YAML frontmatter.
///
/// Walks the leading frontmatter (between `---` delimiters) looking for a
/// `name:` line. Quoted and unquoted values are accepted. Falls back to
/// slugifying `fallback` if no usable name is found.
pub fn extract_skill_slug(skill_md: &str, fallback: &str) -> String {
    let mut delimiters_seen = 0u8;
    let mut in_frontmatter = false;
    for line in skill_md.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            delimiters_seen += 1;
            if delimiters_seen == 1 {
                in_frontmatter = true;
                continue;
            }
            // Closing delimiter — stop scanning.
            break;
        }
        if !in_frontmatter {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name:") {
            let raw = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !raw.is_empty() {
                return slugify(raw);
            }
        }
    }
    slugify(fallback)
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

    #[test]
    fn extract_slug_from_frontmatter_unquoted() {
        let md = "---\nname: web-search-helper\ndescription: foo\n---\nbody";
        assert_eq!(extract_skill_slug(md, "fallback"), "web-search-helper");
    }

    #[test]
    fn extract_slug_from_frontmatter_quoted() {
        let md = "---\nname: \"Order Extractor\"\n---\nbody";
        assert_eq!(extract_skill_slug(md, "fallback"), "order-extractor");
    }

    #[test]
    fn extract_slug_falls_back_when_no_name() {
        let md = "---\ndescription: foo\n---\nbody";
        assert_eq!(extract_skill_slug(md, "Auto Skill"), "auto-skill");
    }

    #[test]
    fn extract_slug_falls_back_when_no_frontmatter() {
        let md = "just body, no frontmatter";
        assert_eq!(extract_skill_slug(md, "Fallback Name"), "fallback-name");
    }

    #[test]
    fn extract_slug_ignores_name_after_closing_delimiter() {
        let md = "---\ndescription: foo\n---\nname: not-a-real-name\n";
        assert_eq!(extract_skill_slug(md, "real"), "real");
    }

    #[test]
    fn validate_accepts_well_formed() {
        let md = "---\nname: my-skill\ndescription: Does X. Use when Y.\n---\nbody\n";
        let (n, d) = validate_skill_md(md).expect("should be valid");
        assert_eq!(n, "my-skill");
        assert_eq!(d, "Does X. Use when Y.");
    }

    #[test]
    fn validate_rejects_no_frontmatter() {
        assert!(validate_skill_md("just body\n").is_err());
    }

    #[test]
    fn validate_rejects_unclosed_frontmatter() {
        assert!(validate_skill_md("---\nname: foo\n").is_err());
    }

    #[test]
    fn validate_rejects_missing_name() {
        let md = "---\ndescription: foo\n---\nbody";
        assert!(validate_skill_md(md).is_err());
    }

    #[test]
    fn validate_rejects_empty_name() {
        let md = "---\nname: \"\"\ndescription: foo\n---\nbody";
        assert!(validate_skill_md(md).is_err());
    }

    #[test]
    fn validate_rejects_missing_description() {
        let md = "---\nname: foo\n---\nbody";
        assert!(validate_skill_md(md).is_err());
    }

    #[test]
    fn validate_rejects_empty_description() {
        let md = "---\nname: foo\ndescription: \"  \"\n---\nbody";
        assert!(validate_skill_md(md).is_err());
    }

    #[test]
    fn validate_rejects_invalid_yaml() {
        let md = "---\nname: foo\ndescription: [unclosed\n---\nbody";
        assert!(validate_skill_md(md).is_err());
    }

    #[test]
    fn cluster_fingerprint_is_order_invariant() {
        let a = vec!["doc-1".to_owned(), "doc-2".to_owned(), "doc-3".to_owned()];
        let b = vec!["doc-3".to_owned(), "doc-1".to_owned(), "doc-2".to_owned()];
        assert_eq!(cluster_fingerprint(&a), cluster_fingerprint(&b));
    }

    #[test]
    fn cluster_fingerprint_distinguishes_different_clusters() {
        let a = vec!["doc-1".to_owned(), "doc-2".to_owned()];
        let b = vec!["doc-1".to_owned(), "doc-3".to_owned()];
        assert_ne!(cluster_fingerprint(&a), cluster_fingerprint(&b));
    }

    #[test]
    fn try_claim_cluster_blocks_duplicates_and_releases_on_drop() {
        // Use a unique-ish id set to avoid colliding with other tests.
        let ids = vec![
            "claim-test-aaa".to_owned(),
            "claim-test-bbb".to_owned(),
            "claim-test-ccc".to_owned(),
        ];

        let g1 = try_claim_cluster(&ids).expect("first claim should win");
        let g2 = try_claim_cluster(&ids);
        assert!(g2.is_none(), "second claim while first held should fail");

        drop(g1);
        let g3 = try_claim_cluster(&ids).expect("claim should work after drop");
        drop(g3);
    }

    #[test]
    fn write_skill_appends_suffix_on_collision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();

        let p1 = write_skill(dir, "shared-name", "first\n").expect("first write");
        let p2 = write_skill(dir, "shared-name", "second\n").expect("second write");
        let p3 = write_skill(dir, "shared-name", "third\n").expect("third write");

        assert_eq!(p1, dir.join("shared-name").join("SKILL.md"));
        assert_eq!(p2, dir.join("shared-name-2").join("SKILL.md"));
        assert_eq!(p3, dir.join("shared-name-3").join("SKILL.md"));

        // Each write landed its own content.
        assert_eq!(fs::read_to_string(&p1).unwrap(), "first\n");
        assert_eq!(fs::read_to_string(&p2).unwrap(), "second\n");
        assert_eq!(fs::read_to_string(&p3).unwrap(), "third\n");
    }
}
