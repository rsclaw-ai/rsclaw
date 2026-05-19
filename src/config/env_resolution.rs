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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

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

    // Fallback: vars that are STILL unresolved (not in shell snapshot
    // and not in .env) — try sourcing the user's login shell rc to
    // pick them up. Common when launchd/systemd starts gateway with
    // an empty env and the user has only ever exported the var in
    // their ~/.zshrc (never written to .env). One-shot per process —
    // self-restart children skip via _RSCLAW_ENV_INHERITED marker.
    let still_missing: Vec<String> = needed
        .iter()
        .filter(|v| std::env::var_os(v.as_str()).is_none())
        .cloned()
        .collect();
    let mut recovered_from_rc: BTreeMap<String, String> = BTreeMap::new();
    if !still_missing.is_empty() {
        if let Some(found) = shell_rc_fallback(&still_missing) {
            for (k, v) in &found {
                // SAFETY: same single-threaded boot phase as above.
                unsafe { std::env::set_var(k, v) };
                file.insert(k.clone(), v.clone());
                recovered_from_rc.insert(k.clone(), v.clone());
            }
        }
    }

    let file_changed = !updated.is_empty() || !added.is_empty() || !recovered_from_rc.is_empty();
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
        if !recovered_from_rc.is_empty() {
            tracing::info!(
                vars = ?recovered_from_rc.keys().collect::<Vec<_>>(),
                path = %env_path.display(),
                ".env: recovered vars by sourcing shell rc files"
            );
        }
    }

    Ok(())
}

/// Spawn the user's login shell with `-lic 'env'` to source their
/// rc/profile files and capture the resulting env. Only used as a
/// last-resort fallback when a config-referenced var is missing from
/// both the shell snapshot and `.env`. Filters output to just the
/// `wanted` list — we don't want to leak unrelated shell-only vars
/// (PROMPT_COMMAND, PS1, HISTFILE, …) into .env.
///
/// Returns `None` when:
///   - on Windows (no POSIX rc-file convention here)
///   - `_RSCLAW_ENV_INHERITED=1` is already set (we're a self-restart
///     child of a process that already ran this)
///   - `RSCLAW_NO_SHELL_SOURCE=1` is set (operator opt-out)
///   - we couldn't determine the login shell or it doesn't exist
///   - the shell timed out, returned non-zero, or wrote no env output
fn shell_rc_fallback(wanted: &[String]) -> Option<BTreeMap<String, String>> {
    if cfg!(windows) {
        return None;
    }
    if std::env::var_os("_RSCLAW_ENV_INHERITED").is_some() {
        return None;
    }
    if std::env::var_os("RSCLAW_NO_SHELL_SOURCE").is_some() {
        return None;
    }

    let shell = resolve_login_shell()?;
    tracing::debug!(
        shell = %shell,
        vars = ?wanted,
        "env reconcile: sourcing shell rc to recover missing vars"
    );

    // `$SHELL -lic 'env'` — login + interactive flags trigger the full
    // rc/profile chain on bash/zsh; fish honours `-l -c` similarly and
    // ships `env` on PATH. The `-i` flag is the load-bearing one for
    // bash/zsh: `.bashrc` / `.zshrc` are sourced only for interactive
    // shells.
    let mut cmd = Command::new(&shell);
    cmd.args(["-lic", "env"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let output = match run_with_timeout(cmd, Duration::from_secs(5)) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                shell = %shell,
                error = %e,
                "env reconcile: shell rc source failed"
            );
            // Set the marker anyway so a flaky shell doesn't cause us
            // to retry every config load.
            unsafe { std::env::set_var("_RSCLAW_ENV_INHERITED", "1") };
            return None;
        }
    };
    if !output.status.success() {
        tracing::warn!(
            shell = %shell,
            exit = ?output.status.code(),
            "env reconcile: shell rc source exited non-zero"
        );
        unsafe { std::env::set_var("_RSCLAW_ENV_INHERITED", "1") };
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let want: std::collections::HashSet<&str> = wanted.iter().map(String::as_str).collect();
    let mut found = BTreeMap::new();
    for line in stdout.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if want.contains(k) {
            found.insert(k.to_owned(), v.to_owned());
        }
    }

    // Always mark — even an empty result means "we tried and
    // shouldn't retry on hot-reload".
    unsafe { std::env::set_var("_RSCLAW_ENV_INHERITED", "1") };

    if found.is_empty() { None } else { Some(found) }
}

/// Find the user's login shell. `$SHELL` is the fast path; falls back
/// to `getent passwd` (Linux) or `dscl . -read /Users/<user>` (macOS)
/// for the launchd / systemd case where the supervisor doesn't pass
/// `$SHELL` through.
fn resolve_login_shell() -> Option<String> {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.is_empty() && Path::new(&s).exists() {
            return Some(s);
        }
    }
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok()?;

    #[cfg(target_os = "macos")]
    {
        let out = Command::new("dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            // dscl prints "UserShell: /bin/zsh"
            if let Some((_, v)) = s.split_once(':') {
                let shell = v.trim().to_owned();
                if Path::new(&shell).exists() {
                    return Some(shell);
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let out = Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()?;
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            // getent prints "user:x:uid:gid:gecos:/home/user:/bin/bash"
            let shell = s.trim().rsplit(':').next()?.to_owned();
            if Path::new(&shell).exists() {
                return Some(shell);
            }
        }
    }

    None
}

/// Run a child to completion or kill it on timeout. Designed for the
/// shell-rc fallback path where a hung shell would otherwise wedge
/// gateway startup forever — and the worker thread waiting on it.
fn run_with_timeout(mut cmd: Command, dur: Duration) -> Result<std::process::Output> {
    let child = cmd.spawn()?;
    let pid = child.id();

    // Move the child into a worker thread that waits for it. The
    // worker sends the output back through a channel. Meanwhile the
    // caller waits on the channel with a timeout — if the timeout
    // fires before the worker sends, we SIGTERM the child by PID,
    // then SIGKILL after a short grace if it's still alive. Without
    // the SIGKILL escalation a `.zshrc` wedged on a network call
    // would leak the worker thread for the rest of the process.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(dur) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            #[cfg(unix)]
            unsafe {
                // SAFETY: libc::kill on a PID we just spawned; sending
                // SIGTERM is benign even if the child has already exited
                // (kill returns ESRCH, which we ignore).
                libc::kill(pid as i32, libc::SIGTERM);
                // Grace window for graceful shutdown — then escalate to
                // SIGKILL so the worker thread can unblock from
                // wait_with_output() and not leak for the rest of the
                // process lifetime.
                const SIGKILL_GRACE: Duration = Duration::from_millis(500);
                if rx.recv_timeout(SIGKILL_GRACE).is_err() {
                    libc::kill(pid as i32, libc::SIGKILL);
                    // Final short window to reap; if even SIGKILL
                    // doesn't free the worker, the thread is leaked
                    // but at most one process is involved per boot.
                    let _ = rx.recv_timeout(Duration::from_millis(500));
                }
            }
            anyhow::bail!("shell command timed out after {dur:?}")
        }
    }
}

/// `${VAR}` placeholder regex — matches the same shape as
/// `loader::ENV_VAR_RE` so the two stay in lockstep.
static PLACEHOLDER_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("valid regex")
});

/// `{source:"env",id:"VAR"}` SecretRef regex — used only as a fallback
/// when JSON5 parsing fails. The primary scan goes via parsed Value
/// tree to avoid matching SecretRef-looking syntax inside string
/// literals (e.g. a tool description that mentions `{source:'env',
/// id:'PATH'}` would otherwise persist `PATH` into `.env`).
static SECRETREF_ENV_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(
        r#""?source"?\s*:\s*"env"\s*,[^}]*?"?id"?\s*:\s*["']([A-Za-z_][A-Za-z0-9_]*)["']"#,
    )
    .expect("valid regex")
});

/// Sentinel-wrapped placeholder used to make the raw config parseable as
/// JSON5 before any `${VAR}` substitution has happened. We replace
/// `${VAR}` → `"__RSCLAW_PH_VAR__"` so the parser never chokes on a bare
/// placeholder (e.g. `port: ${PORT}`), then strip the sentinel back
/// when walking the resulting Value tree.
const PLACEHOLDER_SENTINEL_PREFIX: &str = "__RSCLAW_PH_";
const PLACEHOLDER_SENTINEL_SUFFIX: &str = "__";

/// Strip JSON5 line/block comments without otherwise touching the text.
/// Used only by the fallback regex scan; the primary path parses JSON5
/// directly which already ignores comments.
fn strip_json5_comments(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    let mut in_string: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_string {
            // Inside a string: pass through, watch for the matching close
            // quote, treat `\` as a 2-char escape so `\"` doesn't end the
            // string. JSON5 does not allow unescaped newlines inside
            // single-line strings; we don't try to be cleverer than that.
            out.push(c as char);
            if c == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'/' => {
                    // Line comment — skip to next \n (keep the \n).
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                b'*' => {
                    // Block comment — skip to closing `*/`.
                    i += 2;
                    while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(bytes.len());
                    continue;
                }
                _ => {}
            }
        }
        if c == b'"' || c == b'\'' {
            in_string = Some(c);
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Walk a parsed JSON5 Value tree and collect:
/// - `id` strings from `{source: "env", id: "X"}` SecretRef objects;
/// - `${VAR}` placeholders embedded in any string value.
fn walk_value_for_refs(v: &serde_json::Value, out: &mut BTreeSet<String>) {
    use serde_json::Value;
    match v {
        Value::Object(m) => {
            let is_env_ref = matches!(m.get("source"), Some(Value::String(s)) if s == "env");
            if is_env_ref {
                if let Some(Value::String(raw_id)) = m.get("id") {
                    // `id` may itself be a `${VAR}` placeholder that the
                    // sentinel pass turned into `__RSCLAW_PH_VAR__`. In
                    // that case the placeholder's name is captured
                    // separately via PLACEHOLDER_RE on the raw text —
                    // don't double-add it here.
                    let id = raw_id.as_str();
                    let is_sentinel = id.starts_with(PLACEHOLDER_SENTINEL_PREFIX)
                        && id.ends_with(PLACEHOLDER_SENTINEL_SUFFIX);
                    if !is_sentinel
                        && !id.is_empty()
                        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        && id.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                    {
                        out.insert(id.to_owned());
                    }
                }
            }
            for v in m.values() {
                walk_value_for_refs(v, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk_value_for_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Scan raw config text for both `${VAR}` placeholders and `{source:
/// "env", id: "X"}` SecretRef nodes. Returns the union of referenced
/// var names.
///
/// Primary strategy: substitute `${VAR}` placeholders with a sentinel
/// string so the raw text always parses as JSON5, then walk the Value
/// tree. This anchors SecretRef detection to actual JSON5 structure so
/// matching syntax buried inside string literals (e.g. a tool
/// description) does not get hoisted into `.env`.
///
/// Fallback: if JSON5 parsing still fails, fall back to a
/// comment-stripped raw-regex scan. Better to over-set a couple of env
/// vars than to silently miss a real reference.
pub fn scan_var_refs(raw: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // `${VAR}` placeholders are always collected from the raw text;
    // they live inside strings and are unambiguous.
    for caps in PLACEHOLDER_RE.captures_iter(raw) {
        out.insert(caps[1].to_owned());
    }
    // For SecretRef detection: substitute placeholders → sentinel
    // string, parse JSON5, walk the tree.
    let sentinel = PLACEHOLDER_RE.replace_all(raw, |caps: &regex::Captures<'_>| {
        format!(
            "\"{prefix}{name}{suffix}\"",
            prefix = PLACEHOLDER_SENTINEL_PREFIX,
            name = &caps[1],
            suffix = PLACEHOLDER_SENTINEL_SUFFIX,
        )
    });
    match json5::from_str::<serde_json::Value>(&sentinel) {
        Ok(v) => walk_value_for_refs(&v, &mut out),
        Err(_) => {
            // Fallback: comment-stripped regex scan. Less precise but
            // preserves the old behaviour for configs that won't parse
            // (rare; usually means a hand-edited syntax error).
            let stripped = strip_json5_comments(raw);
            for caps in SECRETREF_ENV_RE.captures_iter(&stripped) {
                out.insert(caps[1].to_owned());
            }
        }
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

    #[test]
    fn secretref_inside_string_literal_ignored() {
        // A tool description that mentions SecretRef syntax inside a
        // STRING VALUE must NOT cause `PATH` to be persisted to `.env`.
        let raw = r#"{
            tools: [{
                name: "explain_secret_ref",
                description: "Pass {source:'env', id:'PATH'} to fetch process PATH at runtime."
            }],
            apiKey: {source: "env", id: "REAL_KEY"}
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("REAL_KEY"), "real ref missing: {got:?}");
        assert!(!got.contains("PATH"), "false positive captured: {got:?}");
    }

    #[test]
    fn secretref_inside_line_comment_ignored() {
        let raw = r#"{
            // example: apiKey: {source: "env", id: "EXAMPLE_KEY"}
            apiKey: {source: "env", id: "REAL_KEY"}
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("REAL_KEY"));
        assert!(!got.contains("EXAMPLE_KEY"), "comment captured: {got:?}");
    }

    #[test]
    fn secretref_inside_block_comment_ignored() {
        let raw = r#"{
            /* legacy: {source: "env", id: "OLD_KEY"} */
            apiKey: {source: "env", id: "REAL_KEY"}
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("REAL_KEY"));
        assert!(!got.contains("OLD_KEY"), "block comment captured: {got:?}");
    }

    #[test]
    fn placeholder_outside_string_still_captured() {
        // `port: ${PORT}` (placeholder appears bare; not valid JSON5).
        // Sentinel pass makes it parseable; PLACEHOLDER_RE still finds
        // PORT on the raw text.
        let raw = r#"{
            port: ${PORT},
            apiKey: "${MY_KEY}"
        }"#;
        let got = scan_var_refs(raw);
        assert!(got.contains("PORT"), "got {got:?}");
        assert!(got.contains("MY_KEY"), "got {got:?}");
    }
}
