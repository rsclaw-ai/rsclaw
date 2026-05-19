//! Bootstrap `.env` load + reconcile shell-vs-file for vars referenced
//! in rsclaw.json5.
//!
//! Pipeline at config load time:
//!   1. Snapshot the *current* process env once (first call wins,
//!      cached for the rest of the process lifetime). This is the
//!      "shell-provided" view, captured before we mutate process env
//!      with `.env` values.
//!   2. Load `$BASE_DIR/.env` into process env additively — vars
//!      already set by the shell are NOT overwritten.
//!   3. Scan the raw rsclaw.json5 text for `${VAR}` placeholders and
//!      `{source:"env",id:"X"}` SecretRef nodes. Both forms reference
//!      env vars.
//!   4. Reconcile: for each referenced var, the shell snapshot value
//!      wins over the `.env` value on diff (rotation case — user
//!      updated their `~/.zshrc`). New vars (in shell, not in `.env`)
//!      are captured. `.env` is rewritten when anything changed.
//!
//! After this runs, process env is the single source of truth for
//! `expand_env_vars` and `SecretOrString::resolve_early`. The `.env`
//! file on disk is a forward-going cache that survives the next
//! service-managed launch (launchd / systemd) where there's no shell
//! env to inherit from.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use regex::Regex;

use super::env_file;

/// Process env captured BEFORE any `.env` load. The first call grabs
/// `std::env::vars()`; later calls return the cached snapshot.
static SHELL_SNAPSHOT: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Initialize (if needed) and return the shell-env snapshot. Idempotent
/// across hot-reloads — only the very first call captures live env.
pub fn shell_snapshot() -> &'static HashMap<String, String> {
    SHELL_SNAPSHOT.get_or_init(|| std::env::vars().collect())
}

/// Run the bootstrap + reconcile pipeline. Idempotent: safe to call on
/// every config (re)load. Best-effort — a write failure on `.env`
/// surfaces as a warn-level log but does not block gateway startup.
pub fn reconcile(raw_config: &str, base_dir: &Path) -> Result<()> {
    // Force snapshot capture if not already done.
    let shell = shell_snapshot();

    let env_path = base_dir.join(".env");
    let mut file = env_file::read(&env_path)?;

    // Additive load: vars only get set if not already in process env.
    // This preserves the "shell wins by default" invariant for the
    // common path (terminal launch). Reconcile below handles the
    // diff case explicitly.
    for (k, v) in &file {
        if std::env::var(k).is_err() {
            // SAFETY: config load runs single-threaded during process
            // startup, before any tokio worker is spawned. Re-loads on
            // hot-reload are also serialized through the loader.
            unsafe { std::env::set_var(k, v) };
        }
    }

    let needed = scan_var_refs(raw_config);

    let mut updated = Vec::new();
    let mut added = Vec::new();

    for var in &needed {
        match (shell.get(var), file.get(var)) {
            (Some(shell_val), Some(file_val)) if shell_val != file_val => {
                // Rotation case: user updated `~/.zshrc`, started from
                // terminal; `.env` is stale. Shell wins.
                unsafe { std::env::set_var(var, shell_val) };
                file.insert(var.clone(), shell_val.clone());
                updated.push(var.clone());
            }
            (Some(shell_val), None) => {
                // First sync: shell has it, `.env` doesn't. Capture
                // for next service-managed launch.
                file.insert(var.clone(), shell_val.clone());
                added.push(var.clone());
            }
            _ => {}
        }
    }

    let file_changed = !updated.is_empty() || !added.is_empty();
    if file_changed {
        env_file::write(&env_path, &file)?;
        if !added.is_empty() {
            tracing::info!(
                vars = ?added,
                path = %env_path.display(),
                ".env: added vars from shell"
            );
        }
        if !updated.is_empty() {
            tracing::info!(
                vars = ?updated,
                path = %env_path.display(),
                ".env: updated vars from shell (rotation)"
            );
        }
    }

    Ok(())
}

/// `${VAR}` placeholder regex — matches the same shape as
/// `loader::ENV_VAR_RE` so the two stay in lockstep.
static PLACEHOLDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex")
});

/// `{source:"env",id:"VAR"}` SecretRef regex. Operates on raw JSON5
/// text because we need the var list BEFORE the config is parsed
/// (chicken-and-egg: parsing fills in ${VAR} from process env, but
/// we need to populate process env first). Tolerant of whitespace and
/// either single or double quotes around values; not a full parser.
static SECRETREF_ENV_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r#""?source"?\s*:\s*"env"\s*,[^}]*?"?id"?\s*:\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#,
    )
    .expect("valid regex")
});

/// Scan raw config text for both `${VAR}` placeholders and `{source:
/// "env", id: "X"}` SecretRef nodes. Returns the union of referenced
/// var names.
pub fn scan_var_refs(raw: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for caps in PLACEHOLDER_RE.captures_iter(raw) {
        out.insert(caps[1].to_owned());
    }
    for caps in SECRETREF_ENV_RE.captures_iter(raw) {
        out.insert(caps[1].to_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_placeholders_collects_all_unique() {
        let raw = r#"{
            apiKey: "${RSCLAW_API_KEY}",
            other: "${FOO_BAR}",
            same: "${RSCLAW_API_KEY}",
        }"#;
        let got = scan_var_refs(raw);
        assert_eq!(got.len(), 2, "got {got:?}");
        assert!(got.contains("RSCLAW_API_KEY"));
        assert!(got.contains("FOO_BAR"));
    }

    #[test]
    fn scan_secretref_env_captures_id() {
        let raw = r#"{
            "apiKey": {"source": "env", "id": "MY_KEY"},
            "other": {"source": "file", "path": "/x"}
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("MY_KEY"), "got {got:?}");
        // file-source SecretRef must NOT match the env regex
        assert_eq!(got.len(), 1, "got {got:?}");
    }

    #[test]
    fn scan_handles_mixed_refs() {
        let raw = r#"{
            top: "${FROM_PLACEHOLDER}",
            nested: { apiKey: {source: "env", id: "FROM_REF"} }
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("FROM_PLACEHOLDER"));
        assert!(got.contains("FROM_REF"));
    }
}
