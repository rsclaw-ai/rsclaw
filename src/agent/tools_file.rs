//! File/exec-related tool methods extracted from `runtime.rs`.
//!
//! Contains: `tool_list_dir`, `tool_search_file`,
//! `tool_search_content`, `tool_read`, `tool_write`, `tool_exec`.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::runtime::{canonicalize_external_path, expand_tilde};
use super::security::{check_file_content_safety, check_read_safety, check_write_safety};

/// Fix common LLM shell-writing mistakes around redirects that bash would
/// otherwise silently mis-parse. The target is constructs like
/// `foo.png2>&1` (bash reads as `foo.png2` + `>&1`), which eats the last
/// character of a preceding token.
///
/// Rules applied:
/// 1. Insert a space before `2>&1` / `&>` / `>&N` redirects when they are
///    glued to a previous non-space, non-redirect character.
/// 2. Same for plain `>` / `>>` / `<` when glued (but only when the prior
///    token ends with a digit or letter — don't break `1>&2` etc).
/// 3. Leave the rest of the command untouched.
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
        assert_eq!(
            sanitize_shell_redirects("ls /path 2>&1"),
            "ls /path 2>&1"
        );
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

impl super::runtime::AgentRuntime {
    /// List files and directories in a path (structured alternative to `exec ls`).
    pub(crate) async fn tool_list_dir(&self, args: Value) -> Result<Value> {
        let fallback_ws = crate::config::loader::base_dir().join("workspace");
        let default_ws = self.handle.config.workspace.as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .unwrap_or(fallback_ws.to_str().unwrap_or("."));
        let path_str = args["path"].as_str().unwrap_or(default_ws);
        let path = expand_tilde(path_str);
        let recursive = args["recursive"].as_bool().unwrap_or(false);
        let pattern = args["pattern"].as_str().unwrap_or("*");

        if !path.exists() {
            return Ok(json!({"error": format!("path not found: {}", path.display())}));
        }
        if !path.is_dir() {
            return Ok(json!({"error": format!("not a directory: {}", path.display())}));
        }

        let glob_pattern = if recursive {
            format!("{}/**/{}", path.display(), pattern)
        } else {
            format!("{}/{}", path.display(), pattern)
        };

        let mut entries: Vec<Value> = Vec::new();
        let entries_iter = match glob::glob(&glob_pattern) {
            Ok(iter) => iter,
            Err(e) => return Ok(json!({"error": format!("invalid pattern: {e}")})),
        };
        for entry in entries_iter {
            if entries.len() >= 100 { break; }
            if let Ok(p) = entry {
                let is_dir = p.is_dir();
                let size = if is_dir { 0 } else { p.metadata().map(|m| m.len()).unwrap_or(0) };
                entries.push(json!({
                    "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                    "path": p.to_string_lossy(),
                    "is_dir": is_dir,
                    "size": size,
                }));
            }
        }

        Ok(json!({
            "path": path.to_string_lossy(),
            "count": entries.len(),
            "entries": entries,
        }))
    }

    /// Search for files by name pattern (structured alternative to `exec find`).
    pub(crate) async fn tool_search_file(&self, args: Value) -> Result<Value> {
        let fallback_ws = crate::config::loader::base_dir().join("workspace");
        let default_ws = self.handle.config.workspace.as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .unwrap_or(fallback_ws.to_str().unwrap_or("."));
        let root = args["path"].as_str().unwrap_or(default_ws);
        let root_path = expand_tilde(root);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_file: `pattern` required"))?;
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        let glob_pattern = format!("{}/**/{}", root_path.display(), pattern);
        let mut results: Vec<Value> = Vec::new();
        let entries_iter = match glob::glob(&glob_pattern) {
            Ok(iter) => iter,
            Err(e) => return Ok(json!({"error": format!("invalid pattern: {e}")})),
        };
        for entry in entries_iter {
            if results.len() >= max_results { break; }
            if let Ok(p) = entry {
                let size = p.metadata().map(|m| m.len()).unwrap_or(0);
                results.push(json!({
                    "path": p.to_string_lossy(),
                    "size": size,
                    "is_dir": p.is_dir(),
                }));
            }
        }

        Ok(json!({
            "pattern": pattern,
            "root": root_path.to_string_lossy(),
            "count": results.len(),
            "results": results,
        }))
    }

    /// Search file contents by pattern (structured alternative to `exec grep`).
    ///
    /// Cross-platform: uses `grep -rn` on Unix, `Select-String` on Windows.
    pub(crate) async fn tool_search_content(&self, args: Value) -> Result<Value> {
        let fallback_ws = crate::config::loader::base_dir().join("workspace");
        let default_ws = self.handle.config.workspace.as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .unwrap_or(fallback_ws.to_str().unwrap_or("."));
        let root = args["path"].as_str().unwrap_or(default_ws);
        let root_path = expand_tilde(root);
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| anyhow!("search_content: `pattern` required"))?;
        let include = args["include"].as_str();
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let max_results = args["max_results"].as_u64().unwrap_or(20) as usize;

        #[cfg(not(target_os = "windows"))]
        let output = {
            let mut cmd = tokio::process::Command::new("grep");
            cmd.arg("-rn");
            if ignore_case { cmd.arg("-i"); }
            if let Some(inc) = include {
                cmd.arg("--include").arg(inc);
            }
            cmd.arg("--").arg(pattern).arg(root_path.to_str().unwrap_or("."));
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::null());
            tokio::time::timeout(Duration::from_secs(15), cmd.output())
                .await
                .map_err(|_| anyhow!("search_content: timed out"))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        #[cfg(target_os = "windows")]
        let output = {
            // PowerShell Select-String is the Windows equivalent of grep -rn.
            let mut ps_args = vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
            ];
            let inc_filter = include
                .map(|i| format!(" -Include '{}'", i.replace('\'', "''")))
                .unwrap_or_default();
            let case_flag = if ignore_case { "" } else { " -CaseSensitive" };
            // Use TAB as separator to avoid conflicts with drive-letter colons in Windows paths.
            // Escape single quotes in all interpolated values to prevent PowerShell injection.
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
                .map_err(|_| anyhow!("search_content: timed out"))?
                .map_err(|e| anyhow!("search_content: {e}"))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut matches: Vec<Value> = Vec::new();
        // Windows uses TAB separator, Unix uses colon.
        let sep = if cfg!(target_os = "windows") { '\t' } else { ':' };
        for line in stdout.lines() {
            if matches.len() >= max_results { break; }
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
        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

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
                    let output = tokio::process::Command::new("pdftotext")
                        .args([full.to_str().unwrap_or(""), "-"])
                        .output()
                        .await
                        .map_err(|e2| {
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
            anyhow::bail!("read `{}`: failed to extract office text", full.display());
        }

        let content = tokio::fs::read_to_string(&full)
            .await
            .map_err(|e| anyhow!("read `{}`: {e}", full.display()))?;
        Ok(json!({"content": content, "path": path}))
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
        let content = args["content"].as_str();

        if path.is_none() || path.map(|p| p.is_empty()).unwrap_or(true) {
            let has_content = content.map(|c| !c.is_empty()).unwrap_or(false);
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
        let content = content.unwrap().to_owned();
        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

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

        // Syntax check for script files
        let syntax_result = super::syntax_check::check_syntax(&full, &content);

        if let Some(syntax_err) = syntax_result {
            // Return both success + syntax warning
            Ok(json!({
                "written": true,
                "path": path,
                "bytes": content.len(),
                "syntax_check": syntax_err
            }))
        } else {
            Ok(json!({"written": true, "path": path, "bytes": content.len()}))
        }
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
            return Ok(json!({
                "task_id": task_id,
                "status": "completed",
                "exit_code": result.exit_code,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "is_error": is_error,
            }));
        }
        Ok(json!({
            "task_id": task_id,
            "status": "not_found",
            "message": "No running or completed task with this ID. It may have already been collected."
        }))
    }

    /// Execute a shell command with timeout, safety checks, and sandbox support.
    ///
    /// Supports background execution via `wait=false` (default). When running
    /// in background mode, returns a `task_id` that can be polled with
    /// `task_id` parameter.
    pub(crate) async fn tool_exec(&self, ctx: &super::runtime::RunContext, tool_call_id: &str, args: Value) -> Result<Value> {
        tracing::debug!(?args, "tool_exec called");

        // Poll existing task
        if let Some(task_id) = args["task_id"].as_str() {
            return self.exec_poll_task(task_id).await;
        }
        // Default to synchronous: most commands (osascript, grep, ls) finish in
        // seconds. Background is an explicit opt-in for long-running tasks.
        let wait = args["wait"].as_bool().unwrap_or(true);
        // Accept both "command" (rsclaw native) and "cmd"+"args" (preparse/openclaw format).
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
                    bail!("[needs confirmation] {reason}. Command: {command}");
                }
            }
        }

        // Always run via shell to support pipes, redirects, &&, etc.
        let (shell, shell_args) = if cfg!(target_os = "windows") {
            // PowerShell: better compatibility, supports pipes, redirects, && via -Command
            ("powershell", vec!["-NoProfile", "-Command"])
        } else {
            ("sh", vec!["-c"])
        };

        let workspace = self
            .handle
            .config
            .workspace
            .as_deref()
            .or(self.config.agents.defaults.workspace.as_deref())
            .map(expand_tilde)
            .unwrap_or_else(|| crate::config::loader::base_dir().join("workspace"));

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
                        bail!("[sandbox] access denied: path `{token}` is outside workspace");
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
                        // Add the dir itself, bin/, and node_modules/.bin/ subdirectories.
                        extra_paths.push(p.join("node_modules").join(".bin"));
                        extra_paths.push(p.join("bin"));
                        extra_paths.push(p.clone());
                    }
                }
            }
            if !extra_paths.is_empty() {
                let sys_path = std::env::var("PATH").unwrap_or_default();
                let mut all: Vec<String> = extra_paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
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
                    "Command timed out after {timeout_secs}s. Use wait=false (default) for long-running commands."
                )
            })?
            .map_err(|e| anyhow!("exec `{command}`: {e}"))?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::info!(cwd = %workspace.display(), command = %command, exit_code = ?output.status.code(), stdout_len = stdout.len(), stderr_len = stderr.len(), "exec: done");

            Ok(json!({
                "task_id": task_id,
                "exit_code": output.status.code(),
                "stdout": stdout,
                "stderr": stderr,
            }))
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
                    cmd.output()
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
                        (None, String::new(), format!("timed out after {} seconds", timeout_secs))
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
}
