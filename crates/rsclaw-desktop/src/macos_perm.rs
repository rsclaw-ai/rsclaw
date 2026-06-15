//! macOS TCC permission preflight (Screen Recording + Accessibility).
//!
//! Why this exists
//! ---------------
//! reqwest aside, the desktop session needs two macOS privacy grants to
//! work: **Screen Recording** (for window/screen capture via xcap) and
//! **Accessibility** (for synthetic mouse/keyboard via enigo). macOS keys
//! these grants in its TCC database by the binary's *code-signing
//! designated requirement*. When the gateway binary is **ad-hoc signed**
//! (`codesign --sign -`), that requirement is a pinned **cdhash** — so
//! every `cargo` rebuild changes the cdhash and the grant silently stops
//! applying, even though System Settings still shows the toggle enabled.
//! The failure mode is ugly and silent: window capture hangs for ~30s and
//! returns `Failed to copy data`, then falls back to a wrong-region grab;
//! input simulation just no-ops.
//!
//! This module turns that *silent* denial into a *loud* one: at startup we
//! call the OS preflight APIs (`CGPreflightScreenCaptureAccess`,
//! `AXIsProcessTrusted`) and log an actionable warning when a grant isn't
//! effective. It does NOT prompt — it only reports — so the operator knows
//! to re-grant (and, ideally, to sign with a STABLE identity so the grant
//! survives rebuilds; an ad-hoc signature never will).

/// Effective (runtime) state of the two macOS grants the desktop session
/// needs. "Effective" means the OS preflight says YES — not merely that a
/// row exists in the TCC database (a cdhash-pinned row for a stale build
/// reads as denied here even while System Settings shows it enabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacPermissions {
    /// `CGPreflightScreenCaptureAccess()` — needed for window/screen capture.
    pub screen_recording: bool,
    /// `AXIsProcessTrusted()` — needed for synthetic mouse/keyboard input.
    pub accessibility: bool,
}

/// Build the loud-warning lines for any grant that is NOT effective.
/// Empty vec == everything is fine. Pure (no FFI) so it is unit-testable
/// on every platform.
pub fn permission_warnings(p: &MacPermissions) -> Vec<String> {
    let mut warnings = Vec::new();
    if !p.screen_recording {
        warnings.push(
            "macOS Screen Recording is NOT effective for this binary — window/screen \
             capture will fail (hang ~30s then `Failed to copy data`, falling back to a \
             wrong-region grab). Re-grant in System Settings → Privacy & Security → \
             Screen Recording. NOTE: an ad-hoc (`codesign --sign -`) signature pins the \
             grant to the build's cdhash, so EVERY rebuild breaks it — sign with a stable \
             identity (see scripts/dev-build.sh) so the grant survives."
                .to_string(),
        );
    }
    if !p.accessibility {
        warnings.push(
            "macOS Accessibility is NOT effective for this binary — synthetic mouse/keyboard \
             input will silently no-op. Re-grant in System Settings → Privacy & Security → \
             Accessibility. (Same cdhash-pinning caveat as Screen Recording: an ad-hoc \
             signature breaks the grant on every rebuild — use a stable signing identity.)"
                .to_string(),
        );
    }
    warnings
}

/// Query the OS for the *effective* (runtime) state of the two grants.
///
/// On macOS this calls the real preflight APIs. On other platforms TCC
/// does not apply, so both are reported as granted (callers no-op).
#[cfg(target_os = "macos")]
pub fn query() -> MacPermissions {
    MacPermissions {
        // `bool CGPreflightScreenCaptureAccess(void)` (macOS 10.15+) — true
        // when this process can currently capture the screen. Unlike reading
        // the TCC row, this honours the cdhash/identity requirement, so a
        // stale-cdhash grant correctly reads as `false`.
        screen_recording: unsafe { ffi::CGPreflightScreenCaptureAccess() },
        // `Boolean AXIsProcessTrusted(void)` — true when this process is
        // trusted for the Accessibility API (synthetic input).
        accessibility: unsafe { ffi::AXIsProcessTrusted() != 0 },
    }
}

#[cfg(not(target_os = "macos"))]
pub fn query() -> MacPermissions {
    MacPermissions {
        screen_recording: true,
        accessibility: true,
    }
}

/// Query the grants and return loud warnings for any that aren't effective.
/// Convenience wrapper for callers (e.g. gateway startup) that just want to
/// log. Empty vec == all good (or non-macOS).
pub fn preflight_warnings() -> Vec<String> {
    permission_warnings(&query())
}

#[cfg(target_os = "macos")]
mod ffi {
    // CoreGraphics: screen-capture preflight (no prompt, just queries state).
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        pub fn CGPreflightScreenCaptureAccess() -> bool;
    }
    // ApplicationServices (HIServices): accessibility trust check.
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        // Returns CoreFoundation `Boolean` (unsigned char): 0 / 1.
        pub fn AXIsProcessTrusted() -> u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_warnings_when_all_granted() {
        let p = MacPermissions {
            screen_recording: true,
            accessibility: true,
        };
        assert!(permission_warnings(&p).is_empty());
    }

    #[test]
    fn warns_about_screen_recording_when_denied() {
        let p = MacPermissions {
            screen_recording: false,
            accessibility: true,
        };
        let w = permission_warnings(&p);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("Screen Recording"),
            "expected Screen Recording warning, got: {w:?}"
        );
    }

    #[test]
    fn warns_about_accessibility_when_denied() {
        let p = MacPermissions {
            screen_recording: true,
            accessibility: false,
        };
        let w = permission_warnings(&p);
        assert_eq!(w.len(), 1);
        assert!(
            w[0].contains("Accessibility"),
            "expected Accessibility warning, got: {w:?}"
        );
    }

    #[test]
    fn warns_about_both_when_both_denied() {
        let p = MacPermissions {
            screen_recording: false,
            accessibility: false,
        };
        assert_eq!(permission_warnings(&p).len(), 2);
    }
}
