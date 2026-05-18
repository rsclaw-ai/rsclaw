//! iPhone Mirroring operator — control an iPhone via the macOS
//! Sequoia "iPhone Mirroring" app.
//!
//! Different from `NativeOperator` in three ways:
//!
//!   1. Screenshot captures **only** the iPhone Mirroring window, not
//!      the full Mac desktop. The model sees just the iPhone screen.
//!   2. Coordinates from the model are in the iPhone's screen space
//!      (e.g. 390×844 for iPhone 15). At execute time we translate to
//!      absolute Mac screen coordinates by adding the window position.
//!   3. The action space is iOS-flavoured — `tap` / `long_press` /
//!      `swipe` / `press_home` / `press_back` / `type` instead of
//!      desktop-style click/drag/hotkey.
//!
//! Input synthesis still goes through `enigo` (the iPhone Mirroring
//! window is just a macOS window from the OS's perspective). This
//! operator is **macOS-only** — on other platforms `screenshot` and
//! `execute` return errors so callers can surface a "iPhone Mirroring
//! is macOS Sequoia only" message rather than panicking.

use std::time::Duration;

use anyhow::{anyhow, Context as _, Result};
use enigo::{
    Axis, Button, Coordinate,
    Direction::{Click, Press, Release},
    Enigo, Key, Keyboard, Mouse, Settings,
};
use image::{ImageFormat, RgbaImage};
use tracing::{debug, warn};

use crate::computer::action::{
    Action, ActionSpec, ExecCtx, MouseButton, Screenshot, ScrollDir,
};
use crate::computer::operator::{ActionFut, ActionOutput, Operator, ScreenshotFut};

/// macOS-only operator that drives iPhone Mirroring. Stateless — every
/// call resolves the iPhone Mirroring window fresh because the user
/// can move / hide / re-launch it between calls.
pub struct IphoneMirrorOperator;

impl IphoneMirrorOperator {
    pub fn new() -> Self {
        Self
    }
}

impl Default for IphoneMirrorOperator {
    fn default() -> Self {
        Self::new()
    }
}

/// Display name of the iPhone Mirroring app's main window. Stable
/// since macOS 15 Sequoia. Adjust here if Apple renames it.
const WINDOW_TITLE_PREFIX: &str = "iPhone Mirroring";
/// Bundle id used by `osascript` activation.
const APP_BUNDLE_NAME: &str = "iPhone Mirroring";

impl Operator for IphoneMirrorOperator {
    fn name(&self) -> &'static str {
        "iphone_mirror"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec::new("tap(start_box='<box>x1,y1</box>')"),
            ActionSpec::with_note(
                "long_press(start_box='<box>x1,y1</box>')",
                "# Press and hold ~1s",
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
                "# Equivalent to swiping up from the bottom edge",
            ),
            ActionSpec::with_note(
                "press_back()",
                "# Equivalent to swiping right from the left edge",
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
            #[cfg(not(target_os = "macos"))]
            {
                anyhow::bail!(
                    "IphoneMirrorOperator: iPhone Mirroring is macOS-only \
                     (Sequoia 15+ required)"
                );
            }
            #[cfg(target_os = "macos")]
            {
                tokio::task::spawn_blocking(capture_iphone_window)
                    .await
                    .context("iphone_mirror screenshot blocking task failed")?
            }
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (action, ctx);
                return Ok(ActionOutput::err(
                    "iPhone Mirroring is macOS-only (Sequoia 15+ required)",
                ));
            }
            #[cfg(target_os = "macos")]
            {
                let action = action.clone();
                let ctx = *ctx;

                // Tokio-only paths: don't need enigo / window lookup.
                match &action {
                    Action::Wait { seconds } => {
                        let s = seconds.clamp(0.0, 60.0);
                        tokio::time::sleep(Duration::from_secs_f32(s)).await;
                        return Ok(ActionOutput::ok());
                    }
                    Action::Finished { content } => {
                        debug!(content = %content, "iphone_mirror: finished");
                        return Ok(ActionOutput::ok());
                    }
                    Action::CallUser { reason } => {
                        debug!(reason = %reason, "iphone_mirror: call_user");
                        return Ok(ActionOutput::ok());
                    }
                    Action::ActivateApp { .. } => {
                        // The whole point of this operator IS the iPhone
                        // Mirroring app, so honour any activate by
                        // bringing iPhone Mirroring to the front.
                        return activate_iphone_mirroring().await;
                    }
                    _ => {}
                }

                tokio::task::spawn_blocking(move || execute_blocking(&action, &ctx))
                    .await
                    .context("iphone_mirror execute blocking task failed")?
            }
        })
    }
}

// ---------------------------------------------------------------------------
// macOS implementation
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn capture_iphone_window() -> Result<Screenshot> {
    let windows =
        xcap::Window::all().map_err(|e| anyhow!("xcap::Window::all failed: {e}"))?;
    let window = windows
        .into_iter()
        .find(|w| {
            w.title()
                .map(|t| t.starts_with(WINDOW_TITLE_PREFIX))
                .unwrap_or(false)
        })
        .ok_or_else(|| {
            anyhow!(
                "iPhone Mirroring window not found — is the app running and \
                 visible? Open Apple Menu → System Settings → iPhone Mirroring."
            )
        })?;

    let img: RgbaImage = window
        .capture_image()
        .map_err(|e| anyhow!("xcap window capture failed: {e}"))?;
    let physical_w = img.width();
    let physical_h = img.height();

    // The iPhone Mirroring window scale factor is the host display's
    // (Retina), not the iPhone's. We treat the window's pixel content
    // as 1× from the model's POV so coordinate scaling stays simple.
    let scale_factor: f32 = 1.0;

    let mut png_bytes: Vec<u8> = Vec::with_capacity(physical_w as usize * physical_h as usize / 4);
    {
        let mut cursor = std::io::Cursor::new(&mut png_bytes);
        img.write_to(&mut cursor, ImageFormat::Png)
            .context("PNG encode failed")?;
    }

    Ok(Screenshot {
        png_bytes,
        logical_size: (physical_w, physical_h),
        physical_size: (physical_w, physical_h),
        scale_factor,
    })
}

/// Resolve the iPhone Mirroring window's screen position so we can map
/// in-iPhone coordinates → absolute Mac coordinates for enigo input.
#[cfg(target_os = "macos")]
fn iphone_window_origin() -> Result<(i32, i32, u32, u32)> {
    let windows =
        xcap::Window::all().map_err(|e| anyhow!("xcap::Window::all failed: {e}"))?;
    let window = windows
        .into_iter()
        .find(|w| {
            w.title()
                .map(|t| t.starts_with(WINDOW_TITLE_PREFIX))
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("iPhone Mirroring window not found"))?;

    let x = window.x().unwrap_or(0);
    let y = window.y().unwrap_or(0);
    let w = window.width().unwrap_or(0);
    let h = window.height().unwrap_or(0);
    Ok((x, y, w, h))
}

#[cfg(target_os = "macos")]
async fn activate_iphone_mirroring() -> Result<ActionOutput> {
    let out = tokio::process::Command::new("osascript")
        .args([
            "-e",
            &format!("tell application \"{APP_BUNDLE_NAME}\" to activate"),
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Ok(ActionOutput::ok()),
        Ok(o) => Ok(ActionOutput::err(format!(
            "osascript activate failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(e) => Ok(ActionOutput::err(format!("osascript spawn failed: {e}"))),
    }
}

#[cfg(target_os = "macos")]
fn execute_blocking(action: &Action, _ctx: &ExecCtx) -> Result<ActionOutput> {
    let mut enigo = match Enigo::new(&Settings::default()) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "failed to construct Enigo");
            return Ok(ActionOutput::err(format!("enigo init failed: {e}")));
        }
    };

    let (win_x, win_y, _win_w, _win_h) = iphone_window_origin()?;

    // Translate model coords (in-iPhone) to Mac screen coords by adding
    // the window origin. enigo on macOS expects logical coordinates;
    // the iPhone window is already in logical space because xcap returns
    // window position in logical pixels.
    let to_screen = |x: i32, y: i32| (win_x + x, win_y + y);

    match action {
        Action::Click { x, y, button } => {
            let (sx, sy) = to_screen(*x, *y);
            let _ = enigo.move_mouse(sx, sy, Coordinate::Abs);
            let btn = match button {
                MouseButton::Left => Button::Left,
                MouseButton::Right => Button::Right,
                MouseButton::Middle => Button::Middle,
            };
            enigo
                .button(btn, Click)
                .map_err(|e| anyhow!("click failed: {e}"))?;
            Ok(ActionOutput::ok())
        }
        Action::DoubleClick { x, y } => {
            let (sx, sy) = to_screen(*x, *y);
            let _ = enigo.move_mouse(sx, sy, Coordinate::Abs);
            enigo
                .button(Button::Left, Click)
                .map_err(|e| anyhow!("double_click step1 failed: {e}"))?;
            std::thread::sleep(Duration::from_millis(60));
            enigo
                .button(Button::Left, Click)
                .map_err(|e| anyhow!("double_click step2 failed: {e}"))?;
            Ok(ActionOutput::ok())
        }
        Action::MouseMove { x, y } => {
            let (sx, sy) = to_screen(*x, *y);
            enigo
                .move_mouse(sx, sy, Coordinate::Abs)
                .map_err(|e| anyhow!("mouse_move failed: {e}"))?;
            Ok(ActionOutput::ok())
        }
        Action::Drag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            // Drag = swipe. Model emits this for swipes too because
            // VlmDriver maps `swipe` → Action::Drag at parse time.
            let (a, b) = to_screen(*from_x, *from_y);
            let (c, d) = to_screen(*to_x, *to_y);
            enigo
                .move_mouse(a, b, Coordinate::Abs)
                .map_err(|e| anyhow!("swipe move failed: {e}"))?;
            enigo
                .button(Button::Left, Press)
                .map_err(|e| anyhow!("swipe press failed: {e}"))?;
            // Tween a few intermediate points so the iPhone gesture
            // recognizer sees a real swipe, not a teleport.
            const STEPS: i32 = 12;
            for i in 1..=STEPS {
                let f = i as f32 / STEPS as f32;
                let nx = a + ((c - a) as f32 * f) as i32;
                let ny = b + ((d - b) as f32 * f) as i32;
                let _ = enigo.move_mouse(nx, ny, Coordinate::Abs);
                std::thread::sleep(Duration::from_millis(15));
            }
            enigo
                .button(Button::Left, Release)
                .map_err(|e| anyhow!("swipe release failed: {e}"))?;
            Ok(ActionOutput::ok())
        }
        Action::HoldKey { key, seconds } => {
            // Long-press: parser maps `long_press(...)` → HoldKey with
            // a virtual "iphone_long_press" key — but the more common
            // path is the model emits long_press(start_box=...) which
            // the driver maps to Action::Click first then we'd need a
            // hold variant. For now just sleep + click.
            let _ = (key, seconds);
            Ok(ActionOutput::err(
                "iphone_mirror: hold_key not directly supported; use long_press via Click + sleep",
            ))
        }
        Action::Type { text } => {
            let stripped = text.trim_end_matches('\n').trim_end_matches("\\n");
            if !stripped.is_empty() {
                enigo
                    .text(stripped)
                    .map_err(|e| anyhow!("type failed: {e}"))?;
            }
            if text.ends_with('\n') || text.ends_with("\\n") {
                let _ = enigo.key(Key::Return, Click);
            }
            Ok(ActionOutput::ok())
        }
        Action::Hotkey { keys } => {
            // Map the two iOS-virtual hotkeys we care about:
            //   "press_home"  -> swipe up from the bottom edge (handled
            //                    upstream in the driver; if it lands here
            //                    we fall through to a no-op error).
            //   "press_back"  -> swipe right from the left edge (same).
            //
            // Other hotkeys are not meaningful in iPhone Mirroring.
            warn!(keys, "iphone_mirror: hotkey not natively supported");
            Ok(ActionOutput::err(format!(
                "iphone_mirror: hotkey '{keys}' not supported. Use \
                 press_home / press_back / swipe instead."
            )))
        }
        Action::Scroll {
            x,
            y,
            direction,
            clicks,
        } => {
            // iPhone scrolling = vertical swipe. Translate to a swipe
            // gesture rather than the wheel event (which iPhone
            // Mirroring may not forward to the device).
            let (sx, sy) = to_screen(*x, *y);
            let dist = clicks.abs().max(1) as i32 * 80;
            let (dx, dy) = match direction {
                ScrollDir::Up => (0, -dist),
                ScrollDir::Down => (0, dist),
                ScrollDir::Left => (-dist, 0),
                ScrollDir::Right => (dist, 0),
            };
            enigo
                .move_mouse(sx, sy, Coordinate::Abs)
                .map_err(|e| anyhow!("scroll move failed: {e}"))?;
            enigo
                .button(Button::Left, Press)
                .map_err(|e| anyhow!("scroll press failed: {e}"))?;
            const STEPS: i32 = 10;
            for i in 1..=STEPS {
                let f = i as f32 / STEPS as f32;
                let nx = sx + (dx as f32 * f) as i32;
                let ny = sy + (dy as f32 * f) as i32;
                let _ = enigo.move_mouse(nx, ny, Coordinate::Abs);
                std::thread::sleep(Duration::from_millis(15));
            }
            // Fall through — keep enigo wheel as well in case the device
            // window is currently using a hover-scroll surface.
            let amt = clicks.abs() as i32;
            let axis = match direction {
                ScrollDir::Up | ScrollDir::Down => Axis::Vertical,
                ScrollDir::Left | ScrollDir::Right => Axis::Horizontal,
            };
            let signed = match direction {
                ScrollDir::Up | ScrollDir::Left => -amt,
                ScrollDir::Down | ScrollDir::Right => amt,
            };
            let _ = enigo.scroll(signed, axis);
            enigo
                .button(Button::Left, Release)
                .map_err(|e| anyhow!("scroll release failed: {e}"))?;
            Ok(ActionOutput::ok())
        }
        // Anything else falls through with a clear message — keeps the
        // operator robust against parser drift.
        Action::Screenshot
        | Action::Wait { .. }
        | Action::Finished { .. }
        | Action::CallUser { .. }
        | Action::ActivateApp { .. } => {
            // These were handled in the async wrapper or are no-ops.
            Ok(ActionOutput::ok())
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn execute_blocking(_action: &Action, _ctx: &ExecCtx) -> Result<ActionOutput> {
    Ok(ActionOutput::err(
        "iPhone Mirroring is macOS-only (Sequoia 15+ required)",
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_iphone_mirror() {
        assert_eq!(IphoneMirrorOperator::new().name(), "iphone_mirror");
    }

    #[test]
    fn action_spaces_includes_ios_specific() {
        let specs = IphoneMirrorOperator::new().action_spaces();
        let rendered: Vec<String> = specs.iter().map(|s| s.render()).collect();
        let joined = rendered.join("\n");
        assert!(joined.contains("tap("));
        assert!(joined.contains("swipe("));
        assert!(joined.contains("press_home"));
        assert!(joined.contains("press_back"));
        assert!(!joined.contains("hotkey"), "iOS shouldn't advertise hotkey");
        assert!(!joined.contains("right_single"), "iOS has no right-click");
    }
}
