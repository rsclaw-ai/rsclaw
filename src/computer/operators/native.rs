//! Native desktop operator backed by `enigo` (input synthesis) and
//! `xcap` (screenshot). Cross-platform: macOS / Windows / Linux X11 +
//! Wayland (xcap handles portal under Wayland).
//!
//! Behaviour notes:
//!   - `enigo::Enigo` is not `Send` on all platforms (Windows in
//!     particular) and input synthesis is fundamentally blocking, so
//!     every action runs inside `tokio::task::spawn_blocking` with a
//!     fresh `Enigo` per call. The operator itself is a unit struct
//!     that is trivially `Send + Sync`.
//!   - Coordinates from the model are in **physical** pixels (raw
//!     screenshot space). On macOS Retina, `enigo` expects logical
//!     coordinates, so we divide by `ctx.scale_factor`. On Windows /
//!     Linux X11 the API takes physical pixels and we pass through
//!     unchanged.
//!   - `activate_app` shells out per platform to bring **all** windows
//!     of the target app to the front. The objc2-app-kit /
//!     EnumWindows / xdotool routes are listed as TODOs for a v2 that
//!     wants zero shell-out overhead.

use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use enigo::{
    Axis, Button, Coordinate,
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use image::{ImageFormat, RgbaImage};
use tracing::{debug, warn};
use xcap::Monitor;

use crate::computer::action::{
    Action, ActionSpec, ExecCtx, MouseButton, Screenshot, ScrollDir,
};
use crate::computer::operator::{ActionFut, ActionOutput, Operator, ScreenshotFut};

/// Native desktop operator. Unit struct: holds no state because every
/// platform call needs a fresh `Enigo` on the calling OS thread anyway
/// (Windows `Enigo` is not `Send`).
pub struct NativeOperator;

impl NativeOperator {
    /// Construct a new native operator. Cheap — no state is captured.
    pub fn new() -> Self {
        Self
    }
}

impl Default for NativeOperator {
    fn default() -> Self {
        Self::new()
    }
}

impl Operator for NativeOperator {
    fn name(&self) -> &'static str {
        "native"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        // Mirrors the UI-TARS-desktop NutJSOperator action space —
        // matches what the GUI VLM training data expects.
        vec![
            ActionSpec::new("click(start_box='<|box_start|>(x1,y1)<|box_end|>')"),
            ActionSpec::new("left_double(start_box='<|box_start|>(x1,y1)<|box_end|>')"),
            ActionSpec::new("right_single(start_box='<|box_start|>(x1,y1)<|box_end|>')"),
            ActionSpec::new(
                "drag(start_box='<|box_start|>(x1,y1)<|box_end|>', end_box='<|box_start|>(x3,y3)<|box_end|>')",
            ),
            ActionSpec::with_note(
                "hotkey(key='')",
                "# Lowercase, space-separated, max 4 keys",
            ),
            ActionSpec::with_note(
                "type(content='')",
                "# Add \\n at end of content to submit",
            ),
            ActionSpec::new(
                "scroll(start_box='<|box_start|>(x1,y1)<|box_end|>', direction='down or up or right or left')",
            ),
            ActionSpec::with_note(
                "wait()",
                "# Default sleep 1s and re-screenshot. Pass wait(seconds=5) for slow page loads (max 60).",
            ),
            ActionSpec::with_note(
                "activate_app(app='AppName')",
                "# Bring an app forward when it's not visible. Use BEFORE clicking when the target app isn't on screen.",
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
            tokio::task::spawn_blocking(capture_primary_screen)
                .await
                .context("screenshot blocking task join failed")?
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            // Clone what we need into the blocking task (Action is
            // Clone, ExecCtx is Copy).
            let action = action.clone();
            let ctx = *ctx;

            // Wait + Finished + CallUser are pure tokio — no enigo,
            // no spawn_blocking. Handle them up front.
            match &action {
                Action::Wait { seconds } => {
                    let s = seconds.clamp(0.0, 60.0);
                    tokio::time::sleep(Duration::from_secs_f32(s)).await;
                    return Ok(ActionOutput::ok());
                }
                Action::Finished { content } => {
                    debug!(content = %content, "native operator: finished");
                    return Ok(ActionOutput::ok());
                }
                Action::CallUser { reason } => {
                    debug!(reason = %reason, "native operator: call_user");
                    return Ok(ActionOutput::ok());
                }
                Action::ActivateApp { app } => {
                    return activate_app(app).await;
                }
                _ => {}
            }

            tokio::task::spawn_blocking(move || execute_blocking(&action, &ctx))
                .await
                .context("execute blocking task join failed")?
        })
    }
}

// ---------------------------------------------------------------------------
// Screenshot
// ---------------------------------------------------------------------------

/// Capture the primary monitor and encode as PNG.
fn capture_primary_screen() -> Result<Screenshot> {
    let monitors =
        Monitor::all().map_err(|e| anyhow!("xcap::Monitor::all failed: {e}"))?;
    if monitors.is_empty() {
        return Err(anyhow!("no monitors detected"));
    }

    // Prefer the primary; fall back to the first if `is_primary` errors
    // (xcap occasionally returns errors on fresh sessions).
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .unwrap_or(&monitors[0])
        .clone();

    let scale_factor = monitor.scale_factor().unwrap_or(1.0);
    let monitor_w = monitor.width().unwrap_or(0);
    let monitor_h = monitor.height().unwrap_or(0);

    let img: RgbaImage = monitor
        .capture_image()
        .map_err(|e| anyhow!("xcap capture_image failed: {e}"))?;

    let physical_w = img.width();
    let physical_h = img.height();

    // xcap's `width`/`height` reports the physical pixel size on macOS
    // Retina, so logical = physical / scale_factor. If the monitor
    // metadata disagrees with the captured image (rare), trust the
    // image.
    let _ = (monitor_w, monitor_h);

    let logical_w = if scale_factor > 0.0 {
        (physical_w as f32 / scale_factor).round() as u32
    } else {
        physical_w
    };
    let logical_h = if scale_factor > 0.0 {
        (physical_h as f32 / scale_factor).round() as u32
    } else {
        physical_h
    };

    // Encode RGBA -> PNG via the `image` crate.
    let mut png_bytes: Vec<u8> = Vec::with_capacity(physical_w as usize * physical_h as usize / 4);
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, ImageFormat::Png)
            .context("PNG encode failed")?;
    }

    Ok(Screenshot {
        png_bytes,
        logical_size: (logical_w, logical_h),
        physical_size: (physical_w, physical_h),
        scale_factor,
    })
}

// ---------------------------------------------------------------------------
// Action dispatch
// ---------------------------------------------------------------------------

/// Execute a non-async action on the calling thread with a fresh
/// `Enigo`. Any input error is reported back via `ActionOutput::err`
/// so the driver can feed it into the next turn rather than blowing
/// up the whole loop.
fn execute_blocking(action: &Action, ctx: &ExecCtx) -> Result<ActionOutput> {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            // The most common Enigo init failure on macOS is a missing
            // Accessibility / Input-Monitoring grant — the message
            // returned by enigo itself ("the application does not have
            // the permission to simulate input") is correct but doesn't
            // tell the user where to fix it. Surface a one-shot
            // actionable hint here so the operator-error gets surfaced
            // to the agent loop with concrete remediation steps.
            let err_str = e.to_string();
            let hint = if cfg!(target_os = "macos")
                && (err_str.contains("permission")
                    || err_str.contains("simulate input"))
            {
                " (macOS: open System Settings -> Privacy & Security -> \
                 Accessibility AND Input Monitoring, then add the \
                 currently-running rsclaw binary at \
                 target/debug/rsclaw or target/release/rsclaw. \
                 Restart the gateway after granting.)"
            } else {
                ""
            };
            warn!(error = %e, hint, "failed to construct Enigo");
            return Ok(ActionOutput::err(format!(
                "enigo init failed: {e}{hint}"
            )));
        }
    };

    match action {
        Action::MouseMove { x, y } => {
            let (lx, ly) = scale_for_input(*x, *y, ctx.scale_factor);
            try_input(enigo.move_mouse(lx, ly, Coordinate::Abs), "move_mouse")
        }
        Action::Click { x, y, button } => {
            let (lx, ly) = scale_for_input(*x, *y, ctx.scale_factor);
            if let Err(msg) = ok_or_msg(
                enigo.move_mouse(lx, ly, Coordinate::Abs),
                "move_mouse",
            ) {
                return Ok(ActionOutput::err(msg));
            }
            let btn = map_button(*button);
            try_input(enigo.button(btn, Click), "button")
        }
        Action::DoubleClick { x, y } => {
            let (lx, ly) = scale_for_input(*x, *y, ctx.scale_factor);
            if let Err(msg) = ok_or_msg(
                enigo.move_mouse(lx, ly, Coordinate::Abs),
                "move_mouse",
            ) {
                return Ok(ActionOutput::err(msg));
            }
            if let Err(msg) = ok_or_msg(enigo.button(Button::Left, Click), "button") {
                return Ok(ActionOutput::err(msg));
            }
            try_input(enigo.button(Button::Left, Click), "button")
        }
        Action::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            let (fx, fy) = scale_for_input(*from_x, *from_y, ctx.scale_factor);
            let (tx, ty) = scale_for_input(*to_x, *to_y, ctx.scale_factor);
            if let Err(msg) = ok_or_msg(
                enigo.move_mouse(fx, fy, Coordinate::Abs),
                "move_mouse",
            ) {
                return Ok(ActionOutput::err(msg));
            }
            if let Err(msg) = ok_or_msg(enigo.button(Button::Left, Press), "button press") {
                return Ok(ActionOutput::err(msg));
            }
            if let Err(msg) = ok_or_msg(
                enigo.move_mouse(tx, ty, Coordinate::Abs),
                "drag move",
            ) {
                // Best-effort release before bailing.
                let _ = enigo.button(Button::Left, Release);
                return Ok(ActionOutput::err(msg));
            }
            try_input(enigo.button(Button::Left, Release), "button release")
        }
        Action::Scroll {
            x,
            y,
            direction,
            clicks,
        } => {
            let (lx, ly) = scale_for_input(*x, *y, ctx.scale_factor);
            // Move first so the scroll lands at the requested point.
            if let Err(msg) = ok_or_msg(
                enigo.move_mouse(lx, ly, Coordinate::Abs),
                "move_mouse",
            ) {
                return Ok(ActionOutput::err(msg));
            }
            let (axis, length) = scroll_amount(*direction, *clicks);
            try_input(enigo.scroll(length, axis), "scroll")
        }
        Action::Type { text } => type_text(&mut enigo, text),
        Action::Hotkey { keys } => press_hotkey(&mut enigo, keys),
        Action::HoldKey { key, seconds } => {
            let Some(k) = parse_key(key) else {
                return Ok(ActionOutput::err(format!("unknown key: {key}")));
            };
            let s = seconds.clamp(0.0, 60.0);
            if let Err(msg) = ok_or_msg(enigo.key(k, Press), "key press") {
                return Ok(ActionOutput::err(msg));
            }
            std::thread::sleep(Duration::from_secs_f32(s));
            try_input(enigo.key(k, Release), "key release")
        }
        Action::Screenshot => {
            // Mainly a no-op gate in some retry patterns. Capture and
            // discard so call sites that rely on the side effect of a
            // fresh frame still get one.
            let _ = capture_primary_screen()?;
            Ok(ActionOutput::ok())
        }
        // The async variants are handled before we reach this function.
        Action::Wait { .. }
        | Action::Finished { .. }
        | Action::CallUser { .. }
        | Action::ActivateApp { .. } => Ok(ActionOutput::ok()),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map physical-pixel coordinates from the screenshot into the
/// coordinate space `enigo` expects on this platform.
///
/// On macOS Retina, `enigo` takes logical pixels (physical / scale).
/// On Windows and Linux X11 the input APIs are physical. We branch at
/// runtime via `cfg!` so a single source file covers all platforms.
fn scale_for_input(x: i32, y: i32, scale_factor: f32) -> (i32, i32) {
    if cfg!(target_os = "macos") && scale_factor > 0.0 && (scale_factor - 1.0).abs() > f32::EPSILON {
        let lx = (x as f32 / scale_factor).round() as i32;
        let ly = (y as f32 / scale_factor).round() as i32;
        (lx, ly)
    } else {
        (x, y)
    }
}

/// Map our `MouseButton` to enigo's `Button`.
fn map_button(b: MouseButton) -> Button {
    match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// Translate a scroll direction + click count into enigo's `(length, axis)`.
fn scroll_amount(dir: ScrollDir, clicks: i32) -> (Axis, i32) {
    match dir {
        ScrollDir::Up => (Axis::Vertical, -clicks),
        ScrollDir::Down => (Axis::Vertical, clicks),
        ScrollDir::Left => (Axis::Horizontal, -clicks),
        ScrollDir::Right => (Axis::Horizontal, clicks),
    }
}

/// Type literal text. If the text ends with `\n`, strip the newline
/// and submit via `Return` to mirror NutJSOperator semantics.
fn type_text(enigo: &mut Enigo, text: &str) -> Result<ActionOutput> {
    let (body, submit) = if let Some(stripped) = text.strip_suffix('\n') {
        (stripped, true)
    } else {
        (text, false)
    };

    if !body.is_empty() {
        if let Err(msg) = ok_or_msg(enigo.text(body), "type text") {
            return Ok(ActionOutput::err(msg));
        }
    }

    if submit {
        return try_input(enigo.key(Key::Return, Click), "submit return");
    }
    Ok(ActionOutput::ok())
}

/// Parse and execute a hotkey combination.
///
/// Tokens are separated by whitespace or `+`, lowercased before
/// lookup. The combo is press-all-then-release-in-reverse so chord
/// semantics work (e.g. `cmd shift t` re-opens the last tab).
/// Capped at 4 keys to keep the LLM honest — anything longer is
/// almost certainly a parse error or hallucination.
fn press_hotkey(enigo: &mut Enigo, keys: &str) -> Result<ActionOutput> {
    let tokens: Vec<&str> = keys
        .split(|c: char| c.is_whitespace() || c == '+')
        .filter(|s| !s.is_empty())
        .collect();

    if tokens.is_empty() {
        return Ok(ActionOutput::err("empty hotkey"));
    }
    if tokens.len() > 4 {
        return Ok(ActionOutput::err(format!(
            "hotkey too long ({} keys, max 4)",
            tokens.len()
        )));
    }

    let mut parsed: Vec<Key> = Vec::with_capacity(tokens.len());
    for tok in &tokens {
        match parse_key(tok) {
            Some(k) => parsed.push(k),
            None => return Ok(ActionOutput::err(format!("unknown key: {tok}"))),
        }
    }

    // Press in order.
    let mut pressed: Vec<Key> = Vec::with_capacity(parsed.len());
    for k in &parsed {
        if let Err(msg) = ok_or_msg(enigo.key(*k, Press), "hotkey press") {
            // Release whatever we managed to press.
            for held in pressed.iter().rev() {
                let _ = enigo.key(*held, Release);
            }
            return Ok(ActionOutput::err(msg));
        }
        pressed.push(*k);
    }

    // Release in reverse.
    let mut last_err: Option<String> = None;
    for k in parsed.iter().rev() {
        if let Err(msg) = ok_or_msg(enigo.key(*k, Release), "hotkey release") {
            last_err = Some(msg);
        }
    }
    match last_err {
        Some(msg) => Ok(ActionOutput::err(msg)),
        None => Ok(ActionOutput::ok()),
    }
}

/// Map a single key token (already lowercased after split) to an
/// `enigo::Key`. Returns `None` for unrecognised tokens so the caller
/// can surface a useful error instead of silently dropping the input.
fn parse_key(raw: &str) -> Option<Key> {
    let key = raw.trim().to_lowercase();
    let k = match key.as_str() {
        "return" | "enter" => Key::Return,
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "cmd" | "command" | "meta" | "win" | "super" => Key::Meta,
        "tab" => Key::Tab,
        "escape" | "esc" => Key::Escape,
        "space" | "spacebar" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "up" | "arrowup" | "uparrow" => Key::UpArrow,
        "down" | "arrowdown" | "downarrow" => Key::DownArrow,
        "left" | "arrowleft" | "leftarrow" => Key::LeftArrow,
        "right" | "arrowright" | "rightarrow" => Key::RightArrow,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "home" => Key::Home,
        "end" => Key::End,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        // Single-char fallback (a-z, 0-9, punctuation).
        s if s.chars().count() == 1 => Key::Unicode(s.chars().next()?),
        _ => return None,
    };
    Some(k)
}

/// Wrap an enigo `InputResult` into a simple `Result<(), String>`.
fn ok_or_msg<E: std::fmt::Display>(
    res: std::result::Result<(), E>,
    op: &'static str,
) -> std::result::Result<(), String> {
    res.map_err(|e| format!("{op}: {e}"))
}

/// Bridge `enigo` errors into `ActionOutput` so the driver can keep
/// going on a single failed action.
fn try_input<E: std::fmt::Display>(
    res: std::result::Result<(), E>,
    op: &'static str,
) -> Result<ActionOutput> {
    match res {
        Ok(()) => Ok(ActionOutput::ok()),
        Err(e) => {
            warn!(operation = op, error = %e, "enigo input error");
            Ok(ActionOutput::err(format!("{op}: {e}")))
        }
    }
}

// ---------------------------------------------------------------------------
// activate_app — platform-specific shell-out
// ---------------------------------------------------------------------------

/// Bring **all** windows of the named application to the front.
///
/// macOS uses `osascript`. Windows uses PowerShell (EnumWindows-via-
/// process). Linux uses `wmctrl` first, then `xdotool` as a fallback.
/// On unsupported platforms returns `ActionOutput::err` instead of
/// panicking.
async fn activate_app(app: &str) -> Result<ActionOutput> {
    let app = app.to_owned();
    let res = tokio::task::spawn_blocking(move || activate_app_blocking(&app))
        .await
        .context("activate_app join failed")?;
    Ok(res)
}

fn activate_app_blocking(app: &str) -> ActionOutput {
    if cfg!(target_os = "macos") {
        // TODO(v2): swap osascript for objc2-app-kit
        // `NSRunningApplication.activate(options: .activateAllWindows)` to
        // remove the spawn-osascript latency (~50-100ms per call).
        let script = format!(r#"tell application "{}" to activate"#, app.replace('"', r#"\""#));
        match Command::new("osascript").arg("-e").arg(&script).output() {
            Ok(out) if out.status.success() => ActionOutput::ok(),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                ActionOutput::err(format!("osascript failed: {stderr}"))
            }
            Err(e) => ActionOutput::err(format!("osascript spawn failed: {e}")),
        }
    } else if cfg!(target_os = "windows") {
        // PowerShell pipeline: enumerate every process whose name
        // matches and hand its main window to SetForegroundWindow.
        // TODO(v2): replace with `windows` crate EnumWindows + per-PID
        // SetForegroundWindow to bring secondary windows too.
        let escaped = app.replace('\'', "''");
        let ps = format!(
            r#"Add-Type -Name W -Namespace N -MemberDefinition '[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);'; Get-Process | Where-Object {{$_.ProcessName -like '*{}*'}} | ForEach-Object {{ if ($_.MainWindowHandle -ne 0) {{ [N.W]::SetForegroundWindow($_.MainWindowHandle) }} }}"#,
            escaped
        );
        match Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
        {
            Ok(out) if out.status.success() => ActionOutput::ok(),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                ActionOutput::err(format!("powershell failed: {stderr}"))
            }
            Err(e) => ActionOutput::err(format!("powershell spawn failed: {e}")),
        }
    } else if cfg!(target_os = "linux") {
        // Try wmctrl first (matches by visible window title), then
        // xdotool by class. Either being absent is a soft error.
        let wmctrl = Command::new("wmctrl").args(["-a", &app]).status();
        if matches!(&wmctrl, Ok(s) if s.success()) {
            return ActionOutput::ok();
        }
        let xdotool = Command::new("xdotool")
            .args(["search", "--class", &app, "windowactivate"])
            .status();
        match xdotool {
            Ok(s) if s.success() => ActionOutput::ok(),
            Ok(s) => ActionOutput::err(format!("xdotool exit status: {s}")),
            Err(e) => ActionOutput::err(format!(
                "neither wmctrl nor xdotool worked: {e}"
            )),
        }
    } else {
        ActionOutput::err("activate_app: unsupported platform")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_constructs() {
        let _op = NativeOperator::default();
        let _op2 = NativeOperator::new();
    }

    #[test]
    fn name_is_native() {
        let op = NativeOperator::new();
        assert_eq!(op.name(), "native");
    }

    #[test]
    fn action_spaces_advertise_core_capabilities() {
        let op = NativeOperator::new();
        let specs = op.action_spaces();
        assert!(
            specs.len() >= 9,
            "expected at least 9 action specs, got {}",
            specs.len()
        );
        let sigs: Vec<&str> = specs.iter().map(|s| s.signature).collect();
        for needle in ["click", "type", "hotkey", "scroll", "wait", "finished"] {
            assert!(
                sigs.iter().any(|s| s.contains(needle)),
                "action_spaces missing `{needle}`: {:?}",
                sigs
            );
        }
    }

    #[test]
    fn parse_key_handles_modifiers_and_chars() {
        assert!(matches!(parse_key("cmd"), Some(Key::Meta)));
        assert!(matches!(parse_key("CTRL"), Some(Key::Control)));
        assert!(matches!(parse_key("return"), Some(Key::Return)));
        assert!(matches!(parse_key("a"), Some(Key::Unicode('a'))));
        assert!(parse_key("not-a-real-key").is_none());
    }

    #[test]
    fn scroll_amount_signs() {
        assert_eq!(scroll_amount(ScrollDir::Up, 3), (Axis::Vertical, -3));
        assert_eq!(scroll_amount(ScrollDir::Down, 3), (Axis::Vertical, 3));
        assert_eq!(scroll_amount(ScrollDir::Left, 2), (Axis::Horizontal, -2));
        assert_eq!(scroll_amount(ScrollDir::Right, 2), (Axis::Horizontal, 2));
    }
}
