//! File safety checks — block dangerous reads, writes, and file content.
//!
//! Extracted from `runtime.rs` to reduce file size.

/// Check write safety:
/// 1. Block absolute paths (must stay within workspace)
/// 2. Block path traversal (../)
/// 3. Block sensitive filenames
/// 4. Scan ALL file content for dangerous commands (not just scripts)
pub(crate) fn check_write_safety(path: &str, full: &std::path::Path, content: &str) -> anyhow::Result<()> {
    // 1. Block absolute paths — write must be relative to workspace
    if path.starts_with('/') || path.starts_with('\\') || path.contains(":\\") {
        anyhow::bail!(
            "[blocked] absolute path not allowed: {path}. Use relative paths within workspace."
        );
    }

    // 2. Block path traversal
    if path.contains("../") || path.contains("..\\") {
        anyhow::bail!("[blocked] path traversal not allowed: {path}");
    }

    // 3. Block sensitive filenames (even within workspace)
    let path_lower = path.to_lowercase();
    const SENSITIVE_NAMES: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".zshrc",
        ".profile",
        ".login",
        "authorized_keys",
        "known_hosts",
        "id_rsa",
        "id_ed25519",
        "crontab",
        ".env",
        "openclaw.json",
        "rsclaw.json5",
        "auth-profiles.json",
    ];
    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    for sensitive in SENSITIVE_NAMES {
        if filename == *sensitive || path_lower.ends_with(sensitive) {
            anyhow::bail!("[blocked] write to sensitive file: {path}");
        }
    }

    // 4. Scan ALL file content for dangerous commands
    if !content.is_empty() {
        let preparse = crate::agent::preparse::PreParseEngine::load();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("//")
                || trimmed.starts_with("--")
            {
                continue;
            }
            match preparse.check_exec_safety(trimmed) {
                crate::agent::preparse::SafetyCheck::Deny(reason) => {
                    anyhow::bail!("[blocked] file contains dangerous command: {reason}");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Check read safety: block access to sensitive files and directories.
pub(crate) fn check_read_safety(path: &str, full: &std::path::Path) -> anyhow::Result<()> {
    let path_str = full.to_string_lossy().to_lowercase();
    let path_lower = path.to_lowercase();

    // Sensitive directories
    const SENSITIVE_DIRS: &[&str] = &[
        ".ssh/",
        ".gnupg/",
        ".gpg/",
        ".aws/",
        ".azure/",
        ".gcloud/",
        ".config/gcloud/",
        ".kube/",
        ".docker/",
        ".claude/",
        ".opencode/",
        ".openclaw/credentials/",
        ".rsclaw/credentials/",
    ];
    for dir in SENSITIVE_DIRS {
        if path_lower.contains(dir) || path_str.contains(dir) {
            anyhow::bail!("[blocked] access to sensitive directory: {path}");
        }
    }

    // Sensitive filenames (private keys, credentials, tokens, etc.)
    let filename = full
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default();

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
    let preparse = crate::agent::preparse::PreParseEngine::load();
    for (line_num, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("--")
        {
            continue;
        }
        match preparse.check_exec_safety(trimmed) {
            crate::agent::preparse::SafetyCheck::Deny(reason) => {
                anyhow::bail!(
                    "[blocked] file {}:{} contains dangerous command: {reason}",
                    file_path.display(),
                    line_num + 1
                );
            }
            _ => {}
        }
    }
    Ok(())
}
