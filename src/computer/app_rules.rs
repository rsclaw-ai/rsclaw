//! App-rules markdown loader. Each `tools/computer_use/app-rules/*.md`
//! describes one app's automation knowledge: triggers (keywords that
//! should activate it), the body (instructions / tips / pitfalls injected
//! into the system prompt).
//!
//! Frontmatter schema (existing files match this approximately):
//!     ---
//!     name: wechat
//!     triggers: [wechat, 微信, weixin]      # OR derive from name+description
//!     description: ...
//!     ---
//!     <markdown body>
//!
//! Behavior:
//!   - Discover `.md` files under the app-rules dir at process start
//!     (and when explicitly reloaded). Hot-reload is NOT a goal — the
//!     dir is meant to be edited offline.
//!   - For each request, match user-instruction keywords against
//!     `triggers` of every loaded rule (case-insensitive substring).
//!   - Return matched bodies in declaration order so the prompt
//!     injection is deterministic.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone)]
pub struct AppRule {
    pub name: String,
    pub triggers: Vec<String>,
    pub description: Option<String>,
    pub body: String,
    pub path: PathBuf,
}

#[derive(Debug, Default, Clone)]
pub struct AppRuleSet {
    pub rules: Vec<AppRule>,
}

impl AppRuleSet {
    /// Scan a directory of `.md` files. Each file is expected to start
    /// with a YAML-style frontmatter block. Files without frontmatter
    /// are skipped with a warning. An empty or missing directory yields
    /// an empty rule set.
    pub fn load_dir(dir: &Path) -> Result<Self> {
        if !dir.exists() {
            return Ok(Self::default());
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        let entries =
            std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            paths.push(path);
        }
        // Sort for deterministic order across platforms (read_dir order is OS-dependent).
        paths.sort();

        let mut rules = Vec::new();
        for path in paths {
            match parse_file(&path) {
                Ok(Some(rule)) => rules.push(rule),
                Ok(None) => {
                    warn!(
                        path = %path.display(),
                        "app-rule file has no frontmatter, skipping"
                    );
                }
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to parse app-rule file, skipping"
                    );
                }
            }
        }
        Ok(Self { rules })
    }

    /// Match a user instruction against loaded rule triggers. A rule
    /// matches when any of its triggers is found as a case-insensitive
    /// substring of the instruction. Returns matches in declaration
    /// order.
    pub fn match_instruction(&self, instruction: &str) -> Vec<&AppRule> {
        let lower = instruction.to_lowercase();
        let mut matched = Vec::new();
        for rule in &self.rules {
            for trigger in &rule.triggers {
                let t = trigger.to_lowercase();
                if t.is_empty() {
                    continue;
                }
                if lower.contains(&t) {
                    matched.push(rule);
                    break;
                }
            }
        }
        matched
    }
}

/// Parse a single markdown file into an `AppRule`. Returns `Ok(None)` if
/// the file lacks a frontmatter block.
fn parse_file(path: &Path) -> Result<Option<AppRule>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read file {}", path.display()))?;
    let Some((frontmatter, body)) = split_frontmatter(&text) else {
        return Ok(None);
    };

    let (name_opt, description, fm_triggers) = parse_frontmatter(frontmatter);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let name = name_opt.unwrap_or_else(|| stem.clone());

    let mut triggers: Vec<String> = Vec::new();
    let push_unique = |t: String, list: &mut Vec<String>| {
        let t = t.trim().to_string();
        if t.is_empty() {
            return;
        }
        let lower = t.to_lowercase();
        if !list.iter().any(|x| x.to_lowercase() == lower) {
            list.push(t);
        }
    };
    push_unique(name.clone(), &mut triggers);
    if !stem.is_empty() {
        push_unique(stem.clone(), &mut triggers);
    }
    for t in fm_triggers {
        push_unique(t, &mut triggers);
    }
    for alias in canonical_aliases(&name).into_iter().chain(canonical_aliases(&stem)) {
        push_unique(alias.to_string(), &mut triggers);
    }

    Ok(Some(AppRule {
        name,
        triggers,
        description,
        body: body.to_string(),
        path: path.to_path_buf(),
    }))
}

/// Split a markdown text into `(frontmatter_body, rest)`. Recognises a
/// leading `---` line, a closing `---` line, and returns the content in
/// between plus everything after. Returns `None` when no valid
/// frontmatter block is present.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    // Strip a UTF-8 BOM if present.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---")?;
    // Must be followed by a newline (no inline content on the opening fence).
    let rest = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n"))?;
    // Find the closing "---" on its own line.
    let mut search_from = 0usize;
    while let Some(idx) = rest[search_from..].find("---") {
        let abs = search_from + idx;
        let line_start_ok = abs == 0 || rest.as_bytes()[abs - 1] == b'\n';
        if !line_start_ok {
            search_from = abs + 3;
            continue;
        }
        let after = &rest[abs + 3..];
        // The line must end here (newline or EOF).
        let body_after = if let Some(b) = after.strip_prefix('\n') {
            b
        } else if let Some(b) = after.strip_prefix("\r\n") {
            b
        } else if after.is_empty() {
            after
        } else {
            search_from = abs + 3;
            continue;
        };
        let frontmatter = &rest[..abs];
        return Some((frontmatter, body_after));
    }
    None
}

/// Parse a frontmatter block into `(name, description, triggers)`.
/// Accepts `key: value` lines and an optional `triggers: [a, b, c]`
/// inline list. Lines that are not recognised are ignored.
fn parse_frontmatter(fm: &str) -> (Option<String>, Option<String>, Vec<String>) {
    let mut name = None;
    let mut description = None;
    let mut triggers = Vec::new();
    for line in fm.lines() {
        let line = line.trim_end_matches('\r');
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim().to_lowercase();
        let value = value.trim();
        match key.as_str() {
            "name" => name = Some(strip_quotes(value).to_string()),
            "description" => description = Some(strip_quotes(value).to_string()),
            "triggers" => {
                triggers = parse_inline_list(value);
            }
            _ => {}
        }
    }
    (name, description, triggers)
}

/// Parse a YAML-flow inline list like `[a, b, "c d"]`. Whitespace and
/// surrounding quotes on each item are stripped. A non-list value is
/// treated as a single item.
fn parse_inline_list(value: &str) -> Vec<String> {
    let v = value.trim();
    if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        inner
            .split(',')
            .map(|s| strip_quotes(s.trim()).to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if v.is_empty() {
        Vec::new()
    } else {
        vec![strip_quotes(v).to_string()]
    }
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Canonical aliases for known apps. Returns an empty slice for unknown
/// names. Lookup is case-insensitive on the input.
fn canonical_aliases(name: &str) -> &'static [&'static str] {
    match name.to_lowercase().as_str() {
        "wechat" => &["wechat", "微信", "weixin"],
        "doubao" => &["doubao", "豆包"],
        "douyin" => &["douyin", "抖音", "tiktok"],
        "tonghuashun" => &["tonghuashun", "同花顺"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, content).expect("write test file");
        p
    }

    const WECHAT_MD: &str = "---\n\
name: wechat\n\
description: WeChat (微信) desktop client automation\n\
---\n\
\n\
# WeChat\n\
Body content here.\n";

    #[test]
    fn loads_wechat_rule_with_canonical_triggers() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "wechat.md", WECHAT_MD);
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        assert_eq!(set.rules.len(), 1);
        let r = &set.rules[0];
        assert_eq!(r.name, "wechat");
        let lower: Vec<String> = r.triggers.iter().map(|s| s.to_lowercase()).collect();
        assert!(lower.iter().any(|s| s == "wechat"), "triggers: {:?}", r.triggers);
        assert!(lower.iter().any(|s| s == "微信"), "triggers: {:?}", r.triggers);
        assert!(lower.iter().any(|s| s == "weixin"), "triggers: {:?}", r.triggers);
        assert!(r.body.contains("Body content here"));
        assert_eq!(r.description.as_deref(), Some("WeChat (微信) desktop client automation"));
    }

    #[test]
    fn skips_files_without_frontmatter() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "wechat.md", WECHAT_MD);
        write(dir.path(), "broken.md", "no frontmatter here\njust text\n");
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        assert_eq!(set.rules.len(), 1);
        assert_eq!(set.rules[0].name, "wechat");
    }

    #[test]
    fn match_english_substring() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "wechat.md", WECHAT_MD);
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        let m = set.match_instruction("send a wechat message");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "wechat");
    }

    #[test]
    fn match_cjk_substring() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "wechat.md", WECHAT_MD);
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        let m = set.match_instruction("微信群里看看");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "wechat");
    }

    #[test]
    fn no_match_returns_empty() {
        let dir = tempdir().expect("tempdir");
        write(dir.path(), "wechat.md", WECHAT_MD);
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        let m = set.match_instruction("buy stocks");
        assert!(m.is_empty());
    }

    #[test]
    fn empty_dir_returns_empty_set() {
        let dir = tempdir().expect("tempdir");
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        assert!(set.rules.is_empty());
    }

    #[test]
    fn explicit_triggers_field_in_frontmatter() {
        let dir = tempdir().expect("tempdir");
        let content = "---\n\
name: myapp\n\
triggers: [foo, bar, \"baz qux\"]\n\
---\n\
body\n";
        write(dir.path(), "myapp.md", content);
        let set = AppRuleSet::load_dir(dir.path()).expect("load");
        assert_eq!(set.rules.len(), 1);
        let r = &set.rules[0];
        let lower: Vec<String> = r.triggers.iter().map(|s| s.to_lowercase()).collect();
        assert!(lower.contains(&"foo".to_string()));
        assert!(lower.contains(&"bar".to_string()));
        assert!(lower.contains(&"baz qux".to_string()));
    }
}
