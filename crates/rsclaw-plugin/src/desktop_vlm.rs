//! VLM desktop operator backed by the host's existing `DesktopSession`.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use rsclaw_computer::{
    Action, ActionSpec, ExecCtx, MouseButton, Operator, ParsedAction, Screenshot, ScrollDir,
    operator::{ActionFut, ActionOutput, FrontmostFut, ScreenshotFut},
};
use rsclaw_desktop::DesktopSession;

#[derive(Clone, Copy)]
struct Viewport {
    x: i32,
    y: i32,
    logical_w: u32,
    logical_h: u32,
    physical_w: u32,
    physical_h: u32,
}

/// Desktop operator that preserves the host's capture and input implementation.
pub(crate) struct DesktopSessionOperator {
    session: Arc<dyn DesktopSession>,
    expected_app: String,
    viewport: Mutex<Option<Viewport>>,
}

impl DesktopSessionOperator {
    pub(crate) fn new(session: Arc<dyn DesktopSession>, expected_app: String) -> Result<Self> {
        if expected_app.trim().is_empty() {
            return Err(anyhow!("expected-app must not be empty"));
        }
        Ok(Self {
            session,
            expected_app,
            viewport: Mutex::new(None),
        })
    }

    async fn require_expected_frontmost(&self) -> Result<()> {
        match self.session.is_app_frontmost(&self.expected_app).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!(
                "foreground application does not match expected-app '{}'",
                self.expected_app
            )),
            Err(error) => Err(anyhow!(
                "cannot verify foreground application for expected-app '{}': {error}",
                self.expected_app
            )),
        }
    }

    fn screen_point(&self, x: i32, y: i32) -> Result<(u32, u32)> {
        let viewport = self
            .viewport
            .lock()
            .map_err(|_| anyhow!("desktop viewport lock poisoned"))?
            .ok_or_else(|| anyhow!("desktop viewport unavailable before screenshot"))?;
        if x < 0 || y < 0 || x >= viewport.physical_w as i32 || y >= viewport.physical_h as i32 {
            return Err(anyhow!(
                "desktop action coordinate is outside the observed screenshot"
            ));
        }
        let logical_x =
            i64::from(x) * i64::from(viewport.logical_w) / i64::from(viewport.physical_w);
        let logical_y =
            i64::from(y) * i64::from(viewport.logical_h) / i64::from(viewport.physical_h);
        let screen_x = i64::from(viewport.x) + logical_x;
        let screen_y = i64::from(viewport.y) + logical_y;
        if screen_x < 0
            || screen_y < 0
            || screen_x > i64::from(u32::MAX)
            || screen_y > i64::from(u32::MAX)
        {
            return Err(anyhow!(
                "desktop action maps outside the addressable screen"
            ));
        }
        Ok((screen_x as u32, screen_y as u32))
    }

    fn validate_click_box(parsed: &ParsedAction, action: &Action, ctx: &ExecCtx) -> Result<()> {
        let Some((x, y)) = click_point(action) else {
            return Ok(());
        };
        let raw = parsed.raw_args.get("start_box").ok_or_else(|| {
            anyhow!("click requires start_box='<box>x1,y1,x2,y2</box>' with four coordinates")
        })?;
        let [x1, y1, x2, y2] = parse_box(raw)?;
        if ![x1, y1, x2, y2].iter().all(|coord| coord.is_finite()) {
            return Err(anyhow!("click box coordinates must be finite"));
        }
        if !(0.0..=1000.0).contains(&x1)
            || !(0.0..=1000.0).contains(&y1)
            || !(0.0..=1000.0).contains(&x2)
            || !(0.0..=1000.0).contains(&y2)
        {
            return Err(anyhow!("click box coordinates must be within 0..1000"));
        }
        if x1 >= x2 || y1 >= y2 {
            return Err(anyhow!(
                "click box coordinates must be ordered x1 < x2 and y1 < y2"
            ));
        }

        let left = (f64::from(x1) * f64::from(ctx.screen_w) / 1000.0).ceil() as i32;
        let top = (f64::from(y1) * f64::from(ctx.screen_h) / 1000.0).ceil() as i32;
        let right = (f64::from(x2) * f64::from(ctx.screen_w) / 1000.0).floor() as i32;
        let bottom = (f64::from(y2) * f64::from(ctx.screen_h) / 1000.0).floor() as i32;
        if left >= right || top >= bottom {
            return Err(anyhow!(
                "click box is too small after mapping to physical pixels"
            ));
        }
        if x < 0
            || y < 0
            || x >= ctx.screen_w as i32
            || y >= ctx.screen_h as i32
            || x - left <= 10
            || right - x <= 10
            || y - top <= 10
            || bottom - y <= 10
        {
            return Err(anyhow!(
                "mapped click must be more than 10 physical pixels inside every edge of its box"
            ));
        }
        Ok(())
    }

    async fn press_hotkey(&self, keys: &str) -> Result<()> {
        let mut parts: Vec<String> = keys
            .split([' ', '+'])
            .filter(|part| !part.is_empty())
            .map(|part| part.to_ascii_lowercase())
            .collect();
        let key = parts
            .pop()
            .ok_or_else(|| anyhow!("hotkey must include a key"))?;
        let modifiers: Vec<String> = parts
            .into_iter()
            .map(|modifier| match modifier.as_str() {
                "cmd" => "command".to_string(),
                "ctrl" => "control".to_string(),
                other => other.to_string(),
            })
            .collect();
        self.session
            .key_press(&key, &modifiers)
            .await
            .map_err(|error| anyhow!(error))?;
        Ok(())
    }
}

impl Operator for DesktopSessionOperator {
    fn name(&self) -> &'static str {
        "desktop_session"
    }

    fn frontmost_app(&self) -> FrontmostFut<'_> {
        Box::pin(async move {
            if self
                .session
                .is_app_frontmost(&self.expected_app)
                .await
                .map_err(|error| anyhow!(error))?
            {
                Ok(Some(self.expected_app.clone()))
            } else {
                Ok(None)
            }
        })
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        vec![
            ActionSpec::new("click(start_box='<box>x1,y1,x2,y2</box>')"),
            ActionSpec::new("left_double(start_box='<box>x1,y1,x2,y2</box>')"),
            ActionSpec::new("right_single(start_box='<box>x1,y1,x2,y2</box>')"),
            ActionSpec::new("drag(start_box='<box>x1,y1</box>', end_box='<box>x3,y3</box>')"),
            ActionSpec::new("hotkey(key='')"),
            ActionSpec::new("type(content='')"),
            ActionSpec::new("scroll(start_box='<box>x1,y1</box>', direction='down or up')"),
            ActionSpec::new("wait()"),
            ActionSpec::new("finished(content='')"),
            ActionSpec::new("call_user(reason='')"),
        ]
    }

    fn validate_action(&self, parsed: &ParsedAction, action: &Action, ctx: &ExecCtx) -> Result<()> {
        Self::validate_click_box(parsed, action, ctx)
    }

    fn screenshot(&self) -> ScreenshotFut<'_> {
        Box::pin(async move {
            *self
                .viewport
                .lock()
                .map_err(|_| anyhow!("desktop viewport lock poisoned"))? = None;
            self.require_expected_frontmost().await?;
            // The host capture is the physical full screen. Do not enumerate or
            // inspect application windows to derive bounds: VLM observations and
            // input coordinates stay in the same full-screen pixel space.
            let data_uri = self
                .session
                .screenshot_full()
                .await
                .map_err(|error| anyhow!(error))?;
            self.require_expected_frontmost().await?;
            let encoded = data_uri
                .split_once(";base64,")
                .map(|(_, encoded)| encoded)
                .ok_or_else(|| anyhow!("desktop screenshot is not a base64 data URI"))?;
            let png_bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .context("decode desktop screenshot")?;
            let (physical_w, physical_h) = png_dimensions(&png_bytes)?;
            if physical_w == 0 || physical_h == 0 {
                return Err(anyhow!("full-screen screenshot has zero size"));
            }
            let viewport = Viewport {
                x: 0,
                y: 0,
                logical_w: physical_w,
                logical_h: physical_h,
                physical_w,
                physical_h,
            };
            *self
                .viewport
                .lock()
                .map_err(|_| anyhow!("desktop viewport lock poisoned"))? = Some(viewport);
            Ok(Screenshot {
                png_bytes,
                logical_size: (physical_w, physical_h),
                physical_size: (physical_w, physical_h),
                scale_factor: 1.0,
            })
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, _ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            if matches!(
                action,
                Action::Wait { .. } | Action::Finished { .. } | Action::CallUser { .. }
            ) {
                return match action {
                    Action::Wait { seconds } => {
                        tokio::time::sleep(Duration::from_secs_f32(seconds.clamp(0.0, 60.0))).await;
                        Ok(ActionOutput::ok())
                    }
                    _ => Ok(ActionOutput::ok()),
                };
            }
            self.require_expected_frontmost().await?;
            let result = match action {
                Action::MouseMove { x, y } => {
                    let (x, y) = self.screen_point(*x, *y)?;
                    self.session.mouse_move(x, y).await
                }
                Action::Click { x, y, button } => {
                    let (x, y) = self.screen_point(*x, *y)?;
                    match button {
                        MouseButton::Left => self.session.mouse_click(x, y).await,
                        MouseButton::Right => self.session.mouse_right_click(x, y).await,
                        MouseButton::Middle => {
                            Err("middle click is unavailable for desktop-vlm-drive".to_string())
                        }
                    }
                }
                Action::ClickAndWait { x, y, wait_ms } => {
                    let (x, y) = self.screen_point(*x, *y)?;
                    let result = self.session.mouse_click(x, y).await;
                    if result.is_ok() {
                        tokio::time::sleep(Duration::from_millis(u64::from(*wait_ms))).await;
                    }
                    result
                }
                Action::DoubleClick { x, y } => {
                    let (x, y) = self.screen_point(*x, *y)?;
                    self.session.mouse_double_click(x, y).await
                }
                Action::Drag {
                    from_x,
                    from_y,
                    to_x,
                    to_y,
                } => {
                    let (x1, y1) = self.screen_point(*from_x, *from_y)?;
                    let (x2, y2) = self.screen_point(*to_x, *to_y)?;
                    self.session.mouse_drag(x1, y1, x2, y2).await
                }
                Action::Scroll {
                    direction, clicks, ..
                } => {
                    let signed = match direction {
                        ScrollDir::Up => -clicks.abs(),
                        ScrollDir::Down => clicks.abs(),
                        ScrollDir::Left | ScrollDir::Right => {
                            return Ok(ActionOutput::err(
                                "horizontal scroll is unavailable for desktop-vlm-drive",
                            ));
                        }
                    };
                    self.session.mouse_scroll(signed).await
                }
                Action::Type { text } => {
                    let (body, submit) = text
                        .strip_suffix('\n')
                        .map_or((text.as_str(), false), |body| (body, true));
                    self.session
                        .clipboard_set(body)
                        .await
                        .map_err(|error| anyhow!(error))?;
                    self.require_expected_frontmost().await?;
                    self.press_hotkey(if cfg!(target_os = "macos") {
                        "cmd v"
                    } else {
                        "ctrl v"
                    })
                    .await?;
                    if submit {
                        self.require_expected_frontmost().await?;
                        self.session
                            .key_press("Return", &[])
                            .await
                            .map_err(|error| anyhow!(error))?;
                    }
                    Ok("ok".to_string())
                }
                Action::Hotkey { keys } => {
                    self.press_hotkey(keys).await?;
                    Ok("ok".to_string())
                }
                Action::Screenshot => Ok("ok".to_string()),
                Action::LongPress { .. } | Action::HoldKey { .. } | Action::ActivateApp { .. } => {
                    Err("action is unavailable for desktop-vlm-drive".to_string())
                }
                Action::Wait { .. } | Action::Finished { .. } | Action::CallUser { .. } => {
                    unreachable!()
                }
            };
            Ok(match result {
                Ok(_) => ActionOutput::ok(),
                Err(error) => ActionOutput::err(error),
            })
        })
    }
}

fn click_point(action: &Action) -> Option<(i32, i32)> {
    match action {
        Action::Click { x, y, .. }
        | Action::ClickAndWait { x, y, .. }
        | Action::DoubleClick { x, y }
        | Action::LongPress { x, y, .. } => Some((*x, *y)),
        _ => None,
    }
}

fn parse_box(raw: &str) -> Result<[f32; 4]> {
    let inner = raw
        .strip_prefix("<box>")
        .and_then(|value| value.strip_suffix("</box>"))
        .ok_or_else(|| {
            anyhow!("click requires start_box='<box>x1,y1,x2,y2</box>' with four coordinates")
        })?;
    let values: Vec<f32> = inner
        .split(',')
        .map(|value| value.trim().parse::<f32>())
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| anyhow!("click box must contain four numeric coordinates"))?;
    values.try_into().map_err(|_| {
        anyhow!("click requires start_box='<box>x1,y1,x2,y2</box>' with four coordinates")
    })
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(anyhow!("desktop screenshot is not a valid PNG"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into()?);
    if width == 0 || height == 0 {
        return Err(anyhow!("desktop screenshot has zero dimensions"));
    }
    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_screen_coordinates_are_identity_and_bounded() {
        let session: Arc<dyn DesktopSession> = rsclaw_desktop::create_session().into();
        let operator = DesktopSessionOperator::new(session, "test-app".to_string())
            .expect("nonempty expected app");
        assert!(operator.screen_point(0, 0).is_err());
        *operator.viewport.lock().expect("viewport lock") = Some(Viewport {
            x: 0,
            y: 0,
            logical_w: 1024,
            logical_h: 768,
            physical_w: 1024,
            physical_h: 768,
        });
        assert_eq!(
            operator.screen_point(165, 148).expect("in bounds"),
            (165, 148)
        );
        assert_eq!(
            operator.screen_point(1023, 767).expect("last pixel"),
            (1023, 767)
        );
        assert!(operator.screen_point(-1, 0).is_err());
        assert!(operator.screen_point(1024, 0).is_err());
        assert!(operator.screen_point(0, 768).is_err());
    }

    #[test]
    fn invalid_screenshot_dimensions_fail_closed() {
        assert!(png_dimensions(b"not a screenshot").is_err());
        let mut png = vec![0_u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(png_dimensions(&png).is_err());
    }

    #[test]
    fn reads_png_dimensions() {
        let mut png = vec![0_u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&1920_u32.to_be_bytes());
        png[20..24].copy_from_slice(&1080_u32.to_be_bytes());
        assert_eq!(
            png_dimensions(&png).expect("valid PNG header"),
            (1920, 1080)
        );
    }

    fn parsed(start_box: &str) -> ParsedAction {
        ParsedAction {
            thought: String::new(),
            action_type: "click".to_owned(),
            raw_args: [("start_box".to_owned(), start_box.to_owned())]
                .into_iter()
                .collect(),
            start: None,
            end: None,
        }
    }

    fn ctx(screen_w: u32, screen_h: u32) -> ExecCtx {
        ExecCtx {
            screen_w,
            screen_h,
            scale_factor: 1.0,
            factors: [screen_w, screen_h],
        }
    }

    #[test]
    fn click_boxes_accept_mapped_centres_with_inward_margin_at_common_sizes() {
        let parsed = parsed("<box>400,400,600,600</box>");
        for (screen_w, screen_h, x, y) in [(800, 632, 400, 316), (1440, 900, 720, 450)] {
            let action = Action::Click {
                x,
                y,
                button: MouseButton::Left,
            };
            DesktopSessionOperator::validate_click_box(&parsed, &action, &ctx(screen_w, screen_h))
                .expect("mapped box centre with margin");
        }
    }

    #[test]
    fn click_box_validation_applies_to_every_click_like_action() {
        let parsed = parsed("<box>400,400,600,600</box>");
        let actions = [
            Action::Click {
                x: 720,
                y: 450,
                button: MouseButton::Left,
            },
            Action::ClickAndWait {
                x: 720,
                y: 450,
                wait_ms: 1,
            },
            Action::DoubleClick { x: 720, y: 450 },
            Action::LongPress {
                x: 720,
                y: 450,
                duration_ms: 1,
            },
        ];
        for action in actions {
            DesktopSessionOperator::validate_click_box(&parsed, &action, &ctx(1440, 900))
                .expect("click-like action is validated against its box");
        }
    }

    #[test]
    fn click_box_validation_rejects_point_and_invalid_boxes() {
        let action = Action::Click {
            x: 400,
            y: 316,
            button: MouseButton::Left,
        };
        for raw in [
            "<box>150,205</box>",
            "<box>NaN,400,600,600</box>",
            "<box>-1,400,600,600</box>",
            "<box>500,400,500,600</box>",
            "<box>490,490,510,510</box>",
            "<point>500,500</point>",
        ] {
            assert!(
                DesktopSessionOperator::validate_click_box(&parsed(raw), &action, &ctx(800, 632))
                    .is_err(),
                "{raw} must be rejected"
            );
        }
    }

    #[test]
    fn desktop_click_action_spaces_require_four_coordinate_boxes() {
        let session: Arc<dyn DesktopSession> = rsclaw_desktop::create_session().into();
        let operator = DesktopSessionOperator::new(session, "test-app".to_owned())
            .expect("nonempty expected app");
        let signatures: Vec<_> = operator
            .action_spaces()
            .into_iter()
            .map(|spec| spec.signature)
            .collect();
        assert!(signatures.contains(&"click(start_box='<box>x1,y1,x2,y2</box>')".to_owned()));
        assert!(signatures.contains(&"left_double(start_box='<box>x1,y1,x2,y2</box>')".to_owned()));
        assert!(
            signatures.contains(&"right_single(start_box='<box>x1,y1,x2,y2</box>')".to_owned())
        );
    }
}
