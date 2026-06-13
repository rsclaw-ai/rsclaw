//! Skill-related built-in tools.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use super::runtime::AgentRuntime;
use crate::skill::SkillManifest;

pub(crate) fn paginate_skill_list<'a, I>(skills: I, args: &Value) -> Value
where
    I: IntoIterator<Item = &'a SkillManifest>,
{
    let query = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let query_lower = query.map(|q| q.to_lowercase());
    let limit = args["limit"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(50)
        .clamp(1, 100);
    let offset = args["offset"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);

    let mut all = skills.into_iter().collect::<Vec<_>>();
    let total = all.len();
    all.retain(|s| {
        let Some(query) = query_lower.as_deref() else {
            return true;
        };
        s.name.to_lowercase().contains(query)
            || s.description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(query)
    });
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let matched = all.len();
    let skills = all
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description.clone().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let next_offset = offset + skills.len();
    let has_more = next_offset < matched;

    json!({
        "count": total,
        "matched": matched,
        "offset": offset,
        "limit": limit,
        "has_more": has_more,
        "next_offset": has_more.then_some(next_offset),
        "skills": skills,
    })
}

impl AgentRuntime {
    pub(crate) fn tool_use_skill(&self, args: Value) -> Result<Value> {
        // Try "name" first, then fall back to common alternatives the model
        // sometimes emits when it confuses the parameter name.
        let name = args
            .get("name")
            .or_else(|| args.get("skill"))
            .or_else(|| args.get("skill_name"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let available: Vec<String> = self.skills.all().map(|s| s.name.clone()).collect();
                anyhow!(
                    "use_skill: 'name' is required. Available skills: {}. \
                     Received args: {}",
                    available.join(", "),
                    args
                )
            })?;
        // Resolve the skill dir: prefer the live registry, else fall back to
        // the skills dir on disk so a freshly `skill_install`ed skill is usable
        // the SAME turn (before the next compact/clear/new reload folds it into
        // the registry + system prompt).
        let skill_dir = if let Some(skill) = self.skills.get(name) {
            skill.dir.clone()
        } else if !crate::skill::valid_slug(name) {
            // Disk fallback joins `name` onto the skills root — reject traversal.
            return Ok(serde_json::json!({ "error": format!("invalid skill name: {name:?}") }));
        } else {
            let disk = crate::config::loader::base_dir().join("skills").join(name);
            if disk.join("SKILL.md").is_file() {
                disk
            } else {
                let available: Vec<&str> = self.skills.all().map(|s| s.name.as_str()).collect();
                return Ok(serde_json::json!({
                    "error": format!("skill '{name}' not installed"),
                    "available": available,
                }));
            }
        };
        let dir = skill_dir.display().to_string();
        let skill_md_path = skill_dir.join("SKILL.md");
        let skill_md = std::fs::read_to_string(&skill_md_path).unwrap_or_else(|e| {
            format!(
                "(failed to read SKILL.md: {e}; check {})",
                skill_md_path.display()
            )
        });
        // Usage stat: feeds the meditation retirement pass for
        // auto-crystallized skills (zero-use-in-N-days -> .retired/).
        // Best-effort -- a stats write must never fail the activation.
        if let Err(e) = crate::skill::record_skill_use(&self.store.db, name) {
            tracing::debug!(skill = name, "skill use stat write failed: {e:#}");
        }
        Ok(serde_json::json!({
            "name": name,
            "dir": dir,
            "skill_md": skill_md,
            "next_step": "Read skill_md to find the exact CLI command and flags, \
                          then call shell to run it. \
                          Pass the user's actual question / parameters via the \
                          flags documented in skill_md."
        }))
    }

    /// `skill_list` -- list locally-installed skills (name + description).
    pub(crate) fn tool_skill_list(&self, args: Value) -> Result<Value> {
        Ok(paginate_skill_list(self.skills.all(), &args))
    }

    /// `skill_search` -- search the remote skill registries (clawhub /
    /// skillhub / iwencai). Returns candidate slugs the model can
    /// `skill_install`.
    pub(crate) async fn tool_skill_search(&self, args: Value) -> Result<Value> {
        let query = args["query"]
            .as_str()
            .or_else(|| args["q"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if query.is_empty() {
            return Ok(json!({ "error": "query is required" }));
        }
        let client = crate::skill::clawhub::ClawhubClient::new();
        let results = match client.search_with_fallback(&query).await {
            Ok(r) => r,
            Err(e) => return Ok(json!({ "error": format!("skill search failed: {e}") })),
        };
        let out: Vec<Value> = results
            .into_iter()
            .take(20)
            .map(|r| {
                json!({
                    "slug": r.slug,
                    "description": r.description,
                    "installs": r.installs,
                    "registry": r.registry,
                })
            })
            .collect();
        Ok(json!({
            "count": out.len(),
            "results": out,
            "hint": "To use one: skill_install(name=\"<slug>\") then skill_use(name=\"<slug>\").",
        }))
    }

    /// `skill_install` -- install a skill from a registry into the global
    /// skills dir. Usable the same turn via `skill_use` (disk fallback); it
    /// folds into the system-prompt skill list on the next compaction/clear/new
    /// reload.
    pub(crate) async fn tool_skill_install(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .or_else(|| args["slug"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if name.is_empty() {
            return Ok(json!({ "error": "name is required" }));
        }
        if !crate::skill::valid_slug(&name) {
            return Ok(json!({ "error": format!("invalid skill name: {name:?}") }));
        }
        // Security gate: the agent AUTO-installs audited, content-pinned skills
        // (allowlist) with no friction. An off-list skill requires explicit
        // user confirmation -- the agent must ask the user (via ask_user or a
        // plain question) whether they trust it, and only on a clear yes retry
        // with confirmed=true.
        let Some(entry) = crate::skill::allowlist::snapshot().lookup_skill(&name) else {
            let confirmed = args["confirmed"].as_bool().unwrap_or(false);
            if !confirmed {
                return Ok(json!({
                    "error": format!("'{name}' is not on the audited auto-install allowlist"),
                    "needs_confirmation": true,
                    "guidance": format!(
                        "'{name}' is not pre-audited. Ask the user explicitly whether they trust \
                         and want to install it. ONLY if they clearly say yes, call skill_install \
                         again with confirmed=true. Do not set confirmed=true on your own."
                    ),
                }));
            }
            // User-confirmed off-allowlist install. Resolve the slug through the
            // normal registry fallback chain (clawhub -> skills.sh -> github ->
            // skillhub -> iwencai); there's no audited URL/hash to pin against,
            // so we trust the user's explicit decision instead.
            let dir = crate::config::loader::base_dir().join("skills");
            let client = crate::skill::clawhub::ClawhubClient::new();
            return match client.install_with_fallback(&name, &dir).await {
                Ok(locked) => Ok(json!({
                    "installed": name,
                    "version": locked.version,
                    "dir": locked.install_dir.display().to_string(),
                    "off_allowlist": true,
                    "note": "Installed off-allowlist with user confirmation (not content-pinned).",
                    "next_step": "Usable now: skill_use(name=\"...\") to read its SKILL.md, then run the CLI it documents via shell.",
                })),
                Err(e) => Ok(json!({ "error": format!("install failed: {e}"), "name": name })),
            };
        };
        if entry.url.is_empty() {
            return Ok(json!({ "error": format!("allowlist entry '{name}' has no download url") }));
        }
        let dir = crate::config::loader::base_dir().join("skills");
        let client = crate::skill::clawhub::ClawhubClient::new();
        // Install ONLY from the audited allowlist URL (a direct https:// spec
        // routes through install_from_url, NOT the public registry search).
        match client.install_with_fallback(&entry.url, &dir).await {
            Ok(locked) => {
                // Content pin: the downloaded SKILL.md must match the audited
                // hash, so a registry can't swap content under an audited slug.
                if let Err(e) =
                    crate::skill::allowlist::verify_skill_content(&locked.install_dir, &entry, true)
                {
                    let _ = std::fs::remove_dir_all(&locked.install_dir);
                    return Ok(json!({ "error": e.to_string(), "name": name }));
                }
                Ok(json!({
                    "installed": name,
                    "version": locked.version,
                    "dir": locked.install_dir.display().to_string(),
                    "next_step": "Usable now: skill_use(name=\"...\") to read its SKILL.md, then run the CLI it documents via shell.",
                }))
            }
            Err(e) => Ok(json!({ "error": format!("install failed: {e}"), "name": name })),
        }
    }

    /// `skill_remove` -- uninstall a locally-installed skill (delete its dir).
    pub(crate) async fn tool_skill_remove(&self, args: Value) -> Result<Value> {
        let name = args["name"]
            .as_str()
            .or_else(|| args["slug"].as_str())
            .unwrap_or("")
            .trim()
            .to_owned();
        if name.is_empty() {
            return Ok(json!({ "error": "name is required" }));
        }
        // Path-traversal guard: `name` is model-controlled and joined onto the
        // skills root, then `remove_dir_all`'d. Reject anything non-slug.
        if !crate::skill::valid_slug(&name) {
            return Ok(json!({ "error": format!("invalid skill name: {name:?}") }));
        }
        let dir = crate::config::loader::base_dir().join("skills").join(&name);
        if !dir.is_dir() {
            return Ok(json!({ "error": format!("skill '{name}' not installed") }));
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(_) => Ok(json!({
                "removed": name,
                "note": "Removed from disk. Drops from the system-prompt skill list on the next compaction/clear/new reload.",
            })),
            Err(e) => Ok(json!({ "error": format!("remove failed: {e}") })),
        }
    }
}
