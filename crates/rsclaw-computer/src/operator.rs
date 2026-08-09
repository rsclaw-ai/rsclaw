//! `Operator` trait — platform abstraction for computer-use.
//!
//! Implementations live under `operators/`. Adding a new platform is
//! one new file; the rest of the stack (parser, driver, prompt, app
//! rules) is untouched.
//!
//! Uses native Rust 2024 async fn in trait per project rules — no
//! `async-trait` macro.

use std::pin::Pin;

use anyhow::Result;

use super::action::{Action, ActionSpec, ExecCtx, Screenshot};

/// One execution result. Most actions just succeed (`ok = true`); error
/// detail is surfaced via `message` so the VlmDriver can feed it back
/// into the next turn's context.
#[derive(Debug, Clone)]
pub struct ActionOutput {
    pub ok: bool,
    pub message: Option<String>,
}

impl ActionOutput {
    pub fn ok() -> Self {
        Self {
            ok: true,
            message: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(msg.into()),
        }
    }
}

/// Boxed future returned from trait methods so the trait stays
/// `dyn`-compatible (native async fn in trait isn't dyn-safe yet
/// without `Pin<Box<...>>` — and `dyn Operator` is needed because
/// the runtime picks the platform impl at startup).
pub type ActionFut<'a> = Pin<Box<dyn Future<Output = Result<ActionOutput>> + Send + 'a>>;
pub type ScreenshotFut<'a> = Pin<Box<dyn Future<Output = Result<Screenshot>> + Send + 'a>>;
pub type FrontmostFut<'a> = Pin<Box<dyn Future<Output = Result<Option<String>>> + Send + 'a>>;

pub trait Operator: Send + Sync {
    /// Stable name for logging / telemetry: "native", "browser", "adb".
    fn name(&self) -> &'static str;

    /// Display name of the application currently receiving input, e.g.
    /// `"WeChat"`. `Ok(None)` means "this operator cannot tell" — the
    /// caller must then treat the target as unverified rather than
    /// assuming it matches.
    ///
    /// This is the permission gate's ground truth. Keying consent on a
    /// label guessed from the user's instruction text is unsound: the
    /// user approves "control WeChat" while the loop is free to drive
    /// whatever happens to be focused. Implementations that genuinely
    /// cannot introspect focus should keep the default `Ok(None)` and
    /// let the caller fail closed.
    fn frontmost_app(&self) -> FrontmostFut<'_> {
        Box::pin(async { Ok(None) })
    }

    /// Self-described capabilities — fed into the system prompt by
    /// `prompt::build_system_prompt`. Different operators may expose
    /// different action sets (browser has no hotkey, mobile has no
    /// right-click).
    fn action_spaces(&self) -> Vec<ActionSpec>;

    /// Capture the current screen. Operators handle DPI / multi-monitor
    /// internally; the returned `Screenshot.scale_factor` lets the
    /// driver map model coords back to logical click positions.
    fn screenshot(&self) -> ScreenshotFut<'_>;

    /// Execute one action. The `ctx` carries screen dims and
    /// model-coordinate factors so the operator can scale box/point
    /// coords to its native input space.
    fn execute<'a>(&'a self, action: &'a Action, ctx: &'a ExecCtx) -> ActionFut<'a>;
}
