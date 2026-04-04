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

pub mod clawhub;
pub mod loader;
pub mod manifest;
pub mod runner;

pub use clawhub::{ClawhubClient, LockFile, LockedSkill, SearchResult, SkillSource};
pub use loader::{SkillRegistry, default_global_skills_dir, load_skills};
pub use manifest::{SkillManifest, ToolSpec, parse_skill_md};
pub use runner::{RunOptions, run_tool};
