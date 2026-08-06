//! Codex plugin loader: parses `.codex-plugin/plugin.json`, scans `skills/*.md`,
//! and reads `.mcp.json` into the existing MCP/skill infrastructure.
//!
//! Codex plugins are static content bundles — no subprocess runtime needed.
//! They provide skills (markdown prompt templates) and MCP server declarations.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use rsclaw_config::schema::McpServerConfig;
use serde::Deserialize;

const MAX_SKILL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct CodexManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Relative path to the skills directory (e.g. "./skills/").
    #[serde(default = "default_skills_dir")]
    pub skills: String,
    /// Relative path to the MCP config file.
    #[serde(default = "default_mcp_path", rename = "mcpServers")]
    pub mcp_servers: String,
}

fn default_skills_dir() -> String {
    "./skills/".to_owned()
}

fn default_mcp_path() -> String {
    "./.mcp.json".to_owned()
}

/// A parsed skill from a `.md` file inside a codex plugin.
#[derive(Debug, Clone)]
pub struct CodexSkill {
    /// Skill name, namespaced as `codex:<stem>`.
    pub name: String,
    /// Description from frontmatter or first non-empty line.
    pub description: String,
    /// Markdown body used as the prompt template.
    pub template: String,
}

/// A fully loaded Codex plugin: manifest + parsed skills + MCP server configs.
#[derive(Debug, Clone)]
pub struct CodexPlugin {
    pub manifest: CodexManifest,
    pub skills: Vec<CodexSkill>,
    pub mcp_servers: Vec<McpServerConfig>,
    /// Absolute path to the plugin directory.
    pub dir: std::path::PathBuf,
}

#[derive(Debug, Deserialize)]
struct McpJson {
    #[serde(default, rename = "mcpServers")]
    mcp_servers: BTreeMap<String, McpJsonServer>,
}

#[derive(Debug, Deserialize)]
struct McpJsonServer {
    #[serde(default)]
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

impl CodexPlugin {
    /// Load a Codex plugin from `dir` (the directory containing `.codex-plugin/`).
    pub fn load(dir: &Path) -> Result<Self> {
        let manifest = Self::load_manifest(dir)?;
        let skills = Self::load_skills(dir, &manifest)?;
        let mcp_servers = Self::load_mcp(dir, &manifest)?;
        Ok(Self {
            manifest,
            skills,
            mcp_servers,
            dir: dir.to_path_buf(),
        })
    }

    /// Check whether a directory looks like a codex plugin.
    pub fn is_codex_plugin(dir: &Path) -> bool {
        dir.join(".codex-plugin").join("plugin.json").exists()
    }

    fn load_manifest(dir: &Path) -> Result<CodexManifest> {
        let path = dir.join(".codex-plugin").join("plugin.json");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read codex plugin.json: {}", path.display()))?;
        serde_json::from_str(&raw)
            .with_context(|| format!("parse codex plugin.json: {}", path.display()))
    }

    fn load_skills(dir: &Path, manifest: &CodexManifest) -> Result<Vec<CodexSkill>> {
        let skills_dir = dir.join(&manifest.skills);
        let Ok(rd) = std::fs::read_dir(&skills_dir) else {
            return Ok(Vec::new());
        };
        let mut paths: Vec<_> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();

        let mut skills = Vec::new();
        for path in &paths {
            let Some(raw) = read_capped(path, MAX_SKILL_BYTES) else {
                continue;
            };
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed")
                .to_owned();
            let name = format!("codex:{stem}");

            let skill = if let Some(parsed) = parse_frontmatter_skill(&raw, &name) {
                parsed
            } else {
                let description = raw
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                CodexSkill {
                    name,
                    description,
                    template: raw.trim().to_owned(),
                }
            };
            skills.push(skill);
        }
        Ok(skills)
    }

    fn load_mcp(dir: &Path, manifest: &CodexManifest) -> Result<Vec<McpServerConfig>> {
        let path = dir.join(&manifest.mcp_servers);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };
        let parsed: McpJson = serde_json::from_str(&raw)
            .with_context(|| format!("parse .mcp.json: {}", path.display()))?;
        Ok(parsed
            .mcp_servers
            .into_iter()
            .map(|(name, server)| {
                let command = resolve_command(&server.command, dir);
                McpServerConfig {
                    name,
                    command,
                    args: if server.args.is_empty() {
                        None
                    } else {
                        Some(server.args)
                    },
                    env: if server.env.is_empty() {
                        None
                    } else {
                        Some(server.env.into_iter().collect())
                    },
                }
            })
            .collect())
    }
}

/// Resolve a server `command` for direct execution. Bare commands are left for
/// PATH lookup; path-like references that are relative are resolved against the
/// plugin directory so a plugin can bundle its own server script.
fn resolve_command(command: &str, plugin_dir: &Path) -> String {
    if command.contains('/') && !Path::new(command).is_absolute() {
        let rel = command.strip_prefix("./").unwrap_or(command);
        plugin_dir.join(rel).to_string_lossy().into_owned()
    } else {
        command.to_owned()
    }
}

fn read_capped(path: &Path, max: u64) -> Option<String> {
    use std::io::Read as _;
    let f = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    f.take(max).read_to_end(&mut bytes).ok()?;
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            if e.utf8_error().error_len().is_some() {
                return None;
            }
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            String::from_utf8(b).ok()?
        }
    };
    Some(text)
}

/// Attempt to parse a skill file with YAML frontmatter (`---\n...\n---\n`).
fn parse_frontmatter_skill(raw: &str, fallback_name: &str) -> Option<CodexSkill> {
    let rest = raw.strip_prefix("---\n")?;
    let (front, body) = rest.split_once("\n---\n")?;
    let mut fm_name = String::new();
    let mut description = String::new();
    for line in front.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            fm_name = v.trim().to_owned();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().to_owned();
        }
    }
    let final_name = if fm_name.is_empty() {
        fallback_name.to_owned()
    } else {
        format!("codex:{fm_name}")
    };
    Some(CodexSkill {
        name: final_name,
        description,
        template: body.trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".codex-plugin");
        fs::create_dir_all(&plugin_dir).expect("mkdir");
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "test-plugin", "version": "0.1.0", "description": "A test plugin", "skills": "./skills/"}"#,
        )
        .expect("write manifest");

        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(&skills_dir).expect("mkdir skills");
        fs::write(
            skills_dir.join("review.md"),
            "---\nname: review\ndescription: Review code\n---\nReview the code for bugs.",
        )
        .expect("write review skill");
        fs::write(
            skills_dir.join("plain.md"),
            "When writing tests, always use descriptive names.",
        )
        .expect("write plain skill");

        fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers": {"github": {"command": "npx", "args": ["-y", "@mcp/github"], "env": {"TOKEN": "abc"}}}}"#,
        )
        .expect("write mcp.json");
        dir
    }

    #[test]
    fn loads_manifest() {
        let dir = fixture_dir();
        let plugin = CodexPlugin::load(dir.path()).expect("load");
        assert_eq!(plugin.manifest.name, "test-plugin");
        assert_eq!(plugin.manifest.version, "0.1.0");
    }

    #[test]
    fn loads_skills_with_frontmatter_and_plain() {
        let dir = fixture_dir();
        let plugin = CodexPlugin::load(dir.path()).expect("load");
        assert_eq!(plugin.skills.len(), 2);

        let plain = plugin
            .skills
            .iter()
            .find(|s| s.name == "codex:plain")
            .expect("plain skill");
        assert_eq!(
            plain.description,
            "When writing tests, always use descriptive names."
        );

        let review = plugin
            .skills
            .iter()
            .find(|s| s.name == "codex:review")
            .expect("review skill");
        assert_eq!(review.description, "Review code");
        assert_eq!(review.template, "Review the code for bugs.");
    }

    #[test]
    fn loads_mcp_servers() {
        let dir = fixture_dir();
        let plugin = CodexPlugin::load(dir.path()).expect("load");
        assert_eq!(plugin.mcp_servers.len(), 1);
        let server = &plugin.mcp_servers[0];
        assert_eq!(server.name, "github");
        assert_eq!(server.command, "npx");
        assert_eq!(
            server.args.as_deref(),
            Some(&["-y".to_owned(), "@mcp/github".to_owned()][..])
        );
        assert_eq!(
            server.env.as_ref().unwrap().get("TOKEN").map(|s| s.as_str()),
            Some("abc")
        );
    }

    #[test]
    fn missing_mcp_json_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".codex-plugin");
        fs::create_dir_all(&plugin_dir).expect("mkdir");
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "bare", "version": "1.0.0", "description": ""}"#,
        )
        .expect("write manifest");
        let plugin = CodexPlugin::load(dir.path()).expect("load");
        assert!(plugin.mcp_servers.is_empty());
        assert!(plugin.skills.is_empty());
    }

    #[test]
    fn missing_manifest_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(CodexPlugin::load(dir.path()).is_err());
    }

    #[test]
    fn read_capped_survives_cjk_cut_mid_character() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cjk.md");
        fs::write(&path, "你好世界").expect("write cjk");
        let text = read_capped(&path, 4).expect("capped read must succeed");
        assert_eq!(text, "你");
    }

    #[test]
    fn read_capped_rejects_interior_invalid_utf8() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("binary.md");
        fs::write(&path, [0xFF, 0xFE, 0x00, 0x01]).expect("write binary");
        assert!(read_capped(&path, 64).is_none());
    }

    #[test]
    fn resolve_command_leaves_bare_commands_for_path_lookup() {
        let dir = Path::new("/plugins/demo");
        assert_eq!(resolve_command("npx", dir), "npx");
        assert_eq!(resolve_command("python3", dir), "python3");
    }

    #[test]
    fn resolve_command_resolves_relative_paths_against_plugin_dir() {
        let dir = Path::new("/plugins/demo");
        assert_eq!(
            resolve_command("./scripts/server.py", dir),
            "/plugins/demo/scripts/server.py"
        );
        assert_eq!(
            resolve_command("bin/server", dir),
            "/plugins/demo/bin/server"
        );
    }

    #[test]
    fn resolve_command_keeps_absolute_paths() {
        let dir = Path::new("/plugins/demo");
        assert_eq!(resolve_command("/usr/bin/server", dir), "/usr/bin/server");
    }

    #[test]
    fn manifest_mcp_servers_path_is_honored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plugin_dir = dir.path().join(".codex-plugin");
        fs::create_dir_all(&plugin_dir).expect("mkdir");
        fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "nested", "version": "1.0.0", "description": "", "mcpServers": "./config/servers.json"}"#,
        )
        .expect("write manifest");
        let config_dir = dir.path().join("config");
        fs::create_dir_all(&config_dir).expect("mkdir config");
        fs::write(
            config_dir.join("servers.json"),
            r#"{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "@mcp/fs"]}}}"#,
        )
        .expect("write servers");
        let plugin = CodexPlugin::load(dir.path()).expect("load");
        assert_eq!(plugin.mcp_servers.len(), 1);
        assert_eq!(plugin.mcp_servers[0].name, "fs");
    }

    #[test]
    fn is_codex_plugin_detects_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!CodexPlugin::is_codex_plugin(dir.path()));
        let marker = dir.path().join(".codex-plugin");
        fs::create_dir_all(&marker).expect("mkdir");
        fs::write(marker.join("plugin.json"), r#"{"name":"x"}"#).expect("write");
        assert!(CodexPlugin::is_codex_plugin(dir.path()));
    }
}
