//! One-shot install hints for missing optional dependencies.
//!
//! When sherpa-onnx (STT/TTS), ffmpeg, or any other optional binary is
//! missing and we silently fall back to a degraded path, we want to tell
//! the user *once* — append a short note explaining what's missing and
//! how to install it. After the first hint per feature, stay quiet so
//! users who deliberately don't want the dep aren't pestered.
//!
//! State is persisted to `<base>/var/install-hints.json` so the hint
//! survives gateway restarts. There is no "refused" state — the user
//! either runs the install command we surfaced or doesn't; either way
//! we do not ask again.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::warn;

const HINTS_FILE: &str = "var/install-hints.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct HintsFile {
    #[serde(default)]
    hinted: HashSet<String>,
}

fn hints_path() -> PathBuf {
    crate::config::loader::base_dir().join(HINTS_FILE)
}

fn load() -> HintsFile {
    let path = hints_path();
    let Ok(body) = std::fs::read_to_string(&path) else {
        return HintsFile::default();
    };
    serde_json::from_str(&body).unwrap_or_else(|e| {
        warn!(path = %path.display(), error = %e, "install-hints.json corrupt, resetting");
        HintsFile::default()
    })
}

fn save(state: &HintsFile) {
    let path = hints_path();
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!(path = %path.display(), error = %e, "install-hints: create parent failed");
        return;
    }
    match serde_json::to_string_pretty(state) {
        Ok(body) => {
            if let Err(e) = std::fs::write(&path, body) {
                warn!(path = %path.display(), error = %e, "install-hints: write failed");
            }
        }
        Err(e) => warn!(error = %e, "install-hints: serialize failed"),
    }
}

/// Return `true` once for every feature, then `false` forever after.
///
/// The first call for `feature` records it to disk and returns `true`.
/// Subsequent calls (in the same process or after a restart) return
/// `false`. Callers gate the hint emission on this — emit only when it
/// returns `true`.
pub fn claim_first_hint(feature: &str) -> bool {
    let mut state = load();
    if state.hinted.contains(feature) {
        return false;
    }
    state.hinted.insert(feature.to_owned());
    save(&state);
    true
}
