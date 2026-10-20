//! Screen observer: an optional local process that turns a screenshot into a
//! structured description of the screen (`vision-observe`).
//!
//! The host does not ship an observer. When one is installed, plugins reach it
//! through `android-call("vision-observe", …)`; without one that command
//! answers `vision-unavailable: …`, which a plugin treats as "no vision, fall
//! back".
//!
//! The observer is any executable speaking JSON lines on stdin/stdout, started
//! as `<observer> serve` and kept running (its models load once):
//!
//! ```text
//! observer → host   {"ready": true, "versions": {...}}                 once, after loading
//! host → observer   {"id": 7, "args": {...}, "image": "<base64 png|jpeg>"}
//! observer → host   {"id": 7, "ok": true, "answer": {...}}
//!                   {"id": 7, "ok": false, "error": "..."}
//! ```
//!
//! `args` are the plugin's request with the image taken out; `answer` goes back
//! to the plugin as it is, with the host's additions (the screenshot it took).
//! Requests are served one at a time. A process that dies, hangs or answers out
//! of turn is killed; the next request starts a new one.
//!
//! The executable is found at `RSCLAW_VISION_OBSERVER`, or else next to the
//! running host binary as `rsclaw-vision-observer` (`.exe` on Windows).

use std::{path::PathBuf, process::Stdio, sync::OnceLock, time::Duration};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    sync::Mutex,
};

/// Environment variable naming the observer executable.
pub const OBSERVER_ENV: &str = "RSCLAW_VISION_OBSERVER";
const OBSERVER_NAME: &str = "rsclaw-vision-observer";
/// Loading the models: the first start may take a while.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Observer {
    // Held so the process lives as long as this value and is killed with it.
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    versions: Value,
}

fn slot() -> &'static Mutex<Option<Observer>> {
    static SLOT: OnceLock<Mutex<Option<Observer>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Where the observer executable is, if one is installed.
pub fn observer_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(OBSERVER_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        format!("{OBSERVER_NAME}.exe")
    } else {
        OBSERVER_NAME.to_string()
    };
    let beside = std::env::current_exe().ok()?.parent()?.join(name);
    beside.is_file().then_some(beside)
}

async fn start() -> Result<Observer, String> {
    let path = observer_path()
        .ok_or_else(|| "vision-unavailable: no screen observer installed".to_string())?;
    let mut command = tokio::process::Command::new(&path);
    command
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn().map_err(|error| {
        format!(
            "vision-unavailable: cannot start `{}`: {error}",
            path.display()
        )
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "vision-failed: observer stdin not captured".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "vision-failed: observer stdout not captured".to_string())?;
    let mut stdout = BufReader::new(stdout);
    let ready = read_line(&mut stdout, READY_TIMEOUT)
        .await
        .map_err(|error| format!("vision-unavailable: observer did not get ready: {error}"))?;
    if ready.get("ready").and_then(Value::as_bool) != Some(true) {
        let reason = ready
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("no ready line");
        return Err(format!("vision-unavailable: {reason}"));
    }
    let versions = ready.get("versions").cloned().unwrap_or(Value::Null);
    tracing::info!(path = %path.display(), %versions, "screen observer ready");
    Ok(Observer {
        _child: child,
        stdin,
        stdout,
        next_id: 1,
        versions,
    })
}

async fn read_line(
    stdout: &mut BufReader<ChildStdout>,
    timeout: Duration,
) -> Result<Value, String> {
    let mut line = String::new();
    let read = tokio::time::timeout(timeout, stdout.read_line(&mut line))
        .await
        .map_err(|_| format!("no answer within {}s", timeout.as_secs()))?
        .map_err(|error| format!("read: {error}"))?;
    if read == 0 {
        return Err("the process exited".to_string());
    }
    serde_json::from_str(line.trim()).map_err(|error| format!("not a JSON line: {error}"))
}

async fn ask(observer: &mut Observer, args: &Value, image: &[u8]) -> Result<Value, String> {
    let id = observer.next_id;
    observer.next_id += 1;
    let request = json!({
        "id": id,
        "args": args,
        "image": base64::engine::general_purpose::STANDARD.encode(image),
    });
    let mut line = request.to_string();
    line.push('\n');
    observer
        .stdin
        .write_all(line.as_bytes())
        .await
        .map_err(|error| format!("write: {error}"))?;
    observer
        .stdin
        .flush()
        .await
        .map_err(|error| format!("flush: {error}"))?;
    let reply = read_line(&mut observer.stdout, REQUEST_TIMEOUT).await?;
    if reply.get("id").and_then(Value::as_u64) != Some(id) {
        return Err(format!("answer out of turn (expected id {id})"));
    }
    if reply.get("ok").and_then(Value::as_bool) == Some(true) {
        return reply
            .get("answer")
            .cloned()
            .ok_or_else(|| "ok without an answer".to_string());
    }
    let error = reply
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    // The observer's own refusal: it stays up, the request failed.
    Ok(json!({ "__observer_error": error }))
}

/// Describe `image` (PNG or JPEG bytes) with the installed screen observer.
///
/// `args` is the plugin's request without the image. Errors are strings
/// prefixed `vision-unavailable:` (no observer, or it cannot start) or
/// `vision-failed:` (this request failed); a plugin treats both as abstain.
pub async fn observe(args: &Value, image: &[u8]) -> Result<Value, String> {
    let mut guard = slot().lock().await;
    if guard.is_none() {
        *guard = Some(start().await?);
    }
    let Some(observer) = guard.as_mut() else {
        return Err("vision-failed: observer not running".to_string());
    };
    match ask(observer, args, image).await {
        Ok(answer) => match answer.get("__observer_error").and_then(Value::as_str) {
            Some(error) => Err(format!("vision-failed: {error}")),
            None => Ok(answer),
        },
        Err(error) => {
            // Dead, hung or confused: drop it (kill_on_drop) and start afresh next time.
            *guard = None;
            Err(format!(
                "vision-failed: observer {error}; restarting it on the next request"
            ))
        }
    }
}

/// The model versions the running observer reported, if one is running.
pub async fn running_versions() -> Option<Value> {
    slot().lock().await.as_ref().map(|o| o.versions.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test: the observer is process-global, and so is the variable naming it.
    #[cfg(unix)]
    #[tokio::test]
    async fn vision_observer_protocol() {
        use std::os::unix::fs::PermissionsExt as _;

        // SAFETY: test-only; nothing else in this crate's tests reads the variable.
        unsafe { std::env::set_var(OBSERVER_ENV, "/nonexistent/rsclaw-vision-observer") };
        let missing = observe(&json!({}), b"").await.err().unwrap_or_default();
        assert!(missing.starts_with("vision-unavailable:"), "{missing}");

        // A stand-in observer: ready, then one answer per request, then it exits
        // after the second (the host must start it again).
        let dir = std::env::temp_dir().join(format!("rsclaw-vision-test-{}", std::process::id()));
        assert!(std::fs::create_dir_all(&dir).is_ok());
        let script = dir.join("observer.sh");
        let body = "#!/bin/sh\necho '{\"ready\":true,\"versions\":{\"pipeline\":\"fake\"}}'\nn=0\nwhile read -r line; do\n  id=$(printf '%s' \"$line\" | sed -E 's/^\\{\"id\":([0-9]+).*/\\1/')\n  if printf '%s' \"$line\" | grep -q '\"read\":9'; then echo \"{\\\"id\\\":$id,\\\"ok\\\":false,\\\"error\\\":\\\"bad image\\\"}\"; continue; fi\n  echo \"{\\\"id\\\":$id,\\\"ok\\\":true,\\\"answer\\\":{\\\"page\\\":\\\"chat\\\"}}\"\n  n=$((n+1)); [ $n -ge 2 ] && exit 0\ndone\n";
        assert!(std::fs::write(&script, body).is_ok());
        assert!(std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).is_ok());
        unsafe { std::env::set_var(OBSERVER_ENV, &script) };

        let first = observe(&json!({"read": 0}), b"png").await;
        assert_eq!(
            first.ok().and_then(|a| a.get("page").cloned()),
            Some(json!("chat"))
        );
        assert_eq!(
            running_versions()
                .await
                .and_then(|v| v.get("pipeline").cloned()),
            Some(json!("fake"))
        );
        // the observer's own refusal fails the request but keeps it running
        let refused = observe(&json!({"read": 9}), b"png")
            .await
            .err()
            .unwrap_or_default();
        assert!(refused.starts_with("vision-failed: bad image"), "{refused}");
        assert!(observe(&json!({"read": 0}), b"png").await.is_ok()); // its second answer, then it exits
        let dead = observe(&json!({"read": 0}), b"png")
            .await
            .err()
            .unwrap_or_default();
        assert!(dead.starts_with("vision-failed:"), "{dead}");
        // started again on the next request
        assert!(observe(&json!({"read": 0}), b"png").await.is_ok());

        unsafe { std::env::remove_var(OBSERVER_ENV) };
        assert!(std::fs::remove_dir_all(&dir).is_ok());
    }
}
