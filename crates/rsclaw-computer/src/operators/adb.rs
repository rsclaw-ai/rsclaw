//! Android operator backed by the `adb` CLI.
//!
//! ADB is the lowest-friction option — it's already installed on most
//! Android dev machines and lives outside any sandbox we'd otherwise
//! have to deal with. Action mapping mirrors UI-TARS-desktop's adb
//! operator:
//!
//!   tap         → `adb shell input tap x y`
//!   long_press  → `adb shell input swipe x y x y 1000`
//!   swipe       → `adb shell input swipe x1 y1 x2 y2 [duration_ms]`
//!   type        → `adb shell input text "..."` (spaces escaped as %s)
//!   press_home  → `adb shell input keyevent KEYCODE_HOME`
//!   press_back  → `adb shell input keyevent KEYCODE_BACK`
//!   open_app    → `adb shell monkey -p <pkg> -c android.intent.category.LAUNCHER 1`
//!   screenshot  → `adb exec-out screencap -p` (binary PNG over stdout)
//!
//! Multi-device support: a `serial` field can be set; passes
//! `-s <serial>` to every adb invocation. Otherwise adb defaults to
//! "the only attached device" or fails when there are zero/multiple.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use tokio::io::AsyncReadExt;
use tracing::{debug, warn};

use crate::computer::action::{
    Action, ActionSpec, ExecCtx, MouseButton, Screenshot, ScrollDir,
};
use crate::computer::operator::{ActionFut, ActionOutput, Operator, ScreenshotFut};

/// Android operator. Constructed with an optional device serial — when
/// `None` adb picks the only attached device, otherwise we pass
/// `-s <serial>` to every invocation.
pub struct AdbOperator {
    serial: Option<String>,
}

impl AdbOperator {
    /// Build an operator that targets whatever `adb devices` resolves
    /// to (errors out at execution time if 0 or 2+ devices).
    pub fn new() -> Self {
        Self { serial: None }
    }

    /// Target a specific device. Useful when multiple phones are
    /// plugged in — get the serial from `adb devices`.
    pub fn with_serial(serial: impl Into<String>) -> Self {
        Self {
            serial: Some(serial.into()),
        }
    }

    /// Build the base `adb [-s SERIAL] ...` argv for a subcommand.
    fn argv<'a>(&'a self, sub: &'a [&'a str]) -> Vec<&'a str> {
        let mut out = Vec::with_capacity(sub.len() + 2);
        if let Some(s) = self.serial.as_deref() {
            out.push("-s");
            out.push(s);
        }
        out.extend_from_slice(sub);
        out
    }
}

impl Default for AdbOperator {
    fn default() -> Self {
        Self::new()
    }
}

impl Operator for AdbOperator {
    fn name(&self) -> &'static str {
        "adb"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec::new("tap(start_box='<box>x1,y1</box>')"),
            ActionSpec::with_note(
                "long_press(start_box='<box>x1,y1</box>')",
                "# Touch and hold ~1s",
            ),
            ActionSpec::new(
                "swipe(start_box='<box>x1,y1</box>', end_box='<box>x3,y3</box>')",
            ),
            ActionSpec::with_note(
                "type(content='')",
                "# Add \\n at end to submit",
            ),
            ActionSpec::with_note(
                "press_home()",
                "# Hardware HOME key",
            ),
            ActionSpec::with_note(
                "press_back()",
                "# Hardware BACK key",
            ),
            ActionSpec::with_note(
                "open_app(app_name='com.tencent.mm')",
                "# Launch by package name",
            ),
            ActionSpec::with_note(
                "wait()",
                "# Default sleep 1s. Pass wait(seconds=5) for slow loads (max 60).",
            ),
            ActionSpec::new("finished(content='xxx')"),
            ActionSpec::with_note(
                "call_user()",
                "# Submit and call user when stuck or need help",
            ),
        ]
    }

    fn screenshot(&self) -> ScreenshotFut<'_> {
        Box::pin(async move {
            let args = self.argv(&["exec-out", "screencap", "-p"]);
            let mut child = tokio::process::Command::new("adb")
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("adb screencap spawn failed (is `adb` in PATH?)")?;

            let mut png_bytes = Vec::with_capacity(2 * 1024 * 1024);
            if let Some(mut stdout) = child.stdout.take() {
                stdout
                    .read_to_end(&mut png_bytes)
                    .await
                    .context("read adb screencap output")?;
            }
            let status = child.wait().await.context("wait adb screencap")?;
            if !status.success() {
                return Err(anyhow!("adb screencap failed (exit {status})"));
            }
            if png_bytes.len() < 24 {
                return Err(anyhow!("adb screencap returned empty / truncated PNG"));
            }

            // Parse width/height from PNG IHDR (bytes 16..24, big-endian).
            let w = u32::from_be_bytes([
                png_bytes[16],
                png_bytes[17],
                png_bytes[18],
                png_bytes[19],
            ]);
            let h = u32::from_be_bytes([
                png_bytes[20],
                png_bytes[21],
                png_bytes[22],
                png_bytes[23],
            ]);

            Ok(Screenshot {
                png_bytes,
                logical_size: (w, h),
                physical_size: (w, h),
                scale_factor: 1.0,
            })
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            let action = action.clone();
            let ctx = *ctx;
            let _ = ctx; // ADB takes raw device coords; no scaling needed.

            match &action {
                Action::Wait { seconds } => {
                    let s = seconds.clamp(0.0, 60.0);
                    tokio::time::sleep(Duration::from_secs_f32(s)).await;
                    Ok(ActionOutput::ok())
                }
                Action::Finished { content } => {
                    debug!(content = %content, "adb: finished");
                    Ok(ActionOutput::ok())
                }
                Action::CallUser { reason } => {
                    debug!(reason = %reason, "adb: call_user");
                    Ok(ActionOutput::ok())
                }
                Action::ActivateApp { app } => {
                    let args = self.argv(&[
                        "shell",
                        "monkey",
                        "-p",
                        app,
                        "-c",
                        "android.intent.category.LAUNCHER",
                        "1",
                    ]);
                    run_adb(&args).await
                }
                Action::Click { x, y, button } => {
                    if !matches!(button, MouseButton::Left) {
                        warn!(?button, "adb: ignoring non-left button");
                    }
                    let xs = x.to_string();
                    let ys = y.to_string();
                    let args = self.argv(&["shell", "input", "tap", &xs, &ys]);
                    run_adb(&args).await
                }
                Action::DoubleClick { x, y } => {
                    let xs = x.to_string();
                    let ys = y.to_string();
                    let a1 = self.argv(&["shell", "input", "tap", &xs, &ys]);
                    let _ = run_adb(&a1).await?;
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let a2 = self.argv(&["shell", "input", "tap", &xs, &ys]);
                    run_adb(&a2).await
                }
                Action::MouseMove { .. } => {
                    // Android has no cursor — bare moves are no-ops.
                    Ok(ActionOutput::ok())
                }
                Action::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => {
                    let xs1 = from_x.to_string();
                    let ys1 = from_y.to_string();
                    let xs2 = to_x.to_string();
                    let ys2 = to_y.to_string();
                    // 300ms gives the gesture engine enough samples
                    // to recognise it as a swipe rather than a long
                    // press at the start point.
                    let args = self.argv(&[
                        "shell", "input", "swipe", &xs1, &ys1, &xs2, &ys2, "300",
                    ]);
                    run_adb(&args).await
                }
                Action::Scroll {
                    x,
                    y,
                    direction,
                    clicks,
                } => {
                    // Translate scroll to a swipe of (clicks * 200) px.
                    let dist = clicks.abs().max(1) * 200;
                    let (dx, dy) = match direction {
                        ScrollDir::Up => (0, -dist),
                        ScrollDir::Down => (0, dist),
                        ScrollDir::Left => (-dist, 0),
                        ScrollDir::Right => (dist, 0),
                    };
                    let xs1 = x.to_string();
                    let ys1 = y.to_string();
                    let xs2 = (x + dx).to_string();
                    let ys2 = (y + dy).to_string();
                    let args = self.argv(&[
                        "shell", "input", "swipe", &xs1, &ys1, &xs2, &ys2, "200",
                    ]);
                    run_adb(&args).await
                }
                Action::Type { text } => {
                    // adb's `input text` interprets spaces literally
                    // ONLY if you escape them as %s. Submit on trailing \n.
                    let stripped = text.trim_end_matches('\n').trim_end_matches("\\n");
                    if !stripped.is_empty() {
                        // adb's `shell input text` runs through the device-side
                        // shell — characters like ;, &, |, >, <, $, `, \ would
                        // break out of the `input text <arg>` slot and execute
                        // arbitrary commands as the shell user. VLMs hallucinate
                        // these (and prompt-injected web pages can deliberately
                        // smuggle them via `type(content='hello; rm -rf /sdcard')`).
                        // Reject loudly instead of escaping — escaping is fragile
                        // across adb backends and the failure surfaces back to
                        // the VLM so it can retry with a sanitized string.
                        const SHELL_META: &[char] =
                            &[';', '&', '|', '>', '<', '$', '`', '\\', '"', '\''];
                        if let Some(bad) = stripped.chars().find(|c| SHELL_META.contains(c)) {
                            return Ok(ActionOutput::err(format!(
                                "adb type: refusing content with shell metachar '{bad}' \
                                 (would inject into device shell); strip and retry"
                            )));
                        }
                        let escaped = stripped.replace(' ', "%s");
                        let args = self.argv(&["shell", "input", "text", &escaped]);
                        let _ = run_adb(&args).await?;
                    }
                    if text.ends_with('\n') || text.ends_with("\\n") {
                        let args = self.argv(&[
                            "shell",
                            "input",
                            "keyevent",
                            "KEYCODE_ENTER",
                        ]);
                        run_adb(&args).await
                    } else {
                        Ok(ActionOutput::ok())
                    }
                }
                Action::Hotkey { keys } => {
                    // Map the special "press_home" / "press_back"
                    // synthetic hotkeys to keyevents. Anything else is
                    // treated as a single keycode after uppercasing.
                    let kc = match keys.trim().to_lowercase().as_str() {
                        "press_home" | "home" => "KEYCODE_HOME",
                        "press_back" | "back" => "KEYCODE_BACK",
                        "menu" => "KEYCODE_MENU",
                        "power" => "KEYCODE_POWER",
                        "enter" | "return" => "KEYCODE_ENTER",
                        "tab" => "KEYCODE_TAB",
                        "space" => "KEYCODE_SPACE",
                        other => {
                            warn!(keys = %other, "adb: unmapped hotkey");
                            return Ok(ActionOutput::err(format!(
                                "adb: hotkey '{other}' not mapped — use press_home / press_back / menu / power / enter / tab / space"
                            )));
                        }
                    };
                    let args = self.argv(&["shell", "input", "keyevent", kc]);
                    run_adb(&args).await
                }
                Action::HoldKey { key, seconds } => {
                    let _ = (key, seconds);
                    Ok(ActionOutput::err(
                        "adb: hold_key not supported (use long_press swipe instead)",
                    ))
                }
                Action::Screenshot => {
                    // Just trigger screenshot via screencap; result is
                    // discarded by the driver anyway (separate path).
                    Ok(ActionOutput::ok())
                }
            }
        })
    }
}

/// Run an `adb …` command, returning `ActionOutput::err` (not Err) on
/// non-zero exit so the driver can feed the failure into the next turn.
async fn run_adb(args: &[&str]) -> Result<ActionOutput> {
    let out = tokio::process::Command::new("adb")
        .args(args)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Ok(ActionOutput::ok()),
        Ok(o) => Ok(ActionOutput::err(format!(
            "adb exit {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(e) => Ok(ActionOutput::err(format!(
            "adb spawn failed: {e} (is `adb` in PATH?)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_adb() {
        assert_eq!(AdbOperator::new().name(), "adb");
    }

    #[test]
    fn action_spaces_includes_android_specific() {
        let specs = AdbOperator::new().action_spaces();
        let joined: String = specs
            .iter()
            .map(|s| s.render())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("tap("));
        assert!(joined.contains("swipe("));
        assert!(joined.contains("press_home"));
        assert!(joined.contains("press_back"));
        assert!(joined.contains("open_app("));
        assert!(!joined.contains("hotkey"), "Android shouldn't advertise raw hotkey");
    }

    #[test]
    fn argv_with_serial() {
        let op = AdbOperator::with_serial("emulator-5554");
        let v = op.argv(&["shell", "input", "tap", "100", "200"]);
        assert_eq!(v, vec!["-s", "emulator-5554", "shell", "input", "tap", "100", "200"]);
    }

    #[test]
    fn argv_without_serial() {
        let op = AdbOperator::new();
        let v = op.argv(&["shell", "ls"]);
        assert_eq!(v, vec!["shell", "ls"]);
    }
}
