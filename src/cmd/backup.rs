use anyhow::Result;

use super::style::*;
use crate::cli::BackupCommand;

pub async fn cmd_backup(sub: BackupCommand) -> Result<()> {
    match sub {
        BackupCommand::Create(args) => cmd_backup_create(args).await,
        BackupCommand::Verify { file } => cmd_backup_verify(&file).await,
    }
}

// ---------------------------------------------------------------------------
// backup create / verify (AGENTS.md S28)
// ---------------------------------------------------------------------------

async fn cmd_backup_create(args: crate::cli::BackupCreateArgs) -> Result<()> {
    use flate2::{Compression, write::GzEncoder};
    use sha2::{Digest, Sha256};

    banner(&format!(
        "rsclaw backup create v{}",
        env!("RSCLAW_BUILD_VERSION")
    ));

    let base = crate::config::loader::base_dir();

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

/// Replace plaintext secret values in config text with `***`.
fn redact_config(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let lower = line.to_lowercase();
        let is_secret = lower.contains("api_key")
            || lower.contains("token")
            || lower.contains("secret")
            || lower.contains("password");
        if is_secret && line.contains('=') && !line.contains("${") {
            let (key, _) = line.splitn(2, '=').collect::<Vec<_>>().split_at(1).0[0]
                .split_at(line.find('=').unwrap_or(line.len()));
            let _ = key; // suppress lint
            if let Some(eq) = line.find('=') {
                out.push_str(&line[..=eq]);
                out.push_str(" \"***\"\n");
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

async fn cmd_backup_verify(file: &str) -> Result<()> {
    use sha2::{Digest, Sha256};

    banner(&format!(
        "rsclaw backup verify v{}",
        env!("RSCLAW_BUILD_VERSION")
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
