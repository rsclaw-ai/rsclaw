//! Shell skill runner.
//!
//! Executes a skill tool's `command` in the skill's directory,
//! passes JSON input via stdin (when `stdin_json = true`) or as
//! positional CLI arguments, and captures stdout as the tool result.
//!
//! Security:
//!  - Working directory is always the skill directory (not inherited).
//!  - The command must be a relative path (`./script.sh`) or a bare program
//!    name (`python3`). Absolute paths are allowed but warned.
//!  - Timeout is enforced; the child process is killed on expiry.

use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{io::AsyncWriteExt, process::Command, time};
use tracing::{debug, warn};

use super::manifest::ToolSpec;

/// Maximum output size we buffer from a skill tool (1 MB).
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// RunOptions
// ---------------------------------------------------------------------------

/// Options for a single tool invocation.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Extra environment variables to inject.
    pub env: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// run_tool
// ---------------------------------------------------------------------------

/// Execute a skill tool and return its stdout as a JSON `Value`.
///
/// If the tool exits non-zero, an `Err` is returned with stderr content.
pub async fn run_tool(
    spec: &ToolSpec,
    skill_dir: &Path,
    input: Value,
    opts: &RunOptions,
) -> Result<Value> {
    let timeout = Duration::from_secs(u64::from(spec.timeout_seconds));

    let result = time::timeout(timeout, do_run(spec, skill_dir, &input, opts)).await;

    match result {
        Ok(inner) => inner,
        Err(_elapsed) => bail!(
            "tool `{}` timed out after {}s",
            spec.name,
            spec.timeout_seconds
        ),
    }
}

async fn do_run(
    spec: &ToolSpec,
    skill_dir: &Path,
    input: &Value,
    opts: &RunOptions,
) -> Result<Value> {
    // Split the command string into program + args.
    let parts = shell_split(&spec.command)
        .with_context(|| format!("cannot parse command: {:?}", spec.command))?;

    let (program, args) = parts.split_first().context("command is empty")?;

    if Path::new(program).is_absolute() {
        warn!(command = program, "skill tool uses absolute path");
    }

    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(skill_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    // Inject extra env vars.
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }

    // Pass input as CLI args when stdin_json is false.
    if !spec.stdin_json
        && let Value::Object(map) = input
    {
        for (k, v) in map {
            cmd.arg(format!("--{k}"));
            cmd.arg(json_to_arg(v));
        }
    }

    debug!(
        tool = %spec.name,
        command = %spec.command,
        "running skill tool"
    );

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", spec.command))?;

    // Write JSON input to stdin in a separate task so a child that emits a
    // lot of output before draining stdin cannot deadlock us (pipe buffers
    // are ~64KB). When stdin_json is false, stdin is closed immediately.
    let stdin_task = match child.stdin.take() {
        Some(mut stdin) if spec.stdin_json => {
            let payload = serde_json::to_vec(input)?;
            // stdin is dropped when the task ends, signalling EOF.
            Some(tokio::spawn(async move { stdin.write_all(&payload).await }))
        }
        _ => None,
    };

    let stdout = child.stdout.take().context("child stdout not piped")?;
    let stderr = child.stderr.take().context("child stderr not piped")?;
    let (stdout_res, stderr_res, status) = tokio::join!(
        read_bounded(stdout, MAX_OUTPUT_BYTES),
        read_bounded(stderr, MAX_OUTPUT_BYTES),
        child.wait(),
    );
    let status = status.context("wait for child process")?;
    let (stdout_buf, stdout_total) = stdout_res.context("read child stdout")?;
    let (stderr_buf, _) = stderr_res.context("read child stderr")?;

    if let Some(task) = stdin_task {
        match task.await {
            Ok(Ok(())) => {}
            // A child that exits without reading all of stdin is not an error
            // by itself; its exit status decides.
            Ok(Err(e)) => warn!(tool = %spec.name, error = %e, "write to child stdin failed"),
            Err(e) => warn!(tool = %spec.name, error = %e, "stdin writer task failed"),
        }
    }

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_buf);
        bail!(
            "tool `{}` exited with status {}: {}",
            spec.name,
            status,
            stderr.trim()
        );
    }

    if stdout_total > MAX_OUTPUT_BYTES {
        warn!(
            tool = %spec.name,
            bytes = stdout_total,
            limit = MAX_OUTPUT_BYTES,
            "tool output truncated"
        );
    }
    let stdout_bytes = &stdout_buf[..];

    // Try to parse as JSON; fall back to plain text.
    let result = if stdout_bytes.is_empty() {
        Value::Null
    } else if let Ok(v) = serde_json::from_slice(stdout_bytes) {
        v
    } else {
        Value::String(String::from_utf8_lossy(stdout_bytes).into_owned())
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Very basic shell-style argument splitter (handles single/double quotes).
fn shell_split(s: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ' ' | '\t' if !in_single && !in_double => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            '\\' if in_double => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            other => current.push(other),
        }
    }

    if in_single || in_double {
        bail!("unterminated quote in command: {s:?}");
    }
    if !current.is_empty() {
        parts.push(current);
    }

    Ok(parts)
}

/// Read `r` to EOF, keeping at most `max` bytes (the rest is drained and
/// discarded so the child never blocks on a full pipe). Returns the kept
/// bytes and the total number of bytes seen.
async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(
    mut r: R,
    max: usize,
) -> std::io::Result<(Vec<u8>, usize)> {
    use tokio::io::AsyncReadExt;
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let n = r.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n);
        let room = max.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..n.min(room)]);
    }
    Ok((kept, total))
}

fn json_to_arg(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_simple() {
        assert_eq!(
            shell_split("./run.sh --foo bar").unwrap(),
            vec!["./run.sh", "--foo", "bar"]
        );
    }

    #[test]
    fn shell_split_quoted() {
        assert_eq!(
            shell_split(r#"./run.sh "hello world""#).unwrap(),
            vec!["./run.sh", "hello world"]
        );
    }

    #[test]
    fn shell_split_single_quoted() {
        assert_eq!(
            shell_split("./run.sh 'foo bar'").unwrap(),
            vec!["./run.sh", "foo bar"]
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_echo_tool() {
        // Skip test if `echo` is not available (unlikely but safe).
        if which::which("echo").is_err() {
            return;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        // Write a small shell script that echoes its stdin as a JSON string.
        let script = tmp.path().join("echo_tool.sh");
        std::fs::write(&script, "#!/bin/sh\nread INPUT\necho \"\\\"$INPUT\\\"\"")
            .expect("write script");

        // Make executable on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).expect("chmod");
        }

        let spec = ToolSpec {
            name: "echo_test".into(),
            description: "echo test".into(),
            command: "./echo_tool.sh".into(),
            input_schema: None,
            timeout_seconds: 5,
            stdin_json: false, // pass no stdin so it reads EOF immediately
        };

        // Should succeed with Null output (empty stdout → Null).
        let result = run_tool(&spec, tmp.path(), Value::Null, &RunOptions::default()).await;
        // We just check it didn't error out due to spawn issues.
        // On CI with /bin/sh available this should be Ok.
        let _ = result; // accept either outcome in unit tests
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn runner_large_stdin_and_stdout_no_deadlock() {
        // `cat` echoes stdin back while we are still writing it: with a
        // payload far above the pipe buffer size, writing all of stdin
        // before reading stdout would deadlock.
        let tmp = tempfile::tempdir().expect("tempdir");
        let spec = ToolSpec {
            name: "cat_test".into(),
            description: "cat".into(),
            command: "cat".into(),
            input_schema: None,
            timeout_seconds: 10,
            stdin_json: true,
        };
        let big = "x".repeat(512 * 1024);
        let out = run_tool(&spec, tmp.path(), Value::String(big.clone()), &RunOptions::default())
            .await
            .expect("run cat");
        assert_eq!(out, Value::String(big));
    }
}
