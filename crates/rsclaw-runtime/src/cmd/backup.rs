use anyhow::Result;
use rsclaw_cli::BackupCommand;

use super::style::*;

pub async fn cmd_backup(sub: BackupCommand) -> Result<()> {
    match sub {
        BackupCommand::Create(args) => cmd_backup_create(args).await,
        BackupCommand::Verify { file } => cmd_backup_verify(&file).await,
    }
}

// ---------------------------------------------------------------------------
// backup create / verify (AGENTS.md S28)
// ---------------------------------------------------------------------------

async fn cmd_backup_create(args: rsclaw_cli::BackupCreateArgs) -> Result<()> {
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};

    banner(&format!(
        "rsclaw backup create v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));

    let base = rsclaw_config::loader::base_dir();

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let archive_name = format!("rsclaw-backup-{timestamp}.tar.gz");
    let archive_path = std::env::current_dir()?.join(&archive_name);
    let checksum_path = archive_path.with_extension("").with_extension("sha256");

    let file = std::fs::File::create(&archive_path)?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = tar::Builder::new(gz);

    // Include workspace.
    let workspace = base.join("workspace");
    if workspace.exists() {
        tar.append_dir_all("workspace", &workspace)?;
    }

    // Include config (redacted: replace secret values with placeholders).
    let config_path = base.join("rsclaw.json5");
    if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path)?;
        let redacted = redact_config(&raw);
        let mut header = tar::Header::new_gnu();
        header.set_size(redacted.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        tar.append_data(&mut header, "rsclaw.json5", redacted.as_bytes())?;
    }

    // Optionally include session transcripts.
    if args.include_sessions {
        let transcripts = base.join("transcripts");
        if transcripts.exists() {
            tar.append_dir_all("transcripts", &transcripts)?;
        }
    }

    let gz = tar.into_inner()?;
    gz.finish()?;

    // Compute SHA-256 checksum.
    let bytes = std::fs::read(&archive_path)?;
    let size_kb = bytes.len() / 1024;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let checksum = format!("{:x}  {}\n", hasher.finalize(), archive_name);
    std::fs::write(&checksum_path, &checksum)?;

    ok(&format!(
        "created {}",
        bold(&archive_path.display().to_string())
    ));
    kv("size", &format!("{} KB", size_kb));
    kv("sha256", &dim(&checksum_path.display().to_string()));
    Ok(())
}

/// True for config keys whose string values are credentials.
fn is_secret_key(key: &str) -> bool {
    let k: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if k.contains("tokenizer") {
        return false;
    }
    const MARKERS: &[&str] = &[
        "apikey",
        "token",
        "secret",
        "password",
        "passwd",
        "credential",
        "privatekey",
        "accesskey",
        "aeskey",
        "signingkey",
        "cookie",
        "authorization",
    ];
    MARKERS.iter().any(|m| k.contains(m))
}

/// Recursively replace string values under secret-like keys with `***`.
/// `${VAR}` references are kept (they're not secrets themselves).
fn redact_value(v: &mut serde_json::Value, secret_parent: bool) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                redact_value(child, secret_parent || is_secret_key(k));
            }
        }
        serde_json::Value::Array(items) => {
            for child in items.iter_mut() {
                redact_value(child, secret_parent);
            }
        }
        serde_json::Value::String(s) if secret_parent && !s.contains("${") => {
            *s = "***".to_owned();
        }
        _ => {}
    }
}

/// Replace plaintext secret values in config text with `***`.
///
/// The config is parsed as JSON5 and every string under a secret-like key
/// (`apiKey`, `token`, `botToken`, `appSecret`, `password`, ...) is redacted
/// recursively; the result is written back as JSON (comments are lost). If
/// the file doesn't parse, a line-oriented regex fallback redacts quoted
/// values of `key: "value"` / `key = "value"` pairs instead.
fn redact_config(raw: &str) -> String {
    if let Ok(mut val) = json5::from_str::<serde_json::Value>(raw) {
        redact_value(&mut val, false);
        if let Ok(mut out) = serde_json::to_string_pretty(&val) {
            out.push('\n');
            return out;
        }
    }
    redact_config_fallback(raw)
}

/// Regex fallback for [`redact_config`] when the config doesn't parse.
fn redact_config_fallback(raw: &str) -> String {
    static RE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(["']?[\w\-]*(?:api_?key|token|secret|password|passwd|credential|private_?key|access_?key|aes_?key|cookie)[\w\-]*["']?\s*[:=]\s*)("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,}\]]+)"#,
        )
        .ok()
    });
    let Some(re) = RE.as_ref() else {
        // Can't redact safely: drop the content rather than leak it.
        return String::from("// config omitted: could not be parsed for redaction\n");
    };
    re.replace_all(raw, |caps: &regex::Captures<'_>| {
        let value = &caps[2];
        if value.contains("${") {
            caps[0].to_owned()
        } else {
            format!("{}\"***\"", &caps[1])
        }
    })
    .into_owned()
}

async fn cmd_backup_verify(file: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    banner(&format!(
        "rsclaw backup verify v{}",
        option_env!("RSCLAW_BUILD_VERSION").unwrap_or("dev")
    ));

    let archive_path = std::path::Path::new(file);
    if !archive_path.exists() {
        anyhow::bail!("file not found: {file}");
    }

    // Look for a paired .sha256 file.
    let checksum_path = archive_path.with_extension("").with_extension("sha256");
    if !checksum_path.exists() {
        anyhow::bail!("checksum file not found: {}", checksum_path.display());
    }

    let expected_line = std::fs::read_to_string(&checksum_path)?;
    let expected_hash = expected_line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_owned();

    let bytes = std::fs::read(archive_path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash == expected_hash {
        ok(&format!("checksum valid: {}", bold(file)));
    } else {
        err_msg(&format!("checksum mismatch: {}", bold(file)));
        kv("expected", &dim(&expected_hash));
        kv("actual", &red(&actual_hash));
        anyhow::bail!("checksum mismatch");
    }
    Ok(())
}

#[cfg(test)]
mod redact_tests {
    use super::*;

    #[test]
    fn backup_redacts_json5_secret_keys() {
        let raw = r#"{
  // comment
  models: { providers: { openai: { apiKey: "sk-live", baseUrl: "https://x" } } },
  channels: { telegram: { botToken: "123:abc" }, feishu: { appSecret: 'sec', appId: "cli_1" } },
  gateway: { auth_token: "tok", port: 18888, maxTokens: 4096 },
  hooks: { token: "${HOOKS_TOKEN}" },
  db: { password: "pw" },
}"#;
        let out = redact_config(raw);
        for leaked in ["sk-live", "123:abc", "\"sec\"", "\"tok\"", "\"pw\""] {
            assert!(!out.contains(leaked), "leaked {leaked}: {out}");
        }
        assert!(out.contains("cli_1"));
        assert!(out.contains("https://x"));
        assert!(out.contains("${HOOKS_TOKEN}"));
        assert!(out.contains("4096"));
    }

    #[test]
    fn backup_redact_fallback_handles_unparseable() {
        let raw = "apiKey: \"sk-live\",\nname: \"bob\"\n{{ broken";
        let out = redact_config(raw);
        assert!(!out.contains("sk-live"));
        assert!(out.contains("bob"));
    }
}
