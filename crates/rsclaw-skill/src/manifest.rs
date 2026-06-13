//! SKILL.md manifest parser.
//!
//! A skill is a directory containing at minimum a `SKILL.md` file.
//! The SKILL.md front-matter (YAML fenced block at the top) declares
//! metadata; the rest of the file is the system-prompt injected to
//! the agent when the skill is active.
//!
//! Front-matter format (between `---` fences):
//!
//! ```yaml
//! name: my-skill
//! description: Does something useful
//! version: 1.0.0
//! tools:
//!   - name: do_thing
//!     description: Runs do_thing
//!     command: ./scripts/do_thing.sh
//!     input_schema:
//!       type: object
//!       properties:
//!         arg1: { type: string }
//!       required: [arg1]
//! ```

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Manifest structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    /// Unique skill name (slug).
    pub name: String,
    /// Human-readable description shown to the agent.
    pub description: Option<String>,
    /// Semver string, e.g. "1.2.3".
    pub version: Option<String>,
    /// Optional minimum rsclaw version required.
    pub requires_rsclaw: Option<String>,
    /// List of shell-backed tools exposed to the agent.
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Extra key-value config forwarded from skills.entries.<slug>.
    #[serde(default, flatten)]
    pub extra: HashMap<String, Value>,

    // --- runtime fields (not in SKILL.md) ---
    /// Absolute path to the skill directory.
    #[serde(skip)]
    pub dir: PathBuf,
    /// Full text of SKILL.md (after the front-matter), used as system prompt.
    #[serde(skip)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name exposed to the LLM (snake_case).
    pub name: String,
    /// Description shown to the LLM.
    pub description: String,
    /// Shell command relative to the skill directory, e.g. `./run.sh`.
    pub command: String,
    /// JSON Schema for tool input parameters.
    pub input_schema: Option<Value>,
    /// Timeout in seconds. Defaults to 30.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
    /// Whether to pass the full JSON input via stdin (true) or as CLI args
    /// (false).
    #[serde(default = "default_true")]
    pub stdin_json: bool,
}

fn default_timeout() -> u32 {
    30
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a `SKILL.md` file into a `SkillManifest`.
///
/// Expects optional YAML front-matter between `---` fences at the top
/// of the file. The remainder is the skill system-prompt.
pub fn parse_skill_md(path: &Path) -> Result<SkillManifest> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read SKILL.md: {}", path.display()))?;

    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    let (front_matter, prompt) = split_front_matter(&content);

    let mut manifest: SkillManifest = if let Some(fm) = front_matter {
        serde_yaml_ng::from_str(&fm)
            .with_context(|| format!("YAML front-matter error in {}", path.display()))?
    } else {
        // No front-matter: derive name from directory name.
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_owned();
        SkillManifest {
            name,
            description: None,
            version: None,
            requires_rsclaw: None,
            tools: Vec::new(),
            extra: HashMap::new(),
            dir: PathBuf::new(),
            prompt: String::new(),
        }
    };

    manifest.dir = dir;
    manifest.prompt = prompt.to_owned();

    Ok(manifest)
}

/// Split SKILL.md into `(Option<front_matter_yaml>, body)`.
///
/// Front-matter is the content between the first and second `---` line.
fn split_front_matter(content: &str) -> (Option<String>, &str) {
    let mut lines = content.splitn(3, "---\n");

    // If content starts with `---\n`, the first element is empty.
    match (lines.next(), lines.next(), lines.next()) {
        (Some(""), Some(fm), Some(rest)) => (Some(fm.to_owned()), rest),
        _ => (None, content),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn parse_full_skill_md() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("SKILL.md");

        std::fs::write(
            &path,
            r#"---
name: demo-skill
description: A demo skill
version: "1.0.0"
tools:
  - name: greet
    description: Say hello
    command: ./greet.sh
    timeout_seconds: 10
---

# Demo Skill

Use this skill to greet people.
"#,
        )
        .expect("write");

        let m = parse_skill_md(&path).expect("parse");
        assert_eq!(m.name, "demo-skill");
        assert_eq!(m.version.as_deref(), Some("1.0.0"));
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.tools[0].name, "greet");
        assert!(m.prompt.contains("Demo Skill"));
    }

    #[test]
    fn parse_skill_md_no_frontmatter() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("SKILL.md");
        std::fs::write(&path, "# Minimal\nJust some text.").expect("write");

        let m = parse_skill_md(&path).expect("parse");
        assert!(m.prompt.contains("Minimal"));
        assert!(m.tools.is_empty());
    }
}
