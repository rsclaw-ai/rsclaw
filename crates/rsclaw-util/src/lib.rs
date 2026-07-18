//! Small shared helpers.

/// Estimate token count for mixed-language text.
/// - ASCII/Latin: ~4 chars per token
/// - CJK (Chinese/Japanese/Korean): ~1.5 chars per token
/// - Other Unicode: ~2 chars per token
pub fn estimate_tokens(text: &str) -> usize {
    let mut ascii_chars = 0usize;
    let mut cjk_chars = 0usize;
    let mut other_chars = 0usize;
    for ch in text.chars() {
        if ch.is_ascii() {
            ascii_chars += 1;
        } else if ('\u{4E00}'..='\u{9FFF}').contains(&ch)
            || ('\u{3400}'..='\u{4DBF}').contains(&ch)
            || ('\u{3000}'..='\u{303F}').contains(&ch)
            || ('\u{FF00}'..='\u{FFEF}').contains(&ch)
            || ('\u{AC00}'..='\u{D7AF}').contains(&ch)
        {
            cjk_chars += 1;
        } else {
            other_chars += 1;
        }
    }
    ascii_chars / 4 + (cjk_chars * 2 + 1) / 3 + other_chars / 2 + 1
}

/// Truncate `s` to at most `max_bytes`, snapping DOWN to the nearest UTF-8 char
/// boundary so the returned slice never falls inside a multi-byte character.
///
/// `&s[..n]` PANICS when `n` lands inside a char (e.g. a 3-byte CJK character)
/// or past the end. Use this for every log/error preview of a string that may
/// contain non-ASCII text — `&s[..s.len().min(n)]` is NOT safe for CJK.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Downscale + re-encode an image so it fits in vision provider inline limits.
///
/// Triggered only when EITHER the raw byte count exceeds `byte_threshold` OR
/// the longer pixel edge exceeds `max_long_edge`. When either is true the image
/// is decoded, resized so its long edge matches `max_long_edge` (Lanczos3),
/// and re-encoded as JPEG at `quality` (recommended: 85). Returns the new bytes
/// and the new MIME type ("image/jpeg" once we re-encode).
///
/// Token cost for vision LLMs is driven by pixel dimensions, not file size, so
/// the downscale is essentially free quality-wise for typical screenshots
/// (1080p / 4K) and bounds the wire payload below every major provider's hard
/// limit (Anthropic 5 MB, OpenAI ~10 MB total body, Gemini 20 MB).
///
/// Fast-path: under both thresholds → returns the input unchanged.
///
/// Errors: returns `Err` if decode fails (file is corrupt or not a supported
/// image format). The caller is expected to surface this — fallback to
/// passing the original bytes is also reasonable for callers that prioritize
/// "ship something" over "reject corrupt input".
pub fn downscale_image_for_vision(
    data: &[u8],
    original_mime: &str,
    byte_threshold: usize,
    max_long_edge: u32,
    quality: u8,
) -> anyhow::Result<(Vec<u8>, String)> {
    // Cheap dimension probe via image's reader — avoids full decode when the
    // image is already small in both bytes and pixels.
    let needs_pixel_resize = {
        let cursor = std::io::Cursor::new(data);
        match image::ImageReader::new(cursor).with_guessed_format() {
            Ok(reader) => match reader.into_dimensions() {
                Ok((w, h)) => w.max(h) > max_long_edge,
                Err(_) => false, // can't read dims — skip pixel check, only bytes matter
            },
            Err(_) => false,
        }
    };

    if data.len() <= byte_threshold && !needs_pixel_resize {
        return Ok((data.to_vec(), original_mime.to_string()));
    }

    let img = image::load_from_memory(data)
        .map_err(|e| anyhow::anyhow!("downscale: decode failed: {e}"))?;
    let (w, h) = (img.width(), img.height());
    let resized = if w.max(h) > max_long_edge {
        let ratio = f64::from(max_long_edge) / f64::from(w.max(h));
        let nw = (f64::from(w) * ratio).round() as u32;
        let nh = (f64::from(h) * ratio).round() as u32;
        img.resize(nw, nh, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    let mut buf = Vec::with_capacity(resized.width() as usize * resized.height() as usize / 4);
    let rgb = resized.to_rgb8();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    rgb.write_with_encoder(encoder)
        .map_err(|e| anyhow::anyhow!("downscale: jpeg encode failed: {e}"))?;
    Ok((buf, "image/jpeg".to_string()))
}

/// Re-encode an oversized vision image without changing its pixel dimensions.
///
/// UI action models ground coordinates against the pixels they receive. This
/// helper therefore only reduces JPEG quality after a byte-size threshold; it
/// never resizes the image.
pub fn reencode_image_for_vision(
    data: &[u8],
    original_mime: &str,
    byte_threshold: usize,
    quality: u8,
) -> anyhow::Result<(Vec<u8>, String)> {
    if data.len() <= byte_threshold {
        return Ok((data.to_vec(), original_mime.to_string()));
    }
    let image = image::load_from_memory(data)
        .map_err(|error| anyhow::anyhow!("vision re-encode: decode failed: {error}"))?;
    let mut encoded = Vec::with_capacity(data.len() / 2);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality);
    image
        .to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|error| anyhow::anyhow!("vision re-encode: jpeg encode failed: {error}"))?;
    Ok((encoded, "image/jpeg".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_never_splits_a_cjk_char() {
        // "创建一个完整..." — each Chinese char is 3 UTF-8 bytes. Truncating at
        // byte 200 (the real panic offset) must snap back to a char boundary.
        let s = "创建一个完整的进销存管理系统".repeat(20);
        for max in [0, 1, 2, 3, 4, 50, 80, 199, 200, 500] {
            let t = truncate_str(&s, max);
            assert!(t.len() <= max.min(s.len()) || max == 0);
            assert!(s.starts_with(t)); // valid prefix, no panic, no garbage
        }
    }

    #[test]
    fn truncate_shorter_than_max_is_identity() {
        assert_eq!(truncate_str("hi", 100), "hi");
        assert_eq!(truncate_str("中文", 100), "中文");
    }

    #[test]
    fn truncate_ascii_exact() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }
}

// Path helpers (crate-split): expand_tilde + canonicalize_external_path lifted
// from agent/runtime.rs so plugin/tools can normalize paths without the agent
// knot.
/// Expand a leading `~/` to the user's home directory.
pub fn expand_tilde(p: &str) -> std::path::PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        dirs_next::home_dir().unwrap_or_default().join(rest)
    } else if p == "~" {
        dirs_next::home_dir().unwrap_or_default()
    } else {
        std::path::PathBuf::from(p)
    }
}
/// Canonicalize a path received from an external source (tool output, plugin
/// result, LLM-generated argument). Performs:
///   1. `~/...` expansion via [`expand_tilde`]
///   2. If still relative, joins with `workspace`
///   3. Collapses `.` and `..` components without requiring filesystem access
///      (does NOT follow symlinks — this is a pure lexical normalization).
///
/// This is the single entry point for turning untrusted path strings into
/// filesystem paths the host will actually read/write. Call this instead of
/// `PathBuf::from(s)` at module boundaries.
pub fn canonicalize_external_path(input: &str, workspace: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let expanded = expand_tilde(input);
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    };
    let mut out = std::path::PathBuf::new();
    for c in absolute.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
/// Read a dotted-path nested value from a JSON object (crate-split: lifted from
/// cmd/config_json.rs so non-cmd crates can resolve config paths).
pub fn get_nested_value<'a>(
    val: &'a serde_json::Value,
    key: &str,
) -> Option<&'a serde_json::Value> {
    let mut cur = val;
    for part in key.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}
