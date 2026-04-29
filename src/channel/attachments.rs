//! Shared helpers for outbound attachment handling.
//!
//! Multiple channels (Discord, Slack, ...) need to translate the runtime's
//! image / file representations into platform-specific upload payloads.
//! These helpers centralise the bits that are identical across platforms:
//!
//! - `parse_data_url` extracts the MIME and base64 payload from a
//!   `data:image/<subtype>;base64,...` URI of any subtype, including those
//!   with extra `;param=value` sections (RFC 2397).
//! - `mime_to_ext` maps an `image/*` MIME to a sensible filename extension
//!   so the platform's CDN inline-preview path picks up the right format.
//! - `pick_file_mime` chooses the MIME for a generic file upload, preferring
//!   the MIME the tool layer attached and falling back to extension-based
//!   guessing so videos/audio/PDFs aren't downgraded to opaque blobs.

/// Parse a `data:image/<subtype>[;params];base64,<payload>` URL.
///
/// Returns `(mime, base64_payload)` slices borrowed from the input on
/// success. Anything not matching the data-URL shape (missing `data:`
/// scheme, missing `;base64` marker, non-image MIME) returns `None` so
/// the caller can decide on a fallback.
pub(crate) fn parse_data_url(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let header = &rest[..comma];
    let payload = &rest[comma + 1..];
    let header = header.strip_suffix(";base64")?;
    let mime = header.split(';').next().unwrap_or(header);
    if !mime.starts_with("image/") {
        return None;
    }
    Some((mime, payload))
}

/// Map an `image/*` MIME type to a sensible filename extension.
pub(crate) fn mime_to_ext(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}

/// Pick the MIME to send for a file attachment.
///
/// Prefer the MIME the tool layer attached. When it's empty (e.g. a tool
/// produced a path without metadata), guess from the filename extension —
/// platforms use Content-Type to decide whether to inline-preview videos,
/// audio, and PDFs, so falling back to `application/octet-stream` would
/// hide those previews.
pub(crate) fn pick_file_mime<'a>(mime: &'a str, filename: &'a str) -> &'a str {
    if !mime.is_empty() {
        return mime;
    }
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_url_png() {
        let (mime, b64) = parse_data_url("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(b64, "iVBORw0KGgo=");
    }

    #[test]
    fn parse_data_url_webp() {
        let (mime, b64) = parse_data_url("data:image/webp;base64,UklGRg==").unwrap();
        assert_eq!(mime, "image/webp");
        assert_eq!(b64, "UklGRg==");
    }

    #[test]
    fn parse_data_url_strips_extra_params() {
        let (mime, b64) =
            parse_data_url("data:image/jpeg;charset=utf-8;base64,/9j/=").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(b64, "/9j/=");
    }

    #[test]
    fn parse_data_url_rejects_non_image() {
        assert!(parse_data_url("data:text/plain;base64,SGk=").is_none());
    }

    #[test]
    fn parse_data_url_rejects_non_base64() {
        assert!(parse_data_url("data:image/png,raw").is_none());
    }

    #[test]
    fn parse_data_url_rejects_http() {
        assert!(parse_data_url("https://example.com/x.png").is_none());
    }

    #[test]
    fn mime_to_ext_known_types() {
        assert_eq!(mime_to_ext("image/jpeg"), "jpg");
        assert_eq!(mime_to_ext("image/webp"), "webp");
        assert_eq!(mime_to_ext("image/png"), "png");
        assert_eq!(mime_to_ext("image/heic"), "png");
    }

    #[test]
    fn pick_file_mime_prefers_explicit() {
        assert_eq!(pick_file_mime("video/mp4", "weird.bin"), "video/mp4");
    }

    #[test]
    fn pick_file_mime_guesses_from_extension_when_empty() {
        assert_eq!(pick_file_mime("", "clip.mp4"), "video/mp4");
        assert_eq!(pick_file_mime("", "song.MP3"), "audio/mpeg");
        assert_eq!(pick_file_mime("", "doc.pdf"), "application/pdf");
    }

    #[test]
    fn pick_file_mime_falls_back_when_unknown_extension() {
        assert_eq!(pick_file_mime("", "blob.xyz"), "application/octet-stream");
        assert_eq!(pick_file_mime("", "noext"), "application/octet-stream");
    }
}
