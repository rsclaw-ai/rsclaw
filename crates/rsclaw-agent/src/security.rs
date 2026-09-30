//! File safety checks — block dangerous reads, writes, and file content.
//!
//! Extracted from `runtime.rs` to reduce file size.
//!
//! Model-supplied paths are untrusted (prompt injection, non-owner senders).
//! Writes are confined to an allowlist of roots (the agent workspace plus
//! owner-configured extras) via [`rsclaw_util::fs_guard::resolve_within`];
//! the sensitive-name denylist below is a second layer on top of that.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use crate::trust::SenderTrust;

/// Hard cap for local files a tool reads fully into memory (read_file text
/// path, OCR / image / video inputs).
pub(crate) const MAX_LOCAL_READ_BYTES: u64 = 20 * 1024 * 1024;

/// Cap for binary documents (PDF / Office / media assets) read into memory.
pub(crate) const MAX_LOCAL_BINARY_READ_BYTES: u64 = 50 * 1024 * 1024;

/// Env var holding extra write roots (OS path-list syntax) that owners may
/// write to in addition to the agent workspace.
pub(crate) const WRITE_ROOTS_ENV: &str = "RSCLAW_WRITE_ROOTS";

/// Extra owner write roots from [`WRITE_ROOTS_ENV`].
pub(crate) fn extra_write_roots() -> Vec<PathBuf> {
    std::env::var_os(WRITE_ROOTS_ENV)
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default()
}

/// Resolve a model-supplied write target against `roots` (first match wins).
/// `~` is expanded first so `~/x` is judged by where it really points.
/// Fails when the path escapes every root (absolute paths elsewhere, `..`,
/// symlinks leaving the root, Windows drive paths, ...).
pub(crate) fn resolve_write_path(path: &str, roots: &[PathBuf]) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("[blocked] empty write path");
    }
    let expanded = rsclaw_util::expand_tilde(trimmed);
    let expanded_str = expanded.to_string_lossy();
    let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
    rsclaw_util::fs_guard::resolve_within_any(&root_refs, &expanded_str).map_err(|_| {
        anyhow!(
            "[blocked] write target `{path}` is outside the agent workspace. Use a path relative to the workspace{}.",
            if roots.len() > 1 {
                " (or inside an allowed write root)"
            } else {
                ""
            }
        )
    })
}

/// Check write safety on an already-resolved target `full`:
/// 1. Block sensitive filenames / directories (shell rc files, ssh keys,
///    autostart entries, rsclaw config and credentials, git hooks)
/// 2. Scan executable-script content for dangerous commands
///
/// Path confinement is done by [`resolve_write_path`]; this is the second
/// layer applied to whatever it returned.
pub(crate) fn check_write_safety(path: &str, full: &Path, content: &str) -> Result<()> {
    if let Some(reason) = sensitive_write_reason(full) {
        bail!("[blocked] write to sensitive location ({reason}): {path}");
    }

    // Scan executable-script content for dangerous commands. Safety rules
    // must be enabled here: `PreParseEngine::load()` builds an engine with
    // safety off, whose checks always return Allow. Only script-like files
    // are scanned — the rules are shell-command regexes (`\bshutdown\b`,
    // `\bsudo\b`, ...) that would reject ordinary prose and source code.
    if !content.is_empty() && is_script_like(full, content) {
        let preparse = crate::preparse::PreParseEngine::load_with_safety(true);
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("//")
                || trimmed.starts_with("--")
            {
                continue;
            }
            if let crate::preparse::SafetyCheck::Deny(reason) = preparse.check_exec_safety(trimmed)
            {
                bail!("[blocked] file contains dangerous command: {reason}");
            }
        }
    }

    Ok(())
}

/// True for files a shell or the OS will execute: shell / batch script
/// extensions, or any content with a `#!` shebang.
pub(crate) fn is_script_like(full: &Path, content: &str) -> bool {
    const SCRIPT_EXTS: &[&str] = &[
        "sh", "bash", "zsh", "fish", "ksh", "csh", "command", "ps1", "psm1", "bat", "cmd",
    ];
    let ext_hit = full
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SCRIPT_EXTS.contains(&e.to_ascii_lowercase().as_str()));
    ext_hit || content.trim_start().starts_with("#!")
}

/// Why writing to `full` is refused, or `None` when it is fine.
fn sensitive_write_reason(full: &Path) -> Option<&'static str> {
    const SENSITIVE_NAMES: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".bash_logout",
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".zlogin",
        ".zlogout",
        ".profile",
        ".login",
        ".cshrc",
        ".tcshrc",
        ".inputrc",
        ".gitconfig",
        ".git-credentials",
        ".netrc",
        ".npmrc",
        ".pypirc",
        "authorized_keys",
        "authorized_keys2",
        "known_hosts",
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "crontab",
        ".env",
        "openclaw.json",
        "rsclaw.json5",
        "auth-profiles.json",
    ];
    const SENSITIVE_DIRS: &[&str] = &[
        ".ssh",
        ".gnupg",
        ".aws",
        ".azure",
        ".kube",
        ".docker",
        "launchagents",
        "launchdaemons",
        "autostart",
        "credentials",
    ];

    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if SENSITIVE_NAMES.contains(&filename.as_str()) || filename.starts_with(".env.") {
        return Some("sensitive file name");
    }

    let comps: Vec<String> = full
        .components()
        .filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    // Only directory components (the file name itself was checked above).
    let dir_count = comps.len().saturating_sub(1);
    for (i, c) in comps.iter().take(dir_count).enumerate() {
        if SENSITIVE_DIRS.contains(&c.as_str()) {
            return Some("sensitive directory");
        }
        // Git hooks / config execute code on the next git command.
        if c == ".git"
            && comps
                .get(i + 1)
                .is_some_and(|n| n == "hooks" || (i + 2 == comps.len() && n == "config"))
        {
            return Some("git hooks/config");
        }
        // Windows per-user Startup folder (`...\\Start Menu\\Programs\\Startup`).
        if c == "programs" && comps.get(i + 1).is_some_and(|n| n == "startup") {
            return Some("autostart directory");
        }
        // systemd user units (~/.config/systemd/user/*.service).
        if c == ".config" && comps.get(i + 1).is_some_and(|n| n == "systemd") {
            return Some("systemd unit directory");
        }
    }

    // rsclaw's own profile dir: top-level config files and .env.
    let base = rsclaw_config::loader::base_dir();
    let base_canon = std::fs::canonicalize(&base).unwrap_or(base);
    if let Some(parent) = full.parent() {
        let parent_canon = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
        if parent_canon == base_canon
            && (filename.ends_with(".json5") || filename.ends_with(".json"))
        {
            return Some("rsclaw config file");
        }
    }
    None
}

/// Check read safety: block access to sensitive files and directories.
pub(crate) fn check_read_safety(path: &str, full: &std::path::Path) -> anyhow::Result<()> {
    let path_str = full.to_string_lossy().to_lowercase().replace('\\', "/");
    let path_lower = path.to_lowercase().replace('\\', "/");

    // Sensitive directories
    const SENSITIVE_DIRS: &[&str] = &[
        ".ssh/",
        ".gnupg/",
        ".gpg/",
        ".aws/",
        ".azure/",
        ".gcloud/",
        ".config/gcloud/",
        ".config/gh/",
        ".kube/",
        ".docker/",
        ".claude/",
        ".opencode/",
        ".openclaw/credentials/",
        ".password-store/",
        ".local/share/keyrings/",
        "library/keychains/",
        "library/cookies/",
    ];
    let path_str_dir = format!("{path_str}/");
    let path_lower_dir = format!("{path_lower}/");
    for dir in SENSITIVE_DIRS {
        if path_lower_dir.contains(dir) || path_str_dir.contains(dir) {
            anyhow::bail!("[blocked] access to sensitive directory: {path}");
        }
    }
    // rsclaw credentials live under whatever profile dir the user runs
    // (`~/.rsclaw/`, `~/.rsclaw-dev/`, `~/.rsclaw-<profile>/`, or
    // `RSCLAW_BASE_DIR`). Substring-matching `.rsclaw/credentials/` would
    // miss the suffixed variants, so use the live base_dir to detect.
    let base = rsclaw_config::loader::base_dir();
    let base_canon = std::fs::canonicalize(&base).unwrap_or_else(|_| base.clone());
    let canon = std::fs::canonicalize(full).unwrap_or_else(|_| full.to_path_buf());
    let creds = base.join("credentials");
    let canon_creds = std::fs::canonicalize(&creds).unwrap_or_else(|_| creds.clone());
    if canon.starts_with(&canon_creds) || canon.starts_with(&creds) || full.starts_with(&creds) {
        anyhow::bail!("[blocked] access to sensitive directory: {path}");
    }

    // Sensitive filenames (private keys, credentials, tokens, etc.)
    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // rsclaw profile dir: every top-level config file (`rsclaw.json5`,
    // backups, per-profile variants) carries provider/channel secrets.
    if let Some(parent) = canon.parent()
        && (parent == base_canon.as_path() || parent == base.as_path())
        && (filename.ends_with(".json5") || filename.ends_with(".json") || filename == ".env")
    {
        anyhow::bail!("[blocked] access to rsclaw config file: {path}");
    }

    const SENSITIVE_FILES: &[&str] = &[
        // SSH keys
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        "id_rsa.pub",
        "id_ed25519.pub",
        "authorized_keys",
        "known_hosts",
        // GPG
        "secring.gpg",
        "trustdb.gpg",
        // Cloud credentials
        "credentials",
        "credentials.json",
        "credentials.yaml",
        "service_account.json",
        "application_default_credentials.json",
        // Env / secrets
        ".env",
        ".env.local",
        ".env.production",
        ".env.secret",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".git-credentials",
        // Shell config (may contain tokens/aliases)
        ".bash_history",
        ".zsh_history",
        // Database
        ".pgpass",
        ".my.cnf",
        ".mongoshrc.js",
        // Docker / Kube
        "config.json", // docker config with auth
        // Crypto wallets
        "wallet.dat",
        "keystore",
        // Keychains / browser credential stores
        "login.keychain",
        "login.keychain-db",
        "login data",
        "cookies",
        "key4.db",
        "logins.json",
        // AI tool config files (contain API keys)
        "openclaw.json",
        "rsclaw.json5",
        "auth-profiles.json",
    ];

    for sensitive in SENSITIVE_FILES {
        if filename == *sensitive {
            anyhow::bail!("[blocked] access to sensitive file: {path}");
        }
    }
    if filename.starts_with(".env.") || filename.ends_with(".keychain") || filename.ends_with(".keychain-db")
    {
        anyhow::bail!("[blocked] access to sensitive file: {path}");
    }

    // Private key content pattern in filename
    if filename.contains("private") && (filename.contains("key") || filename.ends_with(".pem")) {
        anyhow::bail!("[blocked] access to private key file: {path}");
    }

    // Block reading system auth files via absolute path
    const SYSTEM_FILES: &[&str] = &[
        "/etc/shadow",
        "/etc/gshadow",
        "/etc/master.passwd",
        "/etc/sudoers",
    ];
    for sys in SYSTEM_FILES {
        if path_str.ends_with(sys) || path == *sys {
            anyhow::bail!("[blocked] access to system file: {path}");
        }
    }

    Ok(())
}

/// Scan a file's content against exec deny rules.
/// Used when an interpreter (bash, python, etc.) executes a file.
pub(crate) fn check_file_content_safety(file_path: &std::path::Path) -> anyhow::Result<()> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Ok(()), // file doesn't exist or not readable, let exec handle it
    };
    // Safety must be on — `load()` returns an engine whose checks always Allow.
    let preparse = crate::preparse::PreParseEngine::load_with_safety(true);
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("--")
        {
            continue;
        }
        if let crate::preparse::SafetyCheck::Deny(reason) = preparse.check_exec_safety(trimmed) {
            anyhow::bail!(
                "[blocked] file {}:{} contains dangerous command: {reason}",
                file_path.display(),
                line_num + 1
            );
        }
    }
    Ok(())
}

/// `~/Downloads/rsclaw` — where generated media and plugin downloads land.
pub(crate) fn rsclaw_downloads_dir() -> PathBuf {
    dirs_next::download_dir()
        .unwrap_or_else(|| {
            dirs_next::home_dir()
                .unwrap_or_else(rsclaw_config::loader::base_dir)
                .join("Downloads")
        })
        .join("rsclaw")
}

/// Where a tool may read local files from during one turn.
///
/// - Owners: any path, minus secret files/dirs ([`check_read_safety`]) when
///   `tools.exec.safety` is on (default).
/// - Non-owners: confined to the agent workspace (incl. `uploads/`), the
///   rsclaw downloads dir (generated media), installed skills and site
///   rules; secret names are refused even inside those roots.
#[derive(Debug, Clone)]
pub(crate) struct ReadScope {
    /// Sender trust of the turn.
    pub(crate) trust: SenderTrust,
    /// `tools.exec.safety` (owners only; non-owners are always checked).
    pub(crate) safety: bool,
    /// Agent workspace (relative paths resolve against it).
    pub(crate) workspace: PathBuf,
}

impl ReadScope {
    /// Roots a non-owner may read from.
    fn non_owner_roots(&self) -> Vec<PathBuf> {
        let base = rsclaw_config::loader::base_dir();
        vec![
            self.workspace.clone(),
            rsclaw_downloads_dir(),
            base.join("skills"),
            base.join("tools").join("web_browser").join("site-rules"),
        ]
    }

    /// Resolve a model-supplied local path for reading, enforcing the scope.
    pub(crate) fn resolve(&self, path: &str) -> Result<PathBuf> {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            bail!("empty path");
        }
        if self.trust.is_owner() {
            let full = rsclaw_util::canonicalize_external_path(trimmed, &self.workspace);
            if self.safety {
                check_read_safety(trimmed, &full)?;
                // A symlink inside the workspace may point at a secret.
                if let Ok(real) = std::fs::canonicalize(&full)
                    && real != full
                {
                    check_read_safety(trimmed, &real)?;
                }
            }
            return Ok(full);
        }
        let expanded = rsclaw_util::expand_tilde(trimmed);
        let roots = self.non_owner_roots();
        let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
        let full = rsclaw_util::fs_guard::resolve_within_any(&root_refs, &expanded.to_string_lossy())
            .map_err(|_| {
                anyhow!(
                    "[blocked] `{trimmed}` is outside the agent workspace. Only files inside the workspace (including uploads/) can be read in this conversation."
                )
            })?;
        check_read_safety(trimmed, &full)?;
        Ok(full)
    }
}

/// Read a regular local file fully, refusing non-regular files (FIFOs,
/// devices such as `/dev/zero`) and anything larger than `max_bytes`.
pub(crate) async fn read_regular_file_capped(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    // Check BEFORE open: opening a FIFO blocks until a writer appears.
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if meta.len() > max_bytes {
        bail!(
            "{} is too large ({} bytes, limit {max_bytes})",
            path.display(),
            meta.len()
        );
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    let mut buf = Vec::with_capacity(meta.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .await
        .map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if buf.len() as u64 > max_bytes {
        bail!("{} grew past the {max_bytes}-byte limit while reading", path.display());
    }
    Ok(buf)
}

/// Env var names that may carry secrets and must not leak into commands the
/// agent runs. Upper-cased substring match.
const SECRET_ENV_MARKERS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API",
    "AUTH",
    "CREDENTIAL",
    "PRIVATE",
    "COOKIE",
    "SESSION_ID",
];

/// Names matching [`SECRET_ENV_MARKERS`] that are not secrets and that
/// common tooling needs.
const SECRET_ENV_EXEMPT: &[&str] = &["SSH_AUTH_SOCK", "XAUTHORITY", "GPG_AGENT_INFO"];

/// Env vars that hold connection strings with embedded credentials.
const SECRET_ENV_EXACT: &[&str] = &[
    "DATABASE_URL",
    "REDIS_URL",
    "MONGODB_URI",
    "MONGO_URL",
    "AMQP_URL",
];

/// Env var listing extra names (comma separated) that exec may pass through
/// even though they look like secrets.
pub(crate) const EXEC_PASS_ENV: &str = "RSCLAW_EXEC_PASS_ENV";

/// True when env var `name` looks secret-bearing.
pub(crate) fn is_secret_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if SECRET_ENV_EXEMPT.contains(&upper.as_str()) {
        return false;
    }
    SECRET_ENV_EXACT.contains(&upper.as_str())
        || SECRET_ENV_MARKERS.iter().any(|m| upper.contains(m))
}

/// Names of current process env vars to strip from an agent-run command.
/// `explicit` lists names the operator deliberately exposed (the config
/// `env` block); [`EXEC_PASS_ENV`] adds more.
pub(crate) fn secret_env_to_strip(explicit: &std::collections::HashSet<String>) -> Vec<String> {
    let pass: std::collections::HashSet<String> = std::env::var(EXEC_PASS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| is_secret_env_name(k) && !explicit.contains(k) && !pass.contains(k))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_write_confinement_blocks_escapes() {
        let dir = std::env::temp_dir().join(format!("rsclaw-sec-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let roots = vec![dir.clone()];
        assert!(resolve_write_path("notes/a.md", &roots).is_ok());
        assert!(resolve_write_path("../x.md", &roots).is_err());
        assert!(resolve_write_path("~/.zshenv", &roots).is_err());
        assert!(resolve_write_path("/etc/passwd", &roots).is_err());
        let inside = dir.join("b.txt");
        assert!(resolve_write_path(inside.to_str().expect("utf8"), &roots).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn security_sensitive_write_names() {
        let ws = Path::new("/tmp/ws");
        for p in [
            "a/.zshenv",
            ".bash_profile",
            ".ssh/authorized_keys",
            "x/.git/hooks/pre-commit",
            "Library/LaunchAgents/evil.plist",
            ".config/autostart/x.desktop",
            ".env.local",
        ] {
            assert!(
                sensitive_write_reason(&ws.join(p)).is_some(),
                "{p} should be sensitive"
            );
        }
        assert!(sensitive_write_reason(&ws.join("src/main.rs")).is_none());
        assert!(sensitive_write_reason(&ws.join("git/hooks.md")).is_none());
    }

    #[test]
    fn security_content_scan_uses_safety_rules() {
        let ws = Path::new("/tmp/ws/run.sh");
        assert!(check_write_safety("run.sh", ws, "rm -rf /").is_err());
        assert!(check_write_safety("run.sh", ws, "echo hello").is_ok());
        // Prose / source code is not scanned with shell rules.
        let md = Path::new("/tmp/ws/notes.md");
        assert!(check_write_safety("notes.md", md, "call shutdown() then reboot").is_ok());
    }

    #[test]
    fn security_secret_env_names() {
        assert!(is_secret_env_name("OPENAI_API_KEY"));
        assert!(is_secret_env_name("TELEGRAM_BOT_TOKEN"));
        assert!(is_secret_env_name("FEISHU_APP_SECRET"));
        assert!(is_secret_env_name("DATABASE_URL"));
        assert!(!is_secret_env_name("SSH_AUTH_SOCK"));
        assert!(!is_secret_env_name("PATH"));
        assert!(!is_secret_env_name("HOME"));
    }
}
