use anyhow::Result;

// ---------------------------------------------------------------------------
// Memory tier detection (AGENTS.md §18 + §30)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryTier {
    /// ≤ ~1.1 GB total RAM
    Low,
    /// ~2 GB
    Standard,
    /// 4 GB+
    High,
}

pub fn detect_memory_tier() -> MemoryTier {
    let total_kb = sys_info::mem_info().map(|m| m.total).unwrap_or(0);

    match total_kb {
        0..=1_200_000 => MemoryTier::Low,
        1_200_001..=2_500_000 => MemoryTier::Standard,
        _ => MemoryTier::High,
    }
}

// ---------------------------------------------------------------------------
// Cross-platform process utilities
// ---------------------------------------------------------------------------

/// Check whether a process with the given PID is still alive.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        #[allow(unused_mut)]
        let mut taskl = Command::new("tasklist");
        taskl.args(["/FI", &format!("PID eq {pid}"), "/NH"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            taskl.creation_flags(0x08000000);
        }
        taskl
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Send a termination signal to the process (SIGTERM on Unix, taskkill on
/// Windows).
pub fn process_terminate(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
            anyhow::bail!("failed to send SIGTERM to process {pid}");
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        // Use /F (force) and /T (tree) -- without /F, taskkill only sends
        // WM_CLOSE which has no effect on windowless background processes.
        #[allow(unused_mut)]
        let mut taskk = Command::new("taskkill");
        taskk.args(["/F", "/T", "/PID", &pid.to_string()]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            taskk.creation_flags(0x08000000);
        }
        let status = taskk.status()?;
        if !status.success() {
            anyhow::bail!("taskkill failed for process {pid}");
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        anyhow::bail!("process termination not supported on this platform");
    }
}

pub fn build_runtime(_tier: MemoryTier) -> Result<tokio::runtime::Runtime> {
    // Use multi_thread with 1 worker and larger stack to avoid stack overflow
    // in debug builds with large code size.
    // This is a workaround for Windows debug build stack size issues.
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // TODO: scale worker threads based on CPU count
        .worker_threads(1)
        .thread_stack_size(8 * 1024 * 1024) // 8MB stack per thread
        .build()?)
}

// Browser/ffmpeg binary detection (crate-split): lifted from agent/platform.rs
// so browser/channel can resolve binaries without depending on the agent knot.
/// Detect Chrome / Chromium binary path.
///
/// Priority: real Google Chrome > `~/.rsclaw/tools/chrome` (managed Chrome
/// for Testing) > other Chromium-family browsers (Edge / Brave / Chromium).
/// The managed Chrome for Testing is deliberately preferred over Edge/Brave:
/// on a machine without Google Chrome, riding on Edge forces the browser
/// onto the system-Chrome `default` profile (see `is_system_chrome`) and
/// runs automation on a different engine — so we'd rather use the real
/// Chrome engine we manage, on the stable `rsclaw` profile.
///
/// `tools/chrome` therefore also covers machines without any Chrome at all
/// after a single `rsclaw tools install chrome`. See `ensure_chrome()` for
/// the auto-install on absence.
pub fn detect_chrome() -> Option<String> {
    // 1. System-installed Chrome (well-known locations + PATH).
    #[cfg(target_os = "macos")]
    {
        // Bundle paths checked under both the system /Applications and the
        // per-user ~/Applications.
        let mut roots = vec![std::path::PathBuf::from("/Applications")];
        if let Some(home) = dirs_next::home_dir() {
            roots.push(home.join("Applications"));
        }
        // 1a. Real Google Chrome first — ride on the user's own install.
        for root in &roots {
            let p = root.join("Google Chrome.app/Contents/MacOS/Google Chrome");
            if p.exists() {
                return Some(p.to_string_lossy().to_string());
            }
        }
        // 1b. Managed Chrome for Testing (~/.rsclaw/tools/chrome) — preferred
        //     over Edge/Brave/Chromium so automation runs on a real Chrome
        //     engine and the stable `rsclaw` profile instead of falling to
        //     Edge (which is forced onto the `default` profile).
        if let Some(m) = managed_chrome() {
            return Some(m);
        }
        // 1c. Other Chromium-family system browsers.
        let rel = [
            "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            "Brave Browser.app/Contents/MacOS/Brave Browser",
            "Chromium.app/Contents/MacOS/Chromium",
        ];
        for root in &roots {
            for r in &rel {
                let p = root.join(r);
                if p.exists() {
                    return Some(p.to_string_lossy().to_string());
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // Registry (most reliable for system + per-user Google Chrome installs).
        for key_path in &[
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
            r"SOFTWARE\Wow6432Node\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe",
        ] {
            for hive in &["HKLM", "HKCU"] {
                if let Ok(output) = std::process::Command::new("reg")
                    .args(["query", &format!(r"{hive}\{key_path}"), "/ve"])
                    .creation_flags(0x08000000)
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

        // Prefer any real Google Chrome discoverable on PATH before our managed
        // build, but do not let Edge/Brave/Chromium outrank managed Chrome.
        for name in &["google-chrome-stable", "google-chrome", "chrome"] {
            if let Ok(path) = which::which(name) {
                return Some(path.to_string_lossy().to_string());
            }
        }

        let local = std::env::var("LOCALAPPDATA").ok();
        let managed = managed_chrome();
        for path in windows_browser_candidates(local.as_deref(), managed.as_deref()) {
            if std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }

        // Other Chromium-family browsers on PATH are the final Windows fallback.
        for name in &[
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
            "microsoft-edge-stable",
        ] {
            if let Ok(path) = which::which(name) {
                return Some(path.to_string_lossy().to_string());
            }
        }
    }

    // PATH lookup (primary path on Unix). Includes the distro-specific binary
    // names apt/dnf/snap install under.
    #[cfg(not(target_os = "windows"))]
    for name in &[
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "brave-browser",
        "microsoft-edge",
        "microsoft-edge-stable",
        "chrome",
    ] {
        if let Ok(path) = which::which(name) {
            return Some(path.to_string_lossy().to_string());
        }
    }

    // Linux absolute-path fallback: a GUI/launchd-launched gateway inherits a
    // stripped PATH that often omits /usr/bin, so `which` above can miss a
    // perfectly-installed browser. Probe the well-known install locations.
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let candidates = [
            "/usr/bin/google-chrome-stable",
            "/usr/bin/google-chrome",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/brave-browser",
            "/usr/bin/microsoft-edge",
            "/usr/bin/microsoft-edge-stable",
            "/opt/google/chrome/chrome",
            "/snap/bin/chromium",
            "/var/lib/snapd/snap/bin/chromium",
        ];
        for p in &candidates {
            if std::path::Path::new(p).exists() {
                return Some((*p).to_owned());
            }
        }
    }

    // 2. Fall back to ~/.rsclaw/tools/chrome (Chrome for Testing we manage). On
    //    macOS this is already tried above (before Edge/Brave); this covers
    //    Windows/Linux and macOS machines with no system browser at all.
    if let Some(m) = managed_chrome() {
        return Some(m);
    }

    None
}

#[cfg(any(target_os = "windows", test))]
fn windows_browser_candidates(local: Option<&str>, managed: Option<&str>) -> Vec<String> {
    let mut candidates = vec![
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".to_owned(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".to_owned(),
    ];
    if let Some(local) = local {
        candidates.push(format!(r"{local}\Google\Chrome\Application\chrome.exe"));
    }
    if let Some(managed) = managed {
        candidates.push(managed.to_owned());
    }
    candidates.extend([
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe".to_owned(),
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe".to_owned(),
        r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe".to_owned(),
        r"C:\Program Files (x86)\BraveSoftware\Brave-Browser\Application\brave.exe".to_owned(),
    ]);
    if let Some(local) = local {
        candidates.extend([
            format!(r"{local}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{local}\BraveSoftware\Brave-Browser\Application\brave.exe"),
        ]);
    }
    candidates
}

/// Path to the rsclaw-managed Chrome for Testing under
/// `~/.rsclaw/tools/chrome`, if a usable binary is present. Returns `None` when
/// the dir is absent.
fn managed_chrome() -> Option<String> {
    let tools_dir = rsclaw_config::loader::base_dir().join("tools/chrome");
    if !tools_dir.exists() {
        return None;
    }
    #[cfg(target_os = "macos")]
    let candidates: &[&str] = &[
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Chromium.app/Contents/MacOS/Chromium",
        "Google Chrome.app/Contents/MacOS/Google Chrome",
    ];
    #[cfg(target_os = "windows")]
    let candidates: &[&str] = &["chrome.exe", "Google Chrome for Testing.exe"];
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let candidates: &[&str] = &["chrome"];
    for name in candidates {
        let bin = tools_dir.join(name);
        if bin.exists() {
            return Some(bin.to_string_lossy().to_string());
        }
    }
    None
}

/// Detect ffmpeg binary path.
///
/// Priority: `~/.rsclaw/tools/ffmpeg/ffmpeg` > system PATH.
/// Local-first because the bundled build is pinned to a known-good version
/// and ships every codec we need; system ffmpeg may be a stripped distro
/// build missing libopus/libx264.
pub fn detect_ffmpeg() -> Option<String> {
    let tools_dir = rsclaw_config::loader::base_dir().join("tools/ffmpeg");
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
pub mod capture;
pub mod install_hints;

#[cfg(test)]
mod tests {
    use super::windows_browser_candidates;

    #[test]
    fn windows_managed_chrome_precedes_edge_and_brave() {
        let managed = r"C:\Users\test\.rsclaw\tools\chrome\chrome.exe";
        let candidates =
            windows_browser_candidates(Some(r"C:\Users\test\AppData\Local"), Some(managed));
        let managed_index = candidates.iter().position(|path| path == managed).unwrap();
        let first_alternative = candidates
            .iter()
            .position(|path| path.contains("Microsoft\\Edge") || path.contains("BraveSoftware"))
            .unwrap();

        assert!(managed_index < first_alternative);
        assert!(
            candidates[..managed_index]
                .iter()
                .all(|path| path.contains("Google\\Chrome"))
        );
    }
}
