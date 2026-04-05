//! OpenClaw data migration module.
//!
//! Provides three migration modes for transitioning from OpenClaw to rsclaw:
//! - `Seamless`: coexist with OpenClaw, sharing base directory
//! - `Import`: copy all OpenClaw data into rsclaw's own stores
//! - `New`: ignore OpenClaw data entirely, start new
//!
//! OpenClaw stores data as JSONL files, not SQLite. The directory layout is:
//!   ~/.openclaw/
//!     openclaw.json                                # Main config
//!     agents/<agent_id>/sessions/sessions.json     # Session index
//!     agents/<agent_id>/sessions/<uuid>.jsonl      # Session messages
//!     agents/<agent_id>/agent/auth-profiles.json   # API keys

pub mod openclaw;

use std::path::PathBuf;

use tracing::debug;

// ---------------------------------------------------------------------------
// MigrateMode
// ---------------------------------------------------------------------------

/// Controls how rsclaw handles an existing OpenClaw installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateMode {
    /// Use OpenClaw data in-place without copying. RSCLAW_BASE_DIR is set
    /// to the OpenClaw directory. Both cannot run simultaneously.
    Seamless,
    /// Copy sessions, config, workspace from OpenClaw into ~/.rsclaw/,
    /// then operate independently. OpenClaw data is never modified.
    Import,
    /// Ignore OpenClaw data entirely, start new in ~/.rsclaw/.
    New,
}

impl MigrateMode {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "seamless" => Some(Self::Seamless),
            "import" | "migrate" => Some(Self::Import),
            "new" | "fresh" | "clean" => Some(Self::New),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Seamless => "seamless",
            Self::Import => "import",
            Self::New => "new",
        }
    }
}

// ---------------------------------------------------------------------------
// OpenClaw directory detection
// ---------------------------------------------------------------------------

/// Common locations where an OpenClaw installation might live.
const OPENCLAW_DIR_CANDIDATES: &[&str] = &[".openclaw", "bak.openclaw"];

/// Detect the OpenClaw data directory.
/// Checks (in order):
///   1. `OPENCLAW_HOME` environment variable (custom path)
///   2. `OPENCLAW_DIR` environment variable (alias)
///   3. `~/.openclaw/` and `~/bak.openclaw/` default locations
/// Validates by looking for `openclaw.json` or an `agents/` subdirectory.
pub fn detect_openclaw_dir() -> Option<PathBuf> {
    // 1. OPENCLAW_CONFIG_PATH -> derive directory from config file path.
    if let Ok(val) = std::env::var("OPENCLAW_CONFIG_PATH") {
        let config_path = PathBuf::from(&val);
        if config_path.is_file() {
            if let Some(dir) = config_path.parent() {
                debug!(path = %dir.display(), env = "OPENCLAW_CONFIG_PATH", "found OpenClaw via config path env");
                return Some(dir.to_path_buf());
            }
        }
    }

    // 2. OPENCLAW_HOME -> direct directory path.
    if let Ok(val) = std::env::var("OPENCLAW_HOME") {
        let dir = PathBuf::from(val);
        if dir.is_dir() {
            let has_config = dir.join("openclaw.json").is_file();
            let has_agents = dir.join("agents").is_dir();
            if has_config || has_agents {
                debug!(path = %dir.display(), env = "OPENCLAW_HOME", "found OpenClaw via home env");
                return Some(dir);
            }
        }
    }

    // 2. Check default home directory locations.
    let home = dirs_next::home_dir()?;
    for candidate in OPENCLAW_DIR_CANDIDATES {
        let dir = home.join(candidate);
        if dir.is_dir() {
            let has_config = dir.join("openclaw.json").is_file();
            let has_agents = dir.join("agents").is_dir();
            if has_config || has_agents {
                debug!(path = %dir.display(), "found OpenClaw data directory");
                return Some(dir);
            }
        }
    }
    None
}

/// Check whether an OpenClaw installation directory exists.
pub fn openclaw_dir_exists() -> bool {
    detect_openclaw_dir().is_some()
}

/// Check whether the rsclaw data directory already exists.
pub fn rsclaw_dir_exists() -> bool {
    if let Some(home) = dirs_next::home_dir() {
        home.join(".rsclaw").is_dir()
    } else {
        false
    }
}

/// Print a one-line notice when OpenClaw is detected but rsclaw is not yet set up.
/// Returns true if a notice was printed.
pub fn maybe_print_openclaw_notice() -> bool {
    if openclaw_dir_exists() && !rsclaw_dir_exists() {
        println!("  Detected OpenClaw installation. Run `rsclaw migrate` for options.");
        true
    } else {
        false
    }
}

/// Check if rsclaw has been properly set up. Returns true if setup is needed.
///
/// Setup is needed when:
/// - No `~/.rsclaw/` directory exists AND
/// - No `RSCLAW_BASE_DIR` env override AND
/// - Not running with `--profile` or `--dev` (those create their own dirs)
///
/// When setup is needed, prints a message and returns true so the caller
/// can exit early.
pub fn check_needs_setup() -> bool {
    // Resolve base_dir (respects RSCLAW_BASE_DIR from --profile/--dev/--base-dir).
    let base = crate::config::loader::base_dir();
    let config_path = base.join("rsclaw.json5");

    if config_path.is_file() {
        return false;
    }

    // No config found -- tell user to run setup.
    println!();
    if openclaw_dir_exists() {
        println!("  First time? Run `rsclaw setup` to get started.");
        println!("  OpenClaw data detected -- setup will offer to import your data.");
    } else {
        println!("  First time? Run `rsclaw setup` to create config and data directories.");
    }
    println!();
    true
}
