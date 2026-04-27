//! Platform-specific helpers — Chrome detection, key mapping, display checks.
//!
//! Extracted from `runtime.rs` to reduce file size.

use anyhow::Result;

/// Check if a graphical display is available.
pub(crate) fn has_display() -> bool {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        true
    } else {
        std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()
    }
}

/// Detect Chrome / Chromium binary path.
///
/// Priority: user's existing system Chrome > `~/.rsclaw/tools/chrome`
/// (Chrome for Testing we manage). The reorder is intentional — most
/// users already have Google Chrome installed, and we want to ride on
/// their version (security updates, extensions, profiles) rather than
/// silently maintaining a parallel copy.
///
/// `tools/chrome` remains as a fallback so that machines without any
/// Chrome at all still work after a single `rsclaw tools install chrome`.
/// See `ensure_chrome()` for the auto-install on absence.
pub(crate) fn detect_chrome() -> Option<String> {
    // 1. System-installed Chrome (well-known locations + PATH).
    #[cfg(target_os = "macos")]
    {
        let app_paths = [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ];
        for p in &app_paths {
            if std::path::Path::new(p).exists() {
                return Some((*p).to_owned());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Registry (most reliable for system + per-user installs).
        for key_path in &[
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
            r"SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
        ] {
            for hive in &["HKLM", "HKCU"] {
                if let Ok(output) = std::process::Command::new("reg")
                    .args(["query", &format!(r"{hive}\{key_path}"), "/ve"])
                    .output()
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    if let Some(line) = stdout.lines().find(|l| l.contains("REG_SZ")) {
                        if let Some(path_str) = line.split("REG_SZ").nth(1) {
                            let path_str = path_str.trim();
                            if std::path::Path::new(path_str).exists() {
                                return Some(path_str.to_owned());
                            }
                        }
                    }
                }
            }
        }
        let candidates = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        ];
        for path in &candidates {
            if std::path::Path::new(path).exists() {
                return Some((*path).to_string());
            }
        }
        if let Ok(userprofile) = std::env::var("USERPROFILE") {
            let user_chrome = format!(
                r"{}\AppData\Local\Google\Chrome\Application\chrome.exe",
                userprofile
            );
            if std::path::Path::new(&user_chrome).exists() {
                return Some(user_chrome);
            }
        }
    }

    for name in &["google-chrome", "chromium", "chromium-browser", "chrome"] {
        if let Ok(path) = which::which(name) {
            return Some(path.to_string_lossy().to_string());
        }
    }

    // 2. Fall back to ~/.rsclaw/tools/chrome (Chrome for Testing we manage).
    let tools_dir = crate::config::loader::base_dir().join("tools/chrome");
    if tools_dir.exists() {
        #[cfg(target_os = "macos")]
        {
            let candidates = [
                "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                "Chromium.app/Contents/MacOS/Chromium",
                "Google Chrome.app/Contents/MacOS/Google Chrome",
            ];
            for name in &candidates {
                let bin = tools_dir.join(name);
                if bin.exists() {
                    return Some(bin.to_string_lossy().to_string());
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            let candidates = ["chrome.exe", "Google Chrome for Testing.exe"];
            for name in &candidates {
                let bin = tools_dir.join(name);
                if bin.exists() {
                    return Some(bin.to_string_lossy().to_string());
                }
            }
        }
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        {
            let bin = tools_dir.join("chrome");
            if bin.exists() {
                return Some(bin.to_string_lossy().to_string());
            }
        }
    }

    None
}

/// Like `detect_chrome` but auto-installs Chrome for Testing on miss.
/// First call is slow (downloads ~150MB); subsequent calls are instant.
/// Returns the absolute path to a Chrome binary, or an error if both
/// detection and install fail.
pub(crate) async fn ensure_chrome() -> Result<String> {
    if let Some(p) = detect_chrome() {
        return Ok(p);
    }
    tracing::info!("Chrome not found locally, auto-installing Chrome for Testing");
    crate::cmd::tools::cmd_install("chrome", false).await?;
    detect_chrome().ok_or_else(|| {
        anyhow::anyhow!("Chrome auto-install completed but binary still not detected")
    })
}

/// Detect ffmpeg binary path.
///
/// Priority: `~/.rsclaw/tools/ffmpeg/ffmpeg` > system PATH.
/// Local-first because the bundled build is pinned to a known-good version
/// and ships every codec we need; system ffmpeg may be a stripped distro
/// build missing libopus/libx264.
pub(crate) fn detect_ffmpeg() -> Option<String> {
    let tools_dir = crate::config::loader::base_dir().join("tools/ffmpeg");
    #[cfg(target_os = "windows")]
    {
        let local_win = tools_dir.join("ffmpeg.exe");
        if local_win.exists() {
            return Some(local_win.to_string_lossy().to_string());
        }
    }
    let local = tools_dir.join("ffmpeg");
    if local.exists() {
        return Some(local.to_string_lossy().to_string());
    }
    if let Ok(path) = which::which("ffmpeg") {
        return Some(path.to_string_lossy().to_string());
    }
    None
}

/// Like `detect_ffmpeg` but auto-installs ffmpeg on miss (downloads ~80MB).
pub(crate) async fn ensure_ffmpeg() -> Result<String> {
    if let Some(p) = detect_ffmpeg() {
        return Ok(p);
    }
    tracing::info!("ffmpeg not found locally, auto-installing");
    crate::cmd::tools::cmd_install("ffmpeg", false).await?;
    detect_ffmpeg().ok_or_else(|| {
        anyhow::anyhow!("ffmpeg auto-install completed but binary still not detected")
    })
}

/// Run a subprocess and return an error if it fails.
pub(crate) async fn run_subprocess(cmd: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("{cmd}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("{cmd} failed: {stderr}"));
    }
    Ok(())
}

/// Parse JPEG dimensions from SOF0/SOF2 marker (no external deps).
pub(crate) fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        i += 2;
        // SOF0 (0xC0) or SOF2 (0xC2) contain dimensions
        if marker == 0xC0 || marker == 0xC2 {
            if i + 7 <= data.len() {
                let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
                let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                return Some((w, h));
            }
            return None;
        }
        // Skip segment
        if marker >= 0xC0 && marker != 0xD8 && marker != 0xD9 && marker != 0x00 {
            if i + 2 <= data.len() {
                let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
                i += len;
            } else {
                break;
            }
        }
    }
    None
}

/// Run a PowerShell snippet with the required assemblies pre-loaded.
/// Used for Windows computer_use actions (mouse, keyboard).
pub(crate) async fn run_powershell_input(script: &str) -> Result<()> {
    let full = format!("Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; {script}");
    run_subprocess("powershell", &["-NoProfile", "-Command", &full]).await
}

/// Windows: set cursor position via .NET
pub(crate) async fn win_set_cursor(x: i64, y: i64) -> Result<()> {
    run_powershell_input(&format!(
        "[System.Windows.Forms.Cursor]::Position = New-Object System.Drawing.Point({x},{y})"
    )).await
}

/// Windows: mouse click with P/Invoke. Supports left/right/middle and repeat count.
pub(crate) async fn win_mouse_click(x: i64, y: i64, button: &str, clicks: i32) -> Result<()> {
    let (down_flag, up_flag) = match button {
        "right" => ("0x0008", "0x0010"),
        "middle" => ("0x0020", "0x0040"),
        _ => ("0x0002", "0x0004"),
    };
    run_powershell_input(&format!(
        r#"Add-Type @"
using System;
using System.Runtime.InteropServices;
public class WinClick {{
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] static extern void mouse_event(uint f, uint dx, uint dy, uint d, int e);
    public static void Click(int x, int y, uint down, uint up, int n) {{
        SetCursorPos(x, y);
        for (int i = 0; i < n; i++) {{
            mouse_event(down, 0, 0, 0, 0);
            mouse_event(up, 0, 0, 0, 0);
            if (i < n - 1) System.Threading.Thread.Sleep(50);
        }}
    }}
}}
"@
[WinClick]::Click({x}, {y}, {down_flag}, {up_flag}, {clicks})"#
    )).await
}

/// Windows: map key names to SendKeys format, including modifier combos.
pub(crate) fn win_map_key(key: &str) -> String {
    // Handle modifier combos like "ctrl+c" -> "^c"
    if key.contains('+') {
        let parts: Vec<&str> = key.split('+').collect();
        let mut prefix = String::new();
        for &modifier in &parts[..parts.len() - 1] {
            match modifier.to_lowercase().as_str() {
                "ctrl" | "control" => prefix.push('^'),
                "alt" => prefix.push('%'),
                "shift" => prefix.push('+'),
                _ => {}
            }
        }
        let base = win_map_single_key(parts[parts.len() - 1]);
        format!("{prefix}{base}")
    } else {
        win_map_single_key(key)
    }
}

/// Check if a key name is a cliclick kp: special key.
pub(crate) fn is_cliclick_special_key(key: &str) -> bool {
    matches!(key.to_lowercase().as_str(),
        "arrow-down" | "arrow-left" | "arrow-right" | "arrow-up"
        | "brightness-down" | "brightness-up"
        | "delete" | "end" | "enter" | "esc"
        | "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8"
        | "f9" | "f10" | "f11" | "f12" | "f13" | "f14" | "f15" | "f16"
        | "fwd-delete" | "home"
        | "keys-light-down" | "keys-light-toggle" | "keys-light-up"
        | "mute" | "num-0" | "num-1" | "num-2" | "num-3" | "num-4"
        | "num-5" | "num-6" | "num-7" | "num-8" | "num-9"
        | "num-clear" | "num-divide" | "num-enter" | "num-equals"
        | "num-minus" | "num-multiply" | "num-plus"
        | "page-down" | "page-up"
        | "play-next" | "play-pause" | "play-previous"
        | "return" | "space" | "tab"
        | "volume-down" | "volume-up"
    )
}

/// Map modifier name to cliclick format.
pub(crate) fn map_modifier(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".to_owned(),
        "alt" | "option" => "alt".to_owned(),
        "shift" => "shift".to_owned(),
        "cmd" | "command" | "super" => "cmd".to_owned(),
        _ => name.to_owned(),
    }
}

/// Map modifier name to xdotool format.
pub(crate) fn map_modifier_xdotool(name: &str) -> String {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => "ctrl".to_owned(),
        "alt" | "option" => "alt".to_owned(),
        "shift" => "shift".to_owned(),
        "super" | "cmd" | "command" => "super".to_owned(),
        _ => name.to_owned(),
    }
}

/// Map a single key name to Windows SendKeys format.
pub(crate) fn win_map_single_key(key: &str) -> String {
    match key {
        "Return" | "Enter" => "{ENTER}".to_owned(),
        "Escape" | "Esc" => "{ESC}".to_owned(),
        "Tab" => "{TAB}".to_owned(),
        "BackSpace" | "Backspace" => "{BACKSPACE}".to_owned(),
        "Delete" => "{DELETE}".to_owned(),
        "Insert" => "{INSERT}".to_owned(),
        "Up" => "{UP}".to_owned(),
        "Down" => "{DOWN}".to_owned(),
        "Left" => "{LEFT}".to_owned(),
        "Right" => "{RIGHT}".to_owned(),
        "Home" => "{HOME}".to_owned(),
        "End" => "{END}".to_owned(),
        "Page_Up" | "PageUp" => "{PGUP}".to_owned(),
        "Page_Down" | "PageDown" => "{PGDN}".to_owned(),
        "F1" => "{F1}".to_owned(),
        "F2" => "{F2}".to_owned(),
        "F3" => "{F3}".to_owned(),
        "F4" => "{F4}".to_owned(),
        "F5" => "{F5}".to_owned(),
        "F6" => "{F6}".to_owned(),
        "F7" => "{F7}".to_owned(),
        "F8" => "{F8}".to_owned(),
        "F9" => "{F9}".to_owned(),
        "F10" => "{F10}".to_owned(),
        "F11" => "{F11}".to_owned(),
        "F12" => "{F12}".to_owned(),
        "space" | "Space" => " ".to_owned(),
        // Special characters that need escaping in SendKeys
        "+" => "{+}".to_owned(),
        "^" => "{^}".to_owned(),
        "%" => "{%}".to_owned(),
        "~" => "{~}".to_owned(),
        "(" => "{(}".to_owned(),
        ")" => "{)}".to_owned(),
        "{" => "{{}".to_owned(),
        "}" => "{}}".to_owned(),
        "[" => "{[}".to_owned(),
        "]" => "{]}".to_owned(),
        other => other.to_owned(),
    }
}

/// Match user text against installed skills by keyword overlap.
#[allow(dead_code)]
pub(crate) fn match_skills<'a>(
    text: &str,
    skills: &'a crate::skill::SkillRegistry,
) -> Vec<&'a crate::skill::SkillManifest> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let lower = text.to_lowercase();
    let mut matched = Vec::new();

    for skill in skills.all() {
        // Skip tool-based skills.
        if !skill.tools.is_empty() {
            continue;
        }
        // Skip skills with no prompt body.
        if skill.prompt.trim().is_empty() {
            continue;
        }

        // Build keyword set from skill name + description.
        let mut keywords: Vec<&str> = Vec::new();

        for part in skill.name.split(|c: char| c == '-' || c == '_' || c == ' ') {
            let p = part.trim();
            if p.len() >= 2 {
                keywords.push(p);
            }
        }

        if let Some(ref desc) = skill.description {
            for word in desc.split(|c: char| !c.is_alphanumeric() && c != '/' && c != '.') {
                let w = word.trim();
                if w.len() >= 2 {
                    keywords.push(w);
                }
            }
        }

        let hit = keywords.iter().any(|kw| {
            let kl = kw.to_lowercase();
            if matches!(kl.as_str(), "the" | "and" | "for" | "with" | "use" | "when" | "from"
                | "create" | "edit" | "file" | "files" | "data" | "tool" | "agent"
                | "的" | "和" | "在" | "是" | "了" | "等") {
                return false;
            }
            lower.contains(&kl)
        });

        if hit {
            matched.push(skill);
        }
    }

    matched
}

/// Create a `tokio::process::Command` for PowerShell that hides the console window.
pub(crate) fn powershell_hidden() -> tokio::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        cmd.arg("-NoProfile").arg("-WindowStyle").arg("Hidden");
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut cmd = tokio::process::Command::new("powershell");
        cmd.arg("-NoProfile");
        cmd
    }
}
