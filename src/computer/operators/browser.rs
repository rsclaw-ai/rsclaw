//! Browser operator — bridges the existing `web_browser` (CDP) subsystem
//! into the `Operator` trait so the VlmDriver can drive websites
//! without spawning a real desktop session.
//!
//! Defer until phase 2 of the rebuild — the native desktop operator is
//! the higher-impact one and this is just an adapter on top of code we
//! already have in `tools_web` / browser pool.

use crate::computer::action::{Action, ActionSpec, ExecCtx};
use crate::computer::operator::{ActionFut, ActionOutput, Operator, ScreenshotFut};

pub struct BrowserOperator {
    // TODO: hold an Arc<BrowserSession> from src/browser
}

impl BrowserOperator {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for BrowserOperator {
    fn default() -> Self {
        Self::new()
    }
}

impl Operator for BrowserOperator {
    fn name(&self) -> &'static str {
        "browser"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        // Browser has no hotkey / drag — but supports navigation.
        vec![
            ActionSpec::new("click(start_box='[x1,y1,x2,y2]')"),
            ActionSpec::with_note(
                "type(content='')",
                "# Add \\n at end of content to submit",
            ),
            ActionSpec::new(
                "scroll(start_box='[x1,y1,x2,y2]', direction='down or up or right or left')",
            ),
            ActionSpec::with_note("wait()", "# Sleep 2s and re-screenshot"),
            ActionSpec::with_note("navigate(url='')", "# Navigate to a URL"),
            ActionSpec::new("finished(content='xxx')"),
            ActionSpec::with_note(
                "call_user()",
                "# Stuck or need help",
            ),
        ]
    }

    fn screenshot(&self) -> ScreenshotFut<'_> {
        Box::pin(async move {
            anyhow::bail!("BrowserOperator::screenshot — not yet implemented")
        })
    }

    fn execute<'a>(&'a self, _action: &'a Action, _ctx: &'a ExecCtx) -> ActionFut<'a> {
        Box::pin(async move {
            Ok(ActionOutput::err(
                "BrowserOperator::execute — not yet implemented",
            ))
        })
    }
}
