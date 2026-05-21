//! Skill subsystem.
//!
//! A *skill* is a directory containing a `SKILL.md` manifest + optional
//! shell scripts. Skills extend the agent with shell-backed tools.
//!
//! Public API surface:
//!   - `SkillRegistry`  — loaded skills, keyed by slug
//!   - `load_skills()`  — scan directories, apply config enable/disable
//!   - `run_tool()`     — execute a skill tool command
//!   - `ClawhubClient`  — download/install skills from clawhub.ai
//!   - `LockFile`       — `.clawhub/lock.json` read/write

pub mod allowlist;
pub mod clawhub;
pub mod crystallizer;
pub mod loader;
pub mod manifest;
pub mod registry;
pub mod runner;
pub mod workflow_distill;

pub use clawhub::{ClawhubClient, LockFile, LockedSkill, SearchResult, SkillSource};
pub use loader::{SkillRegistry, default_global_skills_dir, load_skills};
pub use manifest::{SkillManifest, ToolSpec, parse_skill_md};
pub use runner::{RunOptions, run_tool};

/// Validate a skill slug that will be joined onto the on-disk skills root to
/// build a path. Rejects anything that could escape the root — path separators,
/// `..`, leading dots, absolute paths — by restricting to a conservative
/// charset: `^[a-z0-9][a-z0-9._-]{0,127}$`. The agent-facing `skill_*` tools
/// take a model-controlled `name`; without this gate `skill_remove(name =
/// "../../.ssh")` would `remove_dir_all` outside the skills directory.
pub fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 128 {
        return false;
    }
    // First char must be alphanumeric — rules out leading `.` (hidden / `..`),
    // `-`, `_`, `/`, and absolute paths.
    if !(b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) {
        return false;
    }
    b.iter()
        .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::valid_slug;

    #[test]
    fn slug_accepts_normal_names() {
        assert!(valid_slug("hithink-market-query"));
        assert!(valid_slug("a"));
        assert!(valid_slug("skill_1.2"));
        assert!(valid_slug("brave-search"));
    }

    #[test]
    fn slug_rejects_traversal_and_separators() {
        assert!(!valid_slug(""), "empty");
        assert!(!valid_slug(".."), "dotdot");
        assert!(!valid_slug("../../.ssh"), "traversal");
        assert!(!valid_slug("foo/bar"), "slash");
        assert!(!valid_slug("/etc/passwd"), "absolute");
        assert!(!valid_slug(".hidden"), "leading dot");
        assert!(!valid_slug("UPPER"), "uppercase");
        assert!(!valid_slug("a b"), "space");
        assert!(!valid_slug(&"x".repeat(129)), "too long");
    }
}
