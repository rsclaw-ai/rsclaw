//! Shared types for the computer-use subsystem.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Screenshot
// ---------------------------------------------------------------------------

/// A single screen capture.
///
/// `logical_size` is the size the OS reports to applications (DPI-aware);
/// `physical_size` is the actual pixel count of the captured image.
/// `scale_factor` = physical / logical, used to map model coordinates
/// (which see the physical bytes) back to logical click positions.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub png_bytes: Vec<u8>,
    pub logical_size: (u32, u32),
    pub physical_size: (u32, u32),
    pub scale_factor: f32,
}

// ---------------------------------------------------------------------------
// Action — what an Operator can execute
// ---------------------------------------------------------------------------

/// One executable action. Coordinates are in **physical** pixels (raw
/// screenshot space). The operator scales to logical when needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// No-op pause. `seconds` clamped to [0, 60] by the operator.
    Wait { seconds: f32 },

    /// Move cursor to (x, y) without clicking.
    MouseMove { x: i32, y: i32 },

    /// Single click at (x, y).
    Click {
        x: i32,
        y: i32,
        button: MouseButton,
    },

    /// Double-click at (x, y) (left button).
    DoubleClick { x: i32, y: i32 },

    /// Click-drag from start to end.
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },

    /// Scroll N "ticks" in a direction at (x, y).
    Scroll {
        x: i32,
        y: i32,
        direction: ScrollDir,
        clicks: i32,
    },

    /// Type literal text at the keyboard focus. `\n` submits when present
    /// at the end (handled by the operator).
    Type { text: String },

    /// Press a key combination. Keys are space- or +-separated, lowercase.
    /// Examples: "return", "ctrl c", "cmd shift t".
    Hotkey { keys: String },

    /// Hold a key for `seconds`. Useful for game-style inputs.
    HoldKey { key: String, seconds: f32 },

    /// Take a screenshot. Operator returns it; the action body itself
    /// has no side effects beyond capture.
    Screenshot,

    /// Bring `app` (case-insensitive display name) to the front and
    /// activate ALL its windows. Important for multi-window apps where
    /// the main window may be behind a secondary panel.
    ActivateApp { app: String },

    /// Terminal action — the model declares the task complete.
    /// `content` is the model's summary for the user.
    Finished { content: String },

    /// Terminal action — the model is stuck and needs human input.
    CallUser { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDir {
    Up,
    Down,
    Left,
    Right,
}

// ---------------------------------------------------------------------------
// ActionSpec — what an Operator advertises to the LLM
// ---------------------------------------------------------------------------

/// One line of an Operator's documented action space, injected into the
/// system prompt at runtime. Allows different operators to expose
/// different capabilities (e.g. browser doesn't have hotkey/drag,
/// mobile doesn't have right-click).
#[derive(Debug, Clone)]
pub struct ActionSpec {
    /// LLM-facing signature line, e.g.
    /// `click(start_box='<|box_start|>(x1,y1)<|box_end|>')`.
    pub signature: &'static str,
    /// Optional inline annotation, e.g. `# Use \\n at end to submit`.
    pub note: Option<&'static str>,
}

impl ActionSpec {
    pub const fn new(signature: &'static str) -> Self {
        Self {
            signature,
            note: None,
        }
    }
    pub const fn with_note(signature: &'static str, note: &'static str) -> Self {
        Self {
            signature,
            note: Some(note),
        }
    }

    /// Render as a single line for the system prompt.
    pub fn render(&self) -> String {
        match self.note {
            Some(n) => format!("{}  {}", self.signature, n),
            None => self.signature.to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// ExecCtx — passed to Operator::execute()
// ---------------------------------------------------------------------------

/// Context an operator needs to execute a parsed action correctly:
/// screen dims for box-coord normalization, scale factor for physical
/// vs logical mapping, and the model's `factors` (e.g. `[1000, 1000]`
/// for normalized 0-1000 coords) for pre-scaling.
#[derive(Debug, Clone, Copy)]
pub struct ExecCtx {
    pub screen_w: u32,
    pub screen_h: u32,
    pub scale_factor: f32,
    /// Model coordinate factors `[width, height]`. Most VLMs use
    /// `[1000, 1000]` (normalized 0-1000) or `[screen_w, screen_h]`
    /// (raw pixel). Driver picks based on the model variant.
    pub factors: [u32; 2],
}

// ---------------------------------------------------------------------------
// ParsedAction — what the parser emits (pre-coord-scaling)
// ---------------------------------------------------------------------------

/// A single Action extracted from the model's `Action: ...` line, plus
/// the surrounding `Thought:` content for logging/UI. Coordinates are
/// kept in their raw model-space form here; the driver scales to
/// physical pixels via [`ExecCtx`] before passing to the operator.
#[derive(Debug, Clone)]
pub struct ParsedAction {
    pub thought: String,
    pub action_type: String,
    /// Raw arg → value pairs as the model emitted them. Driver
    /// interprets `start_box`, `end_box`, `point`, `key`, `content`,
    /// `direction`, etc.
    pub raw_args: std::collections::BTreeMap<String, String>,
    /// Coordinates already extracted from box/point args, in
    /// model-space (NOT yet scaled to screen). `None` for non-spatial
    /// actions like `type` or `hotkey`.
    pub start: Option<(f32, f32)>,
    pub end: Option<(f32, f32)>,
}
