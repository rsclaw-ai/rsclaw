//! Skill directory scanner and registry builder.
//!
//! Scanning order (later entries override earlier ones for the same slug):
//!   1. Global skill dir:    `~/.rsclaw/skills/<slug>/SKILL.md`
//!   2. Workspace skill dir: `<workspace>/skills/<slug>/SKILL.md`
//!
//! A skill is enabled by default unless `skills.entries.<slug>.enabled = false`
//! in the config.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::manifest::{SkillManifest, parse_skill_md};
use crate::config::schema::SkillsConfig;

// ---------------------------------------------------------------------------
// SkillRegistry
// ---------------------------------------------------------------------------

/// Loaded and enabled skills, indexed by slug.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, SkillManifest>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a skill by its slug name.
    pub fn get(&self, name: &str) -> Option<&SkillManifest> {
        self.skills.get(name)
    }

    /// All enabled skills.
    pub fn all(&self) -> impl Iterator<Item = &SkillManifest> {
        self.skills.values()
    }

    /// Number of loaded skills.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Insert or overwrite a skill entry.
    pub fn insert(&mut self, manifest: SkillManifest) {
        self.skills.insert(manifest.name.clone(), manifest);
    }
}

// ---------------------------------------------------------------------------
// Loader
// ---------------------------------------------------------------------------

/// Scan skill directories and build a `SkillRegistry`.
///
/// `global_skills_dir`    — typically `~/.rsclaw/skills/`
/// `workspace_skills_dir` — `<workspace>/skills/` (may be `None`)
/// `config`               — the `skills` section of `RuntimeConfig`
pub fn load_skills(
    global_skills_dir: &Path,
    workspace_skills_dir: Option<&Path>,
    config: Option<&SkillsConfig>,
) -> Result<SkillRegistry> {
    let mut registry = SkillRegistry::new();

    // 1. Global skills
    scan_dir(global_skills_dir, &mut registry)?;

    // 2. Workspace skills override global ones.
    if let Some(ws_dir) = workspace_skills_dir {
        scan_dir(ws_dir, &mut registry)?;
    }

    // 3. Apply enable/disable from config.
    if let Some(cfg) = config
        && let Some(entries) = &cfg.entries
    {
        let disabled: Vec<String> = entries
            .iter()
            .filter(|(_, e)| e.enabled == Some(false))
            .map(|(k, _)| k.clone())
            .collect();
        for slug in disabled {
            if registry.skills.remove(&slug).is_some() {
                debug!(slug, "skill disabled via config");
            }
        }
    }

    tracing::debug!(count = registry.len(), "skills loaded");
    Ok(registry)
}

/// Scan a single directory for skill sub-directories.
fn scan_dir(dir: &Path, registry: &mut SkillRegistry) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read skill dir: {}", dir.display()))?;

    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }

        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            debug!(path = %skill_dir.display(), "skipping: no SKILL.md");
            continue;
        }

        match parse_skill_md(&skill_md) {
            Ok(manifest) => {
                debug!(name = %manifest.name, dir = %skill_dir.display(), "skill loaded");
                registry.insert(manifest);
            }
            Err(e) => {
                warn!(path = %skill_md.display(), "failed to parse SKILL.md: {e:#}");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Default global skill directory: `~/.rsclaw/skills/`.
pub fn default_global_skills_dir() -> Option<PathBuf> {
    dirs_next::home_dir().map(|h| h.join(".rsclaw/skills"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn make_skill(base: &Path, slug: &str, extra_tools: bool) {
        let dir = base.join(slug);
        fs::create_dir_all(&dir).expect("mkdir");
        let tools_section = if extra_tools {
            r#"tools:
  - name: run
    description: Run something
    command: ./run.sh
"#
        } else {
            ""
        };
        fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {slug}\ndescription: Test skill {slug}\n{tools_section}---\n\n# {slug}\n"
            ),
        )
        .expect("write SKILL.md");
    }

    #[test]
    fn scan_global_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        make_skill(tmp.path(), "alpha", false);
        make_skill(tmp.path(), "beta", true);

        let registry = load_skills(tmp.path(), None, None).expect("load");
        assert_eq!(registry.len(), 2);
        assert!(registry.get("alpha").is_some());
        assert!(registry.get("beta").is_some());
        assert_eq!(registry.get("beta").expect("beta").tools.len(), 1);
    }

    #[test]
    fn workspace_overrides_global() {
        let global = tempfile::tempdir().expect("tempdir");
        let workspace = tempfile::tempdir().expect("tempdir");

        make_skill(global.path(), "shared", false);
        // Workspace version has a tool, global doesn't.
        make_skill(workspace.path(), "shared", true);

        let registry = load_skills(global.path(), Some(workspace.path()), None).expect("load");
        assert_eq!(registry.len(), 1);
        let skill = registry.get("shared").expect("shared");
        assert_eq!(skill.tools.len(), 1, "workspace version should win");
    }

    #[test]
    fn config_disables_skill() {
        use crate::config::schema::{SkillEntryConfig, SkillsConfig};

        let tmp = tempfile::tempdir().expect("tempdir");
        make_skill(tmp.path(), "gamma", false);
        make_skill(tmp.path(), "delta", false);

        let mut entries = std::collections::HashMap::new();
        entries.insert(
            "gamma".to_owned(),
            SkillEntryConfig {
                enabled: Some(false),
                extra: Default::default(),
            },
        );

        let cfg = SkillsConfig {
            install: None,
            entries: Some(entries),
        };

        let registry = load_skills(tmp.path(), None, Some(&cfg)).expect("load");
        assert_eq!(registry.len(), 1);
        assert!(registry.get("delta").is_some());
        assert!(registry.get("gamma").is_none(), "gamma should be disabled");
    }
}
