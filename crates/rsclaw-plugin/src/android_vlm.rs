//! Native VLM operator backed by the CLS UIAutomator2 relay.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use rsclaw_computer::{
    Action, ActionSpec, ExecCtx, MouseButton, Operator, Screenshot, ScrollDir,
    operator::{ActionFut, ActionOutput, ScreenshotFut},
};

/// Android operator used by the host-side VLM driver.
pub(crate) struct AndroidUiautoOperator;

impl Operator for AndroidUiautoOperator {
    fn name(&self) -> &'static str {
        "android_uiauto"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec::new("click(start_box='(x,y)')"),
            ActionSpec::with_note(
                "long_press(start_box='(x,y)')",
                "# Touch and hold ~1s to open context menus",
            ),
            ActionSpec::new("type(content='')"),
            ActionSpec::new("scroll(start_box='(x,y)', direction='down or up or left or right')"),
            ActionSpec::new("activate_app(app='')"),
            ActionSpec::with_note("press_back()", "# Android back button"),
            ActionSpec::with_note("press_home()", "# Android home button"),
            ActionSpec::with_note("wait()", "# Wait for the current Android page to settle"),
            ActionSpec::new("finished(content='DONE|name')"),
            ActionSpec::new("call_user(reason='')"),
        ]
    }

    fn screenshot(&self) -> ScreenshotFut<'_> {
        Box::pin(async move {
            let raw = crate::android_uiauto::call("screenshot", "{}")
                .await
                .map_err(|error| anyhow!(error))?;
            let value: serde_json::Value =
                serde_json::from_str(&raw).context("parse CLS Android screenshot response")?;
            let encoded = value
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow!("CLS Android screenshot response missing data"))?;
            let png_bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("decode CLS Android screenshot")?;
            let (width, height) = png_dimensions(&png_bytes)?;
            Ok(Screenshot {
                png_bytes,
                logical_size: (width, height),
                physical_size: (width, height),
                scale_factor: 1.0,
            })
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            match action {
                Action::Click {
                    x,
                    y,
                    button: MouseButton::Left,
                } => {
                    let args = serde_json::json!({ "x": x, "y": y }).to_string();
                    crate::android_uiauto::call("tap", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::ClickAndWait { x, y, wait_ms } => {
                    let args = serde_json::json!({ "x": x, "y": y }).to_string();
                    crate::android_uiauto::call("tap", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    tokio::time::sleep(Duration::from_millis(*wait_ms as u64)).await;
                    Ok(ActionOutput::ok())
                }
                Action::LongPress { x, y, duration_ms } => {
                    let args = serde_json::json!({ "x": x, "y": y, "durationMs": duration_ms })
                        .to_string();
                    crate::android_uiauto::call("long-press", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::DoubleClick { x, y } => {
                    let args = serde_json::json!({ "x": x, "y": y }).to_string();
                    crate::android_uiauto::call("double-tap", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::Type { text } => {
                    let args = serde_json::json!({ "text": text }).to_string();
                    crate::android_uiauto::call("input-text", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::Wait { seconds } => {
                    tokio::time::sleep(Duration::from_secs_f32(seconds.clamp(0.0, 30.0))).await;
                    Ok(ActionOutput::ok())
                }
                Action::Scroll {
                    x,
                    y,
                    direction,
                    clicks,
                } => {
                    let w = ctx.screen_w as i32;
                    let h = ctx.screen_h as i32;
                    let cx = if *x > 0 { *x } else { w / 2 };
                    let cy = if *y > 0 { *y } else { h / 2 };
                    let dist = (h / 4).max(200) * clicks.abs().max(1).min(3);
                    let (x1, y1, x2, y2) = match direction {
                        ScrollDir::Up => (cx, cy + dist / 2, cx, cy - dist / 2),
                        ScrollDir::Down => (cx, cy - dist / 2, cx, cy + dist / 2),
                        ScrollDir::Left => (cx + dist / 2, cy, cx - dist / 2, cy),
                        ScrollDir::Right => (cx - dist / 2, cy, cx + dist / 2, cy),
                    };
                    let args = serde_json::json!({
                        "x1": x1, "y1": y1, "x2": x2, "y2": y2, "durationMs": 300
                    })
                    .to_string();
                    crate::android_uiauto::call("swipe", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::Hotkey { keys } => {
                    let key_name = match keys.as_str() {
                        "press_back" | "back" => "back",
                        "press_home" | "home" => "home",
                        "enter" | "return" => "enter",
                        "delete" | "del" | "backspace" => "del",
                        _ => {
                            return Ok(ActionOutput::err(
                                "unsupported hotkey for Android UIAutomator2",
                            ));
                        }
                    };
                    let args = serde_json::json!({ "key": key_name }).to_string();
                    crate::android_uiauto::call("key", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    Ok(ActionOutput::ok())
                }
                Action::ActivateApp { app } => {
                    let args = serde_json::json!({ "package": app }).to_string();
                    crate::android_uiauto::call("launch", &args)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Ok(ActionOutput::ok())
                }
                Action::Finished { .. } | Action::CallUser { .. } => Ok(ActionOutput::ok()),
                _ => Ok(ActionOutput::err(
                    "action is not available for Android UIAutomator2",
                )),
            }
        })
    }
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(anyhow!("CLS Android screenshot is not a valid PNG"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    if width == 0 || height == 0 {
        return Err(anyhow!("CLS Android screenshot has zero dimensions"));
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_png_dimensions() {
        let mut png = vec![0_u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&1080_u32.to_be_bytes());
        png[20..24].copy_from_slice(&2408_u32.to_be_bytes());
        assert_eq!(
            png_dimensions(&png).expect("valid PNG header"),
            (1080, 2408)
        );
    }
}
