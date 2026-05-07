//! Computer-use subsystem — RsClaw's GUI agent core.
//!
//! Architecture (5 layers, top-down):
//!
//!   Layer A. Third-party primitives (`enigo` + `xcap`):
//!       Native cross-platform input synthesis and screen capture.
//!       Replaces the previous shell-based path (cliclick / PowerShell
//!       / sips / screencapture) with ~100x faster native APIs.
//!
//!   Layer B. Operator trait (`operators/`):
//!       Platform abstraction. Implementations:
//!         - `NativeOperator`  — desktop (Mac/Win/Linux) via enigo+xcap
//!         - `BrowserOperator` — bridge to web_browser subsystem (CDP)
//!         - (future) `AdbOperator` — Android
//!       Each operator self-describes its capabilities via
//!       `action_spaces()` so the system prompt is built dynamically.
//!
//!   Layer C. Driver (`driver.rs`, `parser.rs`, `prompt.rs`):
//!       Model-agnostic AI loop. VlmDriver works with any vision model
//!       that follows the Thought/Action format (UI-TARS 1.0/1.5,
//!       Doubao, GPT-4o, Claude vision, Qwen-VL, ...). Coordinate
//!       parser is format-tolerant (4 formats: <|box_start|>, <point>,
//!       (x,y), [x1,y1,x2,y2]).
//!
//!   Layer D. Permission gate (`permission.rs`):
//!       Pre-execution consent flow. Before any UI loop runs the
//!       backend emits a PermissionRequest event; the desktop UI
//!       surfaces a modal ("RsClaw is about to control WeChat, ~10
//!       steps") and the user grants once / for the session / always
//!       (per-app) / denies. Decisions persist in redb.
//!
//!   Layer E. App rules (`app_rules.rs`, runtime data):
//!       Plain markdown files in `tools/computer_use/app-rules/`.
//!       Loaded at runtime, matched by keyword to the user's
//!       instruction, and injected into the system prompt. Adding a
//!       new app's automation knowledge does NOT require Rust changes.

pub mod action;
pub mod app_rules;
pub mod driver;
pub mod operator;
pub mod operators;
pub mod parser;
pub mod permission;
pub mod prompt;

pub use action::{Action, ActionSpec, ExecCtx, MouseButton, ParsedAction, Screenshot, ScrollDir};
pub use driver::{DriverOutcome, VlmDriver};
pub use operator::Operator;
pub use parser::{parse_vlm_response, CoordFormat};

/// Build the platform-default operator for the running OS. Browser /
/// ADB operators are constructed explicitly elsewhere (they need
/// driver-specific configuration).
pub fn default_operator() -> Box<dyn Operator> {
    Box::new(operators::native::NativeOperator::new())
}
