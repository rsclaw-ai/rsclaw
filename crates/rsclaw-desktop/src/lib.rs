//! Desktop automation primitives for WASM plugins.
//!
//! Provides cross-platform window management, input synthesis, and
//! screenshot capture.  Backed by `enigo` (input) and `xcap`
//! (screenshot) with platform-specific window queries.

use anyhow::Result;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Cross-platform desktop session.
#[async_trait::async_trait]
pub trait DesktopSession: Send + Sync {
    /// Activate an application by bundle-id (macOS), exe name (Windows), or
    /// WM_CLASS (Linux). Returns "ok" on success.
    async fn activate_app(&self, bundle_id: &str) -> Result<String, String>;

    /// List all windows of the target app. Returns JSON array:
    /// `[{"idx":1,"title":"...","x":0,"y":0,"w":900,"h":600}]`
    async fn list_windows(&self, bundle_id: &str) -> Result<String, String>;

    /// Close a specific window by its index (from list-windows).
    async fn close_window(&self, bundle_id: &str, window_idx: u32) -> Result<String, String>;

    /// Get the main window bounds (x, y, w, h) as JSON.
    /// Prefers title=="Weixin", falls back to largest window.
    async fn get_main_window(&self, bundle_id: &str) -> Result<String, String>;

    /// Screenshot the app's main window. Returns `data:image/png;base64,...`.
    async fn screenshot_window(&self, bundle_id: &str) -> Result<String, String>;

    /// Screenshot a screen region. Returns data URI.
    async fn screenshot_region(&self, x: u32, y: u32, w: u32, h: u32) -> Result<String, String>;

    /// OCR the app's main window with on-device text recognition (macOS
    /// Vision). Returns a JSON array of recognised lines with 0-1000
    /// relative centre coordinates: `[{"text":"...","x":143,"y":699}, ...]`.
    /// Precise enough to click a named row, unlike VLM grounding.
    async fn ocr_window(&self, bundle_id: &str) -> Result<String, String>;

    /// Move the mouse cursor to absolute screen coordinates without clicking.
    async fn mouse_move(&self, x: u32, y: u32) -> Result<String, String>;

    /// Mouse left-click at absolute screen coordinates.
    async fn mouse_click(&self, x: u32, y: u32) -> Result<String, String>;

    /// Mouse double-click at absolute screen coordinates.
    async fn mouse_double_click(&self, x: u32, y: u32) -> Result<String, String>;

    /// Drag from (x1, y1) to (x2, y2).
    async fn mouse_drag(&self, x1: u32, y1: u32, x2: u32, y2: u32) -> Result<String, String>;

    /// Scroll the mouse wheel. Positive clicks = down, negative = up.
    async fn mouse_scroll(&self, clicks: i32) -> Result<String, String>;

    /// Press a key with optional modifiers.
    /// key: key name (e.g. "Return", "Escape", "v", "k").
    /// modifiers: list of "command", "control", "shift", "option"/"alt".
    async fn key_press(&self, key: &str, modifiers: &[String]) -> Result<String, String>;

    /// Set the system clipboard text.
    async fn clipboard_set(&self, text: &str) -> Result<String, String>;

    /// Get the system clipboard text.
    async fn clipboard_get(&self) -> Result<String, String>;

    /// Set the system clipboard to a file reference (for paste-as-file).
    async fn clipboard_set_file(&self, file_path: &str) -> Result<String, String>;

    /// Get image data (PNG) from the system clipboard. Returns data URI.
    async fn clipboard_get_image(&self) -> Result<String, String>;

    /// Mouse right-click at absolute screen coordinates.
    async fn mouse_right_click(&self, x: u32, y: u32) -> Result<String, String>;

    /// Open a native file dialog. Returns the selected file path or error.
    async fn file_dialog_open(&self, title: &str, filters: &[String]) -> Result<String, String>;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a platform-appropriate desktop session.
pub fn create_session() -> Box<dyn DesktopSession> {
    #[cfg(target_os = "macos")]
    {
        Box::new(native::NativeDesktopSession)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(native::NativeDesktopSession)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(native::NativeDesktopSession)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        compile_error!("DesktopSession not supported on this platform")
    }
}

mod native;
pub mod macos_perm;
