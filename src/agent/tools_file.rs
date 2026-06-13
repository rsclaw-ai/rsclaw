//! File/exec-related tool methods extracted from `runtime.rs`.
//!
//! Contains: `tool_list_dir`, `tool_search_file`,
//! `tool_search_content`, `tool_read`, `tool_write`, `tool_exec`.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    runtime::{canonicalize_external_path, expand_tilde, resolve_default_workspace},
    security::{check_file_content_safety, check_read_safety, check_write_safety},
};

/// Fix common LLM shell-writing mistakes around redirects that bash would
/// otherwise silently mis-parse. The target is constructs like
/// `foo.png2>&1` (bash reads as `foo.png2` + `>&1`), which eats the last
/// character of a preceding token.
///
/// Rules applied:
/// 1. Insert a space before `2>&1` / `&>` / `>&N` redirects when they are glued
///    to a previous non-space, non-redirect character.
/// 2. Same for plain `>` / `>>` / `<` when glued (but only when the prior token
///    ends with a digit or letter — don't break `1>&2` etc).
/// 3. Leave the rest of the command untouched.
/// Surface CLI auto-correction hints from stderr at the top of the tool
/// result. LLMs routinely miss tip lines buried in a wall of error text;
/// promoting them to a dedicated `hint` field makes the next-turn fix
/// obvious without any auto-retry magic that could execute the wrong
/// thing on a typo'd subcommand.
///
/// Recognised formats (one per line, case-insensitive on the trigger):
/// - clap (Rust):      `tip: some similar subcommands exist: 'a', 'b'` `tip: a
///   similar argument exists: '--foo'`
/// - cobra (Go):       `Did you mean this?\n  xyz`
/// - commander (Node): `(Did you mean --xyz?)`
/// - click (Python):   `Did you mean "xyz"?` / `Did you mean 'xyz'?`
/// - generic:          any line containing `did you mean` (lowered)
fn extract_cli_hint(stderr: &str) -> Option<String> {
    let mut tip_line: Option<String> = None;
    let mut after_did_you_mean_this: Option<String> = None;
    let mut prev_was_did_you_mean_this = false;

    for raw in stderr.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lower = line.to_lowercase();

        // clap: "tip: ..."
        if lower.starts_with("tip:") {
            tip_line = Some(line.trim_start_matches("tip:").trim().to_owned());
        }
        // cobra: "Did you mean this?" then candidates on the next non-empty line
        if prev_was_did_you_mean_this {
            after_did_you_mean_this = Some(line.to_owned());
            prev_was_did_you_mean_this = false;
        }
        if lower.contains("did you mean this?") {
            prev_was_did_you_mean_this = true;
            continue;
        }
        // commander / click / generic "did you mean"
        if lower.contains("did you mean") && tip_line.is_none() {
            tip_line = Some(line.to_owned());
        }
    }

    match (tip_line, after_did_you_mean_this) {
        (Some(t), Some(c)) => Some(format!("{t} ({c})")),
        (Some(t), None) => Some(t),
        (None, Some(c)) => Some(format!("Did you mean: {c}")),
        (None, None) => None,
    }
}

#[cfg(test)]
mod hint_tests {
    use super::extract_cli_hint;

    #[test]
    fn clap_subcommand_tip() {
        let s = "error: unrecognized subcommand 'star'\n\n  tip: some similar subcommands exist: 'status', 'restart', 'start'";
        assert_eq!(
            extract_cli_hint(s).as_deref(),
            Some("some similar subcommands exist: 'status', 'restart', 'start'")
        );
    }

    #[test]
    fn clap_arg_tip() {
        let s = "error: unknown option '--depDate'\n  tip: a similar argument exists: '--dep-date'";
        assert!(extract_cli_hint(s).unwrap().contains("--dep-date"));
    }

    #[test]
    fn commander_did_you_mean() {
        let s = "error: unknown option '--depDate'\n(Did you mean --dep-date?)";
        assert!(
            extract_cli_hint(s)
                .unwrap()
                .to_lowercase()
                .contains("dep-date")
        );
    }

    #[test]
    fn cobra_did_you_mean_this() {
        let s = "Error: unknown command \"star\" for \"rsclaw\"\n\nDid you mean this?\n\tstart\n\nRun 'rsclaw --help' for usage.";
        let hint = extract_cli_hint(s).unwrap();
        assert!(hint.contains("start"), "got: {hint}");
    }

    #[test]
    fn no_hint_clean_error() {
        let s = "error: file not found";
        assert_eq!(extract_cli_hint(s), None);
    }

    #[test]
    fn no_hint_for_unrelated_text_with_did_you_mean_substring() {
        // This shouldn't match — checks we don't false-positive on unrelated content.
        let s = "Successfully wrote 5 lines";
        assert_eq!(extract_cli_hint(s), None);
    }
}

/// Normalize common LLM-introduced Unicode variants back to their ASCII form.
///
/// LLMs paste `old_string` from chat-rendered markdown which has often been
/// "auto-corrected" — smart quotes, em-dashes, NBSP, zero-width spaces.
/// The on-disk source code is plain ASCII, so the literal compare misses.
///
/// Kept in sync with `describe_fuzzy_diff` so the user-facing error message
/// can name the exact transformation.
fn fuzzy_normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mapped = match c {
            // Smart single quotes (incl. low-9, prime, modifier letter apostrophe)
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{02BC}' | '\u{2032}' => '\'',
            // Smart double quotes (incl. low-9, double prime)
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
            // Various Unicode dashes → ASCII hyphen
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Unicode spaces → ASCII space; zero-width characters dropped
            '\u{00A0}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}'
            | '\u{205F}' | '\u{3000}' => ' ',
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => continue,
            c => c,
        };
        out.push(mapped);
    }
    out
}

/// Human-readable summary of what `fuzzy_normalize` changed, for error
/// messages. Reports only categories that actually appeared.
fn describe_fuzzy_diff(original: &str, _normalized: &str) -> String {
    let mut hits: Vec<&str> = Vec::new();
    let mut has_smart_q = false;
    let mut has_dash = false;
    let mut has_space = false;
    let mut has_zerowidth = false;
    for c in original.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{02BC}' | '\u{2032}'
            | '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => has_smart_q = true,
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => has_dash = true,
            '\u{00A0}' | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}'
            | '\u{205F}' | '\u{3000}' => has_space = true,
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}' => has_zerowidth = true,
            _ => {}
        }
    }
    if has_smart_q {
        hits.push("smart quotes");
    }
    if has_dash {
        hits.push("Unicode dashes (en/em/figure)");
    }
    if has_space {
        hits.push("non-ASCII whitespace (NBSP/thin/full-width)");
    }
    if has_zerowidth {
        hits.push("zero-width characters");
    }
    if hits.is_empty() {
        "non-ASCII characters".to_owned()
    } else {
        hits.join(", ")
    }
}

fn sanitize_shell_redirects(cmd: &str) -> String {
    // Conservative: only fix the common `something2>&1` → `something 2>&1` case.
    // Broader rewrites risk changing user intent.
    // Regex-free: linear scan.
    let bytes = cmd.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        // Look for "2>&1" not preceded by whitespace
        if i + 4 <= bytes.len()
            && &bytes[i..i + 4] == b"2>&1"
            && i > 0
            && !bytes[i - 1].is_ascii_whitespace()
        {
            out.push(b' ');
            out.extend_from_slice(b"2>&1");
            i += 4;
            continue;
        }
        // Same for "&>/dev/null" etc.
        if i + 2 <= bytes.len()
            && &bytes[i..i + 2] == b"&>"
            && i > 0
            && !bytes[i - 1].is_ascii_whitespace()
        {
            out.push(b' ');
            out.extend_from_slice(b"&>");
            i += 2;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| cmd.to_owned())
}

#[cfg(test)]
mod shell_sanitize_tests {
    use super::sanitize_shell_redirects;
    #[test]
    fn fixes_glued_2redirect() {
        assert_eq!(
            sanitize_shell_redirects("ls /path/foo.png2>&1"),
            "ls /path/foo.png 2>&1"
        );
    }
    #[test]
    fn leaves_proper_redirect_alone() {
        assert_eq!(sanitize_shell_redirects("ls /path 2>&1"), "ls /path 2>&1");
    }
    #[test]
    fn fixes_glued_amp_redirect() {
        assert_eq!(
            sanitize_shell_redirects("cmd&>/dev/null"),
            "cmd &>/dev/null"
        );
    }
    #[test]
    fn no_op_on_normal_command() {
        assert_eq!(
            sanitize_shell_redirects("echo hello world"),
            "echo hello world"
        );
    }
}

// ---------------------------------------------------------------------------
// Safe filesystem walker (P0a)
//
// Replaces `glob::glob` for tool-side traversal. The story:
//   - `glob` follows symlinks, has no gitignore awareness, and runs
//     synchronously without yielding. A `glob::glob("/Users/$HOME/**/*")`
//     on a tree with one `node_modules` hoist symlink loop loops forever,
//     pinning a tokio worker thread.
//   - `tokio::time::timeout` does NOT save us — the future never reaches an
//     await point, so the timeout's drop-on-elapsed never fires.
//
// `safe_walk` runs the walk on a dedicated blocking thread via
// `spawn_blocking`, polls a wall-clock deadline at every directory entry,
// uses `ignore::WalkBuilder` (BurntSushi's gitignore-aware walker that
// powers ripgrep), and disables symlink-follow so `node_modules` loops
// cannot kill us.
// ---------------------------------------------------------------------------

/// Why a walk stopped early. Returned alongside results so the LLM (and
/// the user) can tell "I gave you the whole tree" from "I bailed at the
/// 50k file mark and there's probably more."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkTrunc {
    NotTruncated,
    /// Hit `max_results` matches — stopped accumulating but the walk
    /// itself ran to completion.
    MaxResults,
    /// Scanned through `max_scanned` directory entries before either
    /// reaching `max_results` matches or running out of tree.
    MaxScanned,
    /// Wall-clock deadline elapsed mid-walk.
    Deadline,
}

impl WalkTrunc {
    fn as_str(self) -> &'static str {
        match self {
            WalkTrunc::NotTruncated => "complete",
            WalkTrunc::MaxResults => "max_results",
            WalkTrunc::MaxScanned => "max_files_scanned",
            WalkTrunc::Deadline => "wall_clock_timeout",
        }
    }
}

/// One returned filesystem entry. Metadata is best-effort (None when the
/// stat call failed — e.g. permission denied, dangling symlink).
struct WalkEntry {
    path: std::path::PathBuf,
    is_dir: bool,
    size: u64,
    mtime: std::time::SystemTime,
}

struct WalkOutcome {
    entries: Vec<WalkEntry>,
    /// How many directory entries the walker visited (NOT how many matched).
    /// Useful telemetry — a search that returned 3 matches but scanned
    /// 200k entries probably needs a tighter `path` argument.
    scanned: usize,
    truncated: WalkTrunc,
}

/// Walk a directory tree safely.
///
/// - `root` — absolute path of the directory to walk. Must exist.
/// - `name_filter` — optional `globset` glob (matches against BASENAME).
///   `None` = include every entry. Pattern like `*.rs`, `Cargo.toml`,
///   `opencode` (literal-name match), etc.
/// - `recursive` — descend into subdirs. When false, only `root`'s direct
///   children are visited.
/// - `max_results` — cap on returned `entries`. Walk continues internally
///   only until this is hit (or one of the other limits).
/// - `max_scanned` — cap on directory entries the walker is allowed to
///   visit. Catches "result is rare but tree is enormous" scenarios.
/// - `deadline` — wall-clock cap. Walker exits cleanly when exceeded.
///
/// All blocking work happens inside `spawn_blocking`, so the caller's
/// async runtime can cancel via `tokio::select!` or task abort.
async fn safe_walk(
    root: std::path::PathBuf,
    name_filter: Option<String>,
    recursive: bool,
    max_results: usize,
    max_scanned: usize,
    deadline: Duration,
) -> Result<WalkOutcome> {
    use ignore::WalkBuilder;
    let started = std::time::Instant::now();

    let outcome = tokio::task::spawn_blocking(move || -> Result<WalkOutcome> {
        // Compile the optional basename filter. `None` => include everything.
        // Glob compile errors propagate to the LLM so it can fix the pattern.
        let matcher = match &name_filter {
            Some(p) => Some(
                globset::Glob::new(p)
                    .map_err(|e| anyhow!("invalid name pattern `{p}`: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let mut builder = WalkBuilder::new(&root);
        builder
            // .gitignore + global ignore + .ignore — sane defaults so
            // `target/`, `node_modules/`, `.venv/` etc. are skipped.
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .hidden(true) // skip dotfiles by default; LLM can pass an explicit dotpath
            // CRITICAL: don't follow symlinks. node_modules npm hoist
            // typically creates dir→dir symlink chains that loop.
            .follow_links(false)
            // Bound depth even when "recursive" — pathological trees
            // (e.g. a misconfigured FUSE mount) can be arbitrarily deep.
            .max_depth(if recursive { Some(32) } else { Some(1) });

        let mut entries: Vec<WalkEntry> = Vec::with_capacity(max_results.min(256));
        let mut scanned: usize = 0;
        let mut truncated = WalkTrunc::NotTruncated;

        for dent in builder.build() {
            scanned += 1;

            // Cheap checks first to keep the hot loop tight.
            if scanned >= max_scanned {
                truncated = WalkTrunc::MaxScanned;
                break;
            }
            // Poll the wall-clock every 256 entries — cheaper than per-entry
            // Instant::now() (which is a syscall on some platforms).
            if scanned & 0xff == 0 && started.elapsed() >= deadline {
                truncated = WalkTrunc::Deadline;
                break;
            }

            let dent = match dent {
                Ok(d) => d,
                // Permission denied / unreadable entry — skip silently.
                // ignore::Error already logs internally if RUST_LOG=ignore=debug.
                Err(_) => continue,
            };

            // Skip the root itself; ignore::Walk yields it as depth=0.
            if dent.depth() == 0 {
                continue;
            }

            let p = dent.path().to_path_buf();

            // Apply optional name filter against basename only — matches
            // the original `glob::glob("root/**/<pattern>")` semantics.
            if let Some(m) = &matcher {
                let name_match = p
                    .file_name()
                    .map(|n| {
                        let np: &std::path::Path = std::path::Path::new(n);
                        m.is_match(np)
                    })
                    .unwrap_or(false);
                if !name_match {
                    continue;
                }
            }

            let (is_dir, size, mtime) = match dent.metadata() {
                Ok(meta) => {
                    let is_dir = meta.is_dir();
                    let size = if is_dir { 0 } else { meta.len() };
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    (is_dir, size, mtime)
                }
                Err(_) => (
                    dent.file_type().map(|ft| ft.is_dir()).unwrap_or(false),
                    0,
                    std::time::UNIX_EPOCH,
                ),
            };

            entries.push(WalkEntry {
                path: p,
                is_dir,
                size,
                mtime,
            });

            if entries.len() >= max_results {
                truncated = WalkTrunc::MaxResults;
                break;
            }
        }

        Ok(WalkOutcome {
            entries,
            scanned,
            truncated,
        })
    })
    .await
    .map_err(|e| anyhow!("walker panicked: {e}"))??;

    Ok(outcome)
}

impl super::runtime::AgentRuntime {
    /// Default workspace path for this agent, used when a file tool was
    /// invoked without an explicit `path`. See [`resolve_default_workspace`]
    /// for the resolution rules — the wrapper just plumbs the runtime's
    /// fields in.
    pub(crate) fn default_workspace(&self) -> std::path::PathBuf {
        resolve_default_workspace(
            self.handle.config.workspace.as_deref(),
            self.config.agents.defaults.workspace.as_deref(),
            &crate::config::loader::base_dir(),
        )
    }

    /// List files and directories in a path (structured alternative to `exec
    /// ls`). Uses [`safe_walk`] so a huge or pathological tree (symlink
    /// loops, deeply nested `node_modules`) cannot wedge the worker.
    pub(crate) async fn tool_list_dir(&self, args: Value) -> Result<Value> {
        let default_ws = self.default_workspace();
        let path = args["path"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(expand_tilde)
            .unwrap_or(default_ws);
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let pattern_raw = args["pattern"].as_str().unwrap_or("*");
        // "*" means "no filter"; safe_walk treats None as "any entry"
        // which is faster (no globset compile + no per-entry match).
        let pattern = if pattern_raw == "*" { None } else { Some(pattern_raw.to_owned()) };

        if !path.exists() {
            return Ok(json!({"error": format!("path not found: {}", path.display())}));
        }
        if !path.is_dir() {
            return Ok(json!({"error": format!("not a directory: {}", path.display())}));
        }

        // Budgets:
        //   max_results 100 (back-compat with previous hard-cap).
        //   max_scanned 50k — `ls` should never need to look at more than that.
        //   deadline 30s — `ls` is interactive; if it can't finish in 30s
        //   on the first 50k entries something is wrong.
        let outcome = match safe_walk(
            path.clone(),
            pattern,
            recursive,
            100,
            50_000,
            Duration::from_secs(30),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => return Ok(json!({"error": format!("walk failed: {e}")})),
        };

        let entries: Vec<Value> = outcome
            .entries
            .iter()
            .map(|e| {
                let name = e
                    .path
                    .file_name()
                    .map(|n| {
                        let s = n.to_string_lossy().into_owned();
                        crate::util::truncate_str(&s, 200).to_owned()
                    })
                    .unwrap_or_default();
                json!({
                    "name": name,
                    "path": e.path.to_string_lossy(),
                    "is_dir": e.is_dir,
                    "size": e.size,
                })
            })
            .collect();

        let mut out = json!({
            "path": path.to_string_lossy(),
            "count": entries.len(),
            "entries": entries,
            "scanned": outcome.scanned,
            "status": outcome.truncated.as_str(),
        });
        if outcome.truncated != WalkTrunc::NotTruncated {
            out["truncated_note"] = json!(format!(
                "list_dir stopped early ({}). Narrow `path` or `pattern` to see more.",
                outcome.truncated.as_str()
            ));
        }
        Ok(out)
    }

    /// Search for files by name pattern (structured alternative to `exec
    /// find`). Uses [`safe_walk`] — gitignore-aware, no symlink-follow,
    /// hard wall-clock cap. The pre-2026-06-01 implementation used
    /// `glob::glob` synchronously which would hang indefinitely on trees
    /// with `node_modules` symlink loops or huge `target/` dirs because
    /// the iterator's blocking syscalls never yielded back to tokio for
    /// the dispatch_tool timeout to take effect.
    pub(crate) async fn tool_search_file(&self, args: Value) -> Result<Value> {
        let default_ws = self.default_workspace();
        let root_path = args["path"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(expand_tilde)
            .unwrap_or(default_ws);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_file: `pattern` required"))?
            .to_owned();
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        // Budgets:
        //   max_scanned 200k — generous for code searches, tight enough
        //   that pathological trees don't burn worker time forever.
        //   deadline 60s — search_file is an interactive tool; longer
        //   than this and the LLM should narrow `path` and retry.
        let outcome = match safe_walk(
            root_path.clone(),
            Some(pattern.clone()),
            true,
            max_results,
            200_000,
            Duration::from_secs(60),
        )
        .await
        {
            Ok(o) => o,
            Err(e) => return Ok(json!({"error": format!("search_file: {e}")})),
        };

        // Newest first — most recently modified files are usually most
        // relevant when an agent is investigating recent activity. With
        // truncation, "newest" is only over the scanned slice, but that's
        // the same caveat as before.
        let mut sorted = outcome.entries;
        sorted.sort_by(|a, b| b.mtime.cmp(&a.mtime));

        let results: Vec<Value> = sorted
            .into_iter()
            .map(|e| {
                json!({
                    "path": e.path.to_string_lossy(),
                    "size": e.size,
                    "is_dir": e.is_dir,
                })
            })
            .collect();

        let mut out = json!({
            "pattern": pattern,
            "root": root_path.to_string_lossy(),
            "count": results.len(),
            "scanned": outcome.scanned,
            "status": outcome.truncated.as_str(),
            "results": results,
        });
        if outcome.truncated != WalkTrunc::NotTruncated {
            out["truncated_note"] = json!(format!(
                "search_file stopped early ({}). Scanned {} entries. \
                 Narrow `path` (e.g. to a specific repo) or refine `pattern` to see more.",
                outcome.truncated.as_str(),
                outcome.scanned,
            ));
        }
        Ok(out)
    }

    /// Search file contents by pattern (structured alternative to `exec grep`).
    ///
    /// Prefers `ripgrep` when on PATH (cross-platform, supports `output_mode`
    /// and `multiline`). Falls back to `grep -rn` on Unix / `Select-String`
    /// on Windows when `rg` is not installed; `output_mode` and `multiline`
    /// are ignored on that path.
    pub(crate) async fn tool_search_content(&self, args: Value) -> Result<Value> {
        let default_ws = self.default_workspace();
        let root_path = args["path"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(expand_tilde)
            .unwrap_or(default_ws);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_content: `pattern` required"))?;
        let include = args["include"].as_str();
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;
        let multiline = args["multiline"].as_bool().unwrap_or(false);
        let output_mode = args["output_mode"].as_str().unwrap_or("content");

        if has_ripgrep().await {
            return run_ripgrep(
                pattern,
                &root_path,
                include,
                ignore_case,
                multiline,
                output_mode,
                max_results,
            )
            .await;
        }

        #[cfg(not(target_os = "windows"))]
        let output = {
            let mut cmd = tokio::process::Command::new("grep");
            cmd.arg("-rn");
            if ignore_case {
                cmd.arg("-i");
            }
            if let Some(inc) = include {
                cmd.arg("--include").arg(inc);
            }
            cmd.arg("--")
                .arg(pattern)
                .arg(root_path.as_os_str());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::null());
            tokio::time::timeout(Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| anyhow!("search_content: grep timed out after 15s. Narrow `path` to a smaller directory or add an `include` filter, then retry."))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        #[cfg(target_os = "windows")]
        let output = {
            // PowerShell Select-String is the Windows equivalent of grep -rn.
            let mut ps_args = vec!["-NoProfile".to_owned(), "-Command".to_owned()];
            let inc_filter = include
                .map(|i| format!(" -Include '{}'", i.replace('\'', "''")))
                .unwrap_or_default();
            let case_flag = if ignore_case { "" } else { " -CaseSensitive" };
            // Use TAB as separator to avoid conflicts with drive-letter colons in Windows
            // paths. Escape single quotes in all interpolated values to prevent
            // PowerShell injection.
            let safe_path = root_path.display().to_string().replace('\'', "''");
            let safe_pattern = pattern.replace('\'', "''");
            let ps_cmd = format!(
                "Get-ChildItem -Path '{safe_path}' -Recurse{inc_filter} -File | Select-String -Pattern '{safe_pattern}'{case_flag} | Select-Object -First {max_results} | ForEach-Object {{ \"$($_.Path)\t$($_.LineNumber)\t$($_.Line)\" }}"
            );
            ps_args.push(ps_cmd);
            let mut cmd = tokio::process::Command::new("powershell");
            cmd.args(&ps_args);
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::null());
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            tokio::time::timeout(Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| anyhow!("search_content: Select-String timed out after 15s. Narrow `path` to a smaller directory or add an `include` filter, then retry."))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut matches: Vec<Value> = Vec::new();
        // Windows uses TAB separator, Unix uses colon.
        let sep = if cfg!(target_os = "windows") {
            '\t'
        } else {
            ':'
        };
        for line in stdout.lines() {
            if matches.len() >= max_results {
                break;
            }
            // Parse: file<sep>line<sep>content
            // On Unix with colons: handle drive-less paths (no ambiguity).
            // On Windows with TABs: no ambiguity with path colons.
            let parts: Vec<&str> = line.splitn(3, sep).collect();
            if parts.len() == 3 {
                matches.push(json!({
                    "file": parts[0],
                    "line": parts[1].parse::<u64>().unwrap_or(0),
                    "content": parts[2].chars().take(200).collect::<String>(),
                }));
            }
        }

        Ok(json!({
            "pattern": pattern,
            "root": root_path.to_string_lossy(),
            "count": matches.len(),
            "matches": matches,
        }))
    }

    /// Read a file, with special handling for PDF and Office documents.
    pub(crate) async fn tool_read(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .or_else(|| args["filename"].as_str())
            .or_else(|| args["file"].as_str())
            .ok_or_else(|| anyhow!("read: `path` required"))?;
        let workspace = self.default_workspace();

        let full = canonicalize_external_path(path, &workspace);

        // Safety: block reading sensitive files
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        if safety_enabled {
            check_read_safety(path, &full)?;
        }

        let lower = path.to_lowercase();
        // Binary file types: extract text instead of raw read
        if lower.ends_with(".pdf") {
            let pdf_bytes = tokio::fs::read(&full)
                .await
                .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
            let content = match crate::agent::doc::safe_extract_pdf_from_mem(&pdf_bytes) {
                Ok(text) => text,
                Err(e) => {
                    // Fallback to pdftotext CLI
                    tracing::warn!("pdf-extract failed ({e}), trying pdftotext CLI");
                    #[allow(unused_mut)]
                    let mut pdf_cmd = tokio::process::Command::new("pdftotext");
                    pdf_cmd.args([full.to_str().unwrap_or(""), "-"]);
                    #[cfg(windows)]
                    {
                        use std::os::windows::process::CommandExt;
                        pdf_cmd.creation_flags(0x08000000);
                    }
                    let output = pdf_cmd.output().await.map_err(|e2| {
                        anyhow!(
                            "read `{}`: pdf extraction failed: {e}, pdftotext: {e2}",
                            full.display()
                        )
                    })?;
                    if !output.status.success() {
                        anyhow::bail!("read `{}`: pdf extraction failed: {e}", full.display());
                    }
                    String::from_utf8_lossy(&output.stdout).to_string()
                }
            };
            return Ok(json!({"content": content, "path": path}));
        }
        if lower.ends_with(".docx") || lower.ends_with(".xlsx") || lower.ends_with(".pptx") {
            let bytes = tokio::fs::read(&full)
                .await
                .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
            if let Some(text) = crate::channel::extract_office_text(path, &bytes) {
                return Ok(json!({"content": text, "path": path}));
            }
            anyhow::bail!(
                "read `{}`: could not extract text — the file may be corrupt, password-protected, or not a real OOXML document. Verify with exec `file <path>`, or tell the user this file cannot be parsed.",
                full.display()
            );
        }

        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;

        // Pagination: 1-indexed `offset` line + `limit` lines (default 2000).
        // Cheap path: when neither is set AND the file is small, skip the
        // line-split allocation and return the whole content. Otherwise
        // walk lines and slice. The runtime backstop still applies its
        // size cap on the final returned payload.
        const DEFAULT_LIMIT: usize = 2000;
        let offset_raw = args
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);
        let limit_raw = args.get("limit").and_then(|v| v.as_u64());
        let no_pagination = args.get("offset").is_none() && args.get("limit").is_none();

        if no_pagination && content.len() < 64 * 1024 {
            // Small file, caller didn't ask for pagination → original behavior.
            return Ok(json!({"content": content, "path": path}));
        }

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let offset = offset_raw as usize;
        let start = offset.saturating_sub(1);
        if start >= total_lines && total_lines > 0 {
            anyhow::bail!(
                "read `{}`: offset {offset} is beyond end of file ({total_lines} lines total)",
                path
            );
        }
        let limit = limit_raw.map(|n| n as usize).unwrap_or(DEFAULT_LIMIT).max(1);
        let end = (start + limit).min(total_lines);
        let slice = lines[start..end].join("\n");

        let mut result = json!({
            "content": slice,
            "path": path,
            "offset": offset,
            "lines_returned": end - start,
            "total_lines": total_lines,
        });
        if end < total_lines {
            result["truncated"] = json!(true);
            result["next_offset"] = json!(end + 1);
            result["hint"] = json!(format!(
                "[Showing lines {}-{} of {}. Use offset={} to continue.]",
                start + 1,
                end,
                total_lines,
                end + 1
            ));
        }
        Ok(result)
    }

    /// Write content to a file, creating parent directories as needed.
    pub(crate) async fn tool_write(&self, args: Value) -> Result<Value> {
        // Check if this is a malformed JSON case from streaming
        if let Some(parse_error) = args.get("_parse_error").and_then(|v| v.as_str()) {
            tracing::warn!("tool_write: received malformed JSON from model");
            let is_truncated = parse_error.starts_with("truncated:");
            return Ok(json!({
                "error": if is_truncated { "Your tool call was truncated by the API." } else { "Your tool call contained malformed JSON arguments." },
                "details": parse_error,
                "hint": if is_truncated {
                    "The API truncated your response. Split into multiple smaller writes (under 3500 chars each)."
                } else {
                    "Ensure all quotes/backslashes are escaped and JSON is complete."
                }
            }));
        }

        // Handle various parameter names LLMs might use.
        let path = args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .or_else(|| args["filename"].as_str())
            .or_else(|| args["file"].as_str())
            .or_else(|| args.as_str());
        // v1's raw-text tool params let structured values flow through
        // verbatim — writing a .json file often arrives as `content: {...}`
        // (object), not a string. The intent is unambiguous: serialize it
        // instead of bouncing the call with a "missing content" error the
        // model cannot act on (observed: 5x identical-retry loops).
        let content = match &args["content"] {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            other => Some(serde_json::to_string_pretty(other).unwrap_or_default()),
        };

        if path.is_none() || path.map(|p| p.is_empty()).unwrap_or(true) {
            let has_content = content.as_ref().map(|c| !c.is_empty()).unwrap_or(false);
            tracing::warn!(has_content, "tool_write: missing path parameter");
            return Ok(json!({
                "error": "Missing 'path' parameter. The write tool requires BOTH 'path' and 'content'.",
                "hint": "Retry with: {\"path\": \"file.py\", \"content\": \"...\"}"
            }));
        }

        if content.is_none() {
            tracing::warn!("tool_write: missing content parameter");
            return Ok(json!({
                "error": "Missing 'content' parameter.",
                "hint": "Provide a 'content' parameter with the text to write."
            }));
        }

        let path = path.unwrap().to_owned();
        let content = content.unwrap();

        // Office/PDF formats are binary containers — writing prose into a
        // file named *.docx produces a fake document that won't open.
        // Observed with the cold-tool stub live: the model writes the fake
        // instead of requesting create_docx. Redirect with an actionable
        // error (the create_* tools may be behind `request_tool`).
        let lower = path.to_lowercase();
        for (ext, tool) in [
            (".docx", "create_docx"),
            (".xlsx", "create_xlsx"),
            (".pptx", "create_pptx"),
            (".pdf", "create_pdf"),
        ] {
            if lower.ends_with(ext) {
                return Ok(json!({
                    "error": format!("`{ext}` is a binary format — write_file would produce a corrupt file that won't open."),
                    "hint": format!("Use the `{tool}` tool instead (if it is not in your tool list, call request_tool(name=\"{tool}\") first, then call it).")
                }));
            }
        }
        let workspace = self.default_workspace();

        let full = canonicalize_external_path(&path, &workspace);

        // Safety: block sensitive paths (only when tools.exec.safety = true)
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        if safety_enabled {
            check_write_safety(&path, &full, &content)?;
        }

        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&full, &content)
            .await
            .map_err(|e| anyhow!("write `{}`: {e}", full.display()))?;
        Ok(json!({"written": true, "path": path, "bytes": content.len()}))
    }

    /// Poll a background exec task by task_id.
    async fn exec_poll_task(&self, task_id: &str) -> Result<Value> {
        // Check if still running
        if self.exec_pool.is_running(task_id).await {
            return Ok(json!({
                "task_id": task_id,
                "status": "running",
                "message": "Task is still running. Poll again later."
            }));
        }
        // Try to collect result
        if let Some(result) = self.exec_pool.try_collect_by_task(task_id).await {
            let is_error = result.exit_code.map(|c| c != 0).unwrap_or(true);
            let mut payload = json!({
                "task_id": task_id,
                "status": "completed",
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "is_error": is_error,
            });
            if is_error && let Some(hint) = extract_cli_hint(&result.stderr) {
                payload["hint"] = json!(hint);
            }
            return Ok(payload);
        }
        Ok(json!({
            "task_id": task_id,
            "status": "not_found",
            "message": "No running or completed task with this ID. It may have already been collected."
        }))
    }

    /// Execute a shell command with timeout, safety checks, and sandbox
    /// support.
    ///
    /// Supports background execution via `wait=false` (default). When running
    /// in background mode, returns a `task_id` that can be polled with
    /// `task_id` parameter.
    pub(crate) async fn tool_exec(
        &self,
        ctx: &super::runtime::RunContext,
        tool_call_id: &str,
        args: Value,
    ) -> Result<Value> {
        tracing::debug!(?args, "tool_exec called");

        // Poll existing task
        if let Some(task_id) = args["task_id"].as_str() {
            return self.exec_poll_task(task_id).await;
        }
        // Default to synchronous: most commands (osascript, grep, ls) finish in
        // seconds. Background is an explicit opt-in for long-running tasks.
        let wait = args["wait"].as_bool().unwrap_or(true);
        // Accept both "command" (rsclaw native) and "cmd"+"args" (preparse/openclaw
        // format).
        let command = if let Some(cmd) = args["command"].as_str() {
            cmd.to_owned()
        } else if let Some(cmd) = args["cmd"].as_str() {
            // Reconstruct command string from cmd + args array.
            // Quote args containing spaces/special chars to preserve paths
            // like "C:/Program Files/chrome/chrome.exe".
            let cmd_args = args["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| {
                            if s.contains(' ') || s.contains('\"') || s.contains('\'') {
                                format!("\"{}\"", s.replace('\"', "\\\""))
                            } else {
                                s.to_owned()
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            if cmd_args.is_empty() {
                cmd.to_owned()
            } else {
                format!("{cmd} {cmd_args}")
            }
        } else {
            bail!("exec: `command` required");
        };
        // LLM writes `foo.png2>&1` (no space before the redirect) alarmingly
        // often. Bash parses that as "foo.png2" with `>&1` redirect, eating the
        // last character of the previous token. Fix common variants here so a
        // single misplaced space doesn't wreck the whole command.
        let command = sanitize_shell_redirects(&command);
        let command = command.as_str();

        // Safety check (only when tools.exec.safety = true)
        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);

        if safety_enabled {
            let preparse = crate::agent::preparse::PreParseEngine::load_with_safety(true);
            match preparse.check_exec_safety(command) {
                crate::agent::preparse::SafetyCheck::Allow => {}
                crate::agent::preparse::SafetyCheck::Deny(reason) => {
                    bail!("[blocked] {reason}");
                }
                crate::agent::preparse::SafetyCheck::Confirm(reason) => {
                    bail!("[needs confirmation] {reason}. Command: {command}. Do not retry on your own — ask the user to explicitly approve this command, and re-run it only after they confirm.");
                }
            }
        }

        // Always run via shell to support pipes, redirects, &&, etc.
        let (shell, shell_args) = if cfg!(target_os = "windows") {
            // PowerShell: better compatibility, supports pipes, redirects, && via -Command.
            // -ExecutionPolicy Bypass is essential for the agent to run the
            // language tooling rsclaw installs: `npm` / `npx` resolve to
            // npm.ps1 / npx.ps1 under PowerShell, and on a machine with the
            // default Restricted policy those .ps1 wrappers refuse to run
            // ("running scripts is disabled on this system"). Bypass applies
            // ONLY to this child process — it never changes the machine's
            // persistent policy. Without it `npm install` etc. just fails.
            ("powershell", vec!["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
        } else {
            ("sh", vec!["-c"])
        };

        let workspace = self.default_workspace();

        // Interpreter file scan + sandbox (only when safety enabled)
        if safety_enabled {
            let cmd_tokens: Vec<&str> = command.split_whitespace().collect();
            const INTERPRETERS: &[&str] = &[
                "bash",
                "sh",
                "zsh",
                "fish",
                "dash",
                "csh",
                "tcsh",
                "python",
                "python3",
                "python2",
                "ruby",
                "perl",
                "node",
                "bun",
                "deno",
                "powershell",
                "pwsh",
            ];
            if let Some(first) = cmd_tokens.first() {
                if INTERPRETERS
                    .iter()
                    .any(|i| first.ends_with(i) || *first == *i)
                {
                    if let Some(file_arg) = cmd_tokens.get(1) {
                        let file_path = std::path::Path::new(file_arg);
                        let resolved = if file_path.is_absolute() {
                            file_path.to_path_buf()
                        } else {
                            workspace.join(file_path)
                        };
                        check_file_content_safety(&resolved)?;
                    }
                }
            }

            // Sandbox: restrict file access to workspace only.
            let ws_canon = if workspace.exists() {
                std::fs::canonicalize(&workspace).unwrap_or_else(|_| workspace.clone())
            } else {
                workspace.clone()
            };
            for token in command.split_whitespace() {
                let is_abs = std::path::Path::new(token).is_absolute();
                if is_abs || token.contains("..") {
                    let resolved = if is_abs {
                        std::path::PathBuf::from(token)
                    } else {
                        workspace.join(token)
                    };
                    let canon = if resolved.exists() {
                        std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone())
                    } else {
                        resolved.clone()
                    };
                    if !canon.starts_with(&ws_canon) {
                        bail!(
                            "[sandbox] access denied: `{token}` is outside the workspace `{}`. Use paths under the workspace, or ask the user to relax `tools.exec.safety` if outside access is intended.",
                            ws_canon.display()
                        );
                    }
                }
            }
        }

        tracing::info!(cwd = %workspace.display(), command = %command, "exec: executing");

        // Timeout priority: tool call arg > config > default 30s.
        // The model can pass a timeout parameter for long-running commands.
        let config_timeout = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.timeout_seconds)
            .unwrap_or(30);
        let timeout_secs = args["timeout"]
            .as_u64()
            .map(|t| t.min(300)) // cap at 5 min from model, config can go higher
            .unwrap_or(config_timeout);

        let mut cmd = tokio::process::Command::new(shell);
        // Prepend ~/.rsclaw/tools/* to PATH so locally installed tools are found first.
        let tools_base = crate::config::loader::base_dir().join("tools");
        if tools_base.exists() {
            let mut extra_paths = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&tools_base) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        // Add the dir itself, bin/, node_modules/.bin/, and
                        // Scripts/. The last is where pip drops console-script
                        // shims on Windows (tools/python/Scripts/<tool>.exe),
                        // so a `pip install <cli>` followed by running that cli
                        // resolves. Harmless on Unix where Scripts/ won't exist.
                        extra_paths.push(p.join("node_modules").join(".bin"));
                        extra_paths.push(p.join("bin"));
                        extra_paths.push(p.join("Scripts"));
                        extra_paths.push(p.clone());
                    }
                }
            }
            if !extra_paths.is_empty() {
                let sys_path = std::env::var("PATH").unwrap_or_default();
                let mut all: Vec<String> = extra_paths
                    .iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                // Add common user bin paths that may be missing when launched
                // from a desktop app (macOS/Windows don't inherit shell profile).
                if let Some(home) = dirs_next::home_dir() {
                    for rel in &[".local/bin", ".cargo/bin", "bin", "go/bin", ".bun/bin"] {
                        let p = home.join(rel);
                        if p.exists() {
                            all.push(p.to_string_lossy().to_string());
                        }
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    for p in &["/opt/homebrew/bin", "/opt/homebrew/sbin", "/usr/local/bin"] {
                        if std::path::Path::new(p).exists() {
                            all.push(p.to_string());
                        }
                    }
                }
                all.push(sys_path);
                cmd.env("PATH", all.join(if cfg!(windows) { ";" } else { ":" }));
            }
        }
        // Trusted caller identity for league/competition tooling: carry the
        // CHANNEL-AUTHENTICATED sender (peer_id) into exec'd commands so tools
        // like `leaguetool connect`/`submit` bind to the VERIFIED identity
        // instead of an LLM-supplied (spoofable) value — closing Sybil/spoof
        // even against prompt injection. Works for ALL channels:
        //   wechat → openid, feishu → open_id, a2a → caller.id, …
        if !ctx.peer_id.is_empty() {
            cmd.env("RSCLAW_CALLER_ID", &ctx.peer_id);
            cmd.env("RSCLAW_CALLER_CHANNEL", &ctx.channel);
            // Back-compat alias for the earlier A2A-only env var.
            if ctx.channel == "a2a" {
                cmd.env("RSCLAW_A2A_CALLER", &ctx.peer_id);
            }
        }
        // Ensure the workspace exists before chdir. A dynamically-spawned
        // sub-agent's workspace (e.g. ~/.rsclaw/workspace/task-xxx) is only
        // recorded in the agent entry at spawn time, not created on disk —
        // and Command::current_dir on a missing directory fails the spawn
        // with ENOENT ("exec `ls`: No such file or directory (os error 2)")
        // even though the command itself is perfectly valid.
        if !workspace.exists() {
            if let Err(e) = std::fs::create_dir_all(&workspace) {
                tracing::warn!(
                    workspace = %workspace.display(),
                    error = %e,
                    "exec: failed to create workspace dir; falling back to base_dir"
                );
            }
        }
        cmd.args(&shell_args)
            .arg(command)
            .current_dir(&workspace)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let task_id = uuid::Uuid::new_v4().to_string();

        if wait {
            // Synchronous execution — wait for result.
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                cmd.output()
            )
            .await
            .map_err(|_| {
                tracing::warn!(command = %command, timeout_secs, "exec: timed out");
                anyhow!(
                    "Command timed out after {timeout_secs}s. Re-run with `wait: false` to run it in the background (results arrive next turn), or pass a larger `timeout` (up to 300s)."
                )
            })?
            .map_err(|e| anyhow!("exec `{command}`: {e}"))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::info!(cwd = %workspace.display(), command = %command, exit_code = ?output.status.code(), stdout_len = stdout.len(), stderr_len = stderr.len(), "exec: done");

            let mut result = json!({
                "task_id": task_id,
                "exit_code": output.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            });
            if output.status.code().map(|c| c != 0).unwrap_or(true)
                && let Some(hint) = extract_cli_hint(&stderr)
            {
                result["hint"] = json!(hint);
            }
            Ok(result)
        } else {
            // Background execution — spawn and return task_id immediately.
            // The result will be collected by exec_pool on the next turn.
            let pool = std::sync::Arc::clone(&ctx.exec_pool);
            let session_key = ctx.session_key.clone();
            let tool_call_id_owned = tool_call_id.to_owned();
            let command_owned = command.to_owned();

            tracing::info!(task_id = %task_id, command = %command, "exec: spawning background task");

            let tid = task_id.clone();
            tokio::spawn(async move {
                let started_at = std::time::Instant::now();
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(timeout_secs),
                    cmd.output(),
                )
                .await;

                let (exit_code, stdout, stderr) = match result {
                    Ok(Ok(output)) => {
                        let exit_code = output.status.code();
                        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        (exit_code, stdout, stderr)
                    }
                    Ok(Err(e)) => {
                        tracing::error!(task_id = %tid, "exec background spawn failed: {}", e);
                        (None, String::new(), format!("spawn error: {}", e))
                    }
                    Err(_) => {
                        tracing::warn!(task_id = %tid, timeout_secs, "exec background timed out");
                        (
                            None,
                            String::new(),
                            format!("timed out after {} seconds", timeout_secs),
                        )
                    }
                };

                let completed_at = std::time::Instant::now();
                tracing::info!(
                    task_id = %tid,
                    exit_code = ?exit_code,
                    stdout_len = stdout.len(),
                    stderr_len = stderr.len(),
                    "exec background completed"
                );

                let exec_result = super::exec_pool::ExecResult {
                    task_id: tid,
                    tool_call_id: tool_call_id_owned,
                    command: command_owned,
                    exit_code,
                    stdout,
                    stderr,
                    started_at,
                    completed_at,
                };

                pool.add_pending_for_session(session_key, exec_result).await;
            });

            Ok(json!({
                "task_id": task_id,
                "status": "running",
                "message": "Command started in background. Results will be delivered on your next turn, or poll with task_id."
            }))
        }
    }

    /// Edit a file by exact-string replacement (Edit-style, not full
    /// overwrite).
    ///
    /// Fails fast if `old_string` is absent or non-unique (unless
    /// `replace_all`). The agent is expected to `read_file` first so it can
    /// copy a verbatim substring; the check is not enforced server-side.
    pub(crate) async fn tool_edit(&self, args: Value) -> Result<Value> {
        let path = args["path"]
            .as_str()
            .or_else(|| args["file_path"].as_str())
            .ok_or_else(|| anyhow!("edit_file: `path` required"))?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| anyhow!("edit_file: `old_string` required"))?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| anyhow!("edit_file: `new_string` required"))?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        if old_string.is_empty() {
            bail!("edit_file: `old_string` must be non-empty — use write_file to create new files");
        }
        if old_string == new_string {
            bail!("edit_file: `old_string` and `new_string` are identical — no change requested");
        }

        let workspace = self.default_workspace();
        let full = canonicalize_external_path(path, &workspace);

        let safety_enabled = self
            .config
            .ext
            .tools
            .as_ref()
            .and_then(|t| t.exec.as_ref())
            .and_then(|e| e.safety)
            .unwrap_or(false);
        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| anyhow!("edit_file: cannot read `{}`: {e}", full.display()))?;

        let count = content.matches(old_string).count();
        if count == 0 {
            // LLM-side gotcha: models sometimes copy old_string from a
            // markdown-rendered code block, where ASCII quotes got smart-quoted,
            // hyphens turned into em-dashes, regular spaces turned into NBSP,
            // etc. Detect the case and tell the LLM precisely what's wrong —
            // much more actionable than a bare "not found".
            let norm_content = fuzzy_normalize(&content);
            let norm_old = fuzzy_normalize(old_string);
            if !norm_old.is_empty() && norm_content.contains(&norm_old) {
                let diffs = describe_fuzzy_diff(old_string, &norm_old);
                bail!(
                    "edit_file: `old_string` not found verbatim in `{}`, but a fuzzy match exists. \
                     Your old_string appears to contain non-ASCII variants: {}. \
                     Re-read the file and copy the literal characters from disk, \
                     not from a chat / markdown rendering.",
                    path, diffs
                );
            }
            bail!(
                "edit_file: `old_string` not found in `{}`. Read the file first and copy the exact substring (including whitespace).",
                path
            );
        }
        if count > 1 && !replace_all {
            bail!(
                "edit_file: `old_string` matches {count} locations in `{}`. Add surrounding context to make it unique, or pass `replace_all: true` to replace every occurrence.",
                path
            );
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            content.replacen(old_string, new_string, 1)
        };

        if safety_enabled {
            // Reuse the write-safety gate: same predicate as write_file —
            // checks both the destination path and the post-edit content.
            check_write_safety(path, &full, &new_content)?;
        }

        tokio::fs::write(&full, &new_content)
            .await
            .map_err(|e| anyhow!("edit_file: cannot write `{}`: {e}", full.display()))?;

        Ok(json!({
            "ok": true,
            "path": path,
            "replacements": if replace_all { count } else { 1 },
            "bytes": new_content.len(),
        }))
    }
}

/// Check whether `ripgrep` (`rg`) is on PATH. Re-checked per call (~20 ms);
/// the cost is dwarfed by the actual search and avoids a global mutable.
async fn has_ripgrep() -> bool {
    #[allow(unused_mut)]
    let mut rg_cmd = tokio::process::Command::new("rg");
    rg_cmd
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        rg_cmd.creation_flags(0x08000000);
    }
    rg_cmd
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a ripgrep search with full support for `output_mode` and `multiline`.
async fn run_ripgrep(
    pattern: &str,
    root: &std::path::Path,
    include: Option<&str>,
    ignore_case: bool,
    multiline: bool,
    output_mode: &str,
    max_results: usize,
) -> Result<Value> {
    let mut cmd = tokio::process::Command::new("rg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    match output_mode {
        "files_with_matches" => {
            cmd.arg("--files-with-matches");
        }
        "count" => {
            cmd.arg("--count");
        }
        _ => {
            cmd.args(["--line-number", "--no-heading"]);
        }
    }

    if ignore_case {
        cmd.arg("--ignore-case");
    }
    if multiline {
        cmd.args(["--multiline", "--multiline-dotall"]);
    }
    if let Some(inc) = include {
        cmd.arg("--glob").arg(inc);
    }

    cmd.arg("--").arg(pattern).arg(root);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());

    let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .map_err(|_| anyhow!("search_content: ripgrep timed out after 15s. Narrow `path` to a smaller directory or add an `include` glob, then retry."))?
        .map_err(|e| anyhow!("search_content: {e}"))?;

    let text = String::from_utf8_lossy(&output.stdout);
    let results: Vec<String> = text.lines().take(max_results).map(String::from).collect();

    Ok(json!({
        "pattern": pattern,
        "root": root.to_string_lossy(),
        "count": results.len(),
        "output_mode": output_mode,
        "engine": "ripgrep",
        "results": results,
    }))
}
