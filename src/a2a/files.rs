//! A2A file IO — converts wire-level `A2aPart` variants into / from
//! on-disk artifacts that the agent runtime can consume.
//!
//! Layout (mirrors the existing `workspace/uploads/` and `~/Downloads/rsclaw/`
//! conventions, just with `a2a_` prefix and its own bucket):
//!
//! ```text
//! workspace/a2a/
//!   images/   a2a_i_<YYYYMMDDHHmm><abc>.png|jpg|webp|...
//!   videos/   a2a_v_<ts><abc>.mp4|mov|webm|...
//!   audios/   a2a_a_<ts><abc>.mp3|wav|opus|...
//!   docs/     a2a_d_<ts><abc>.pdf|docx|md|...
//!   files/    a2a_f_<ts><abc>.bin
//! ```
//!
//! On the **ingest** side the runtime extracts every non-text `A2aPart` from
//! an incoming `message.parts`, writes it to the right bucket, and synthesises
//! a `@a2a_<kind>_<...>` reference token so the existing `resolve_file_refs`
//! pipeline (image vision + file attachment loading) picks the file up on
//! its own. `A2aPart::Data` is serialised as a fenced JSON block and folded
//! back into the text part so the LLM sees structured input verbatim.
//!
//! On the **emit** side `reply.images` and `reply.files` produced by the
//! runtime are read back from disk, base64-encoded, and emitted as
//! `A2aPart::Raw` alongside the text part inside the final artifact.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use tracing::warn;

use super::types::A2aPart;
use crate::channel::{canonical_filename, category_for_kind, kind_for_mime};

/// `<workspace>/a2a/<category>/` — created if it doesn't exist.
pub fn a2a_dir(workspace: &Path, kind: char) -> PathBuf {
    let dir = workspace.join("a2a").join(category_for_kind(kind));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(err = %e, dir = %dir.display(), "failed to create a2a bucket");
    }
    dir
}

/// Canonical filename for an A2A-received file: `a2a_<kind>_<ts><abc>.<ext>`.
pub fn a2a_filename(mime_type: &str, original: &str) -> String {
    canonical_filename("a2a", mime_type, original)
}

/// What an ingested `A2aPart` contributes to the eventual `AgentMessage`.
/// One ingest call may produce up to one of each — Text parts contribute
/// `text`; Raw image parts contribute a `@-ref` token in `text` (the
/// existing runtime pipeline resolves it to vision); Raw non-image parts
/// contribute a `@-ref` token too; Data parts contribute a fenced JSON
/// block in `text`.
#[derive(Debug, Default, Clone)]
pub struct IngestedParts {
    /// Concatenated text — original Text parts joined, with synthesised
    /// `@a2a_<kind>_...` reference tokens appended for each non-text part
    /// that landed on disk, plus fenced JSON blocks for Data parts.
    pub text: String,
}

/// Ingest every part of an incoming A2A `message.parts` vector. Writes
/// every Raw/Url part to `workspace/a2a/<category>/`, downloads remote
/// `Url` parts inline, and folds Data parts into the synthesised text.
///
/// Failure on a single non-text part (bad base64, unreachable URL, etc.)
/// is logged at WARN and that part is dropped — the rest of the message
/// continues. A2A peers can retry by re-sending; failing the whole turn
/// would be worse UX than degrading to text-only.
pub async fn ingest_message_parts(
    workspace: &Path,
    parts: &[A2aPart],
) -> IngestedParts {
    let mut text_parts: Vec<String> = Vec::new();
    let mut ref_tokens: Vec<String> = Vec::new();
    let mut data_blocks: Vec<String> = Vec::new();

    for part in parts {
        match part {
            A2aPart::Text { text } => {
                text_parts.push(text.clone());
            }
            A2aPart::Raw { bytes, mime_type } => match ingest_raw(workspace, bytes, mime_type) {
                Ok(name) => ref_tokens.push(format!("@{name}")),
                Err(e) => warn!(err = %e, mime = %mime_type, "A2A ingest_raw failed"),
            },
            A2aPart::Url { url, mime_type } => {
                match ingest_url(workspace, url, mime_type.as_deref()).await {
                    Ok(name) => ref_tokens.push(format!("@{name}")),
                    Err(e) => warn!(err = %e, url = %url, "A2A ingest_url failed"),
                }
            }
            A2aPart::Data { data } => {
                // Fence as JSON so the LLM treats it as structured input
                // rather than free-form prose.
                match serde_json::to_string_pretty(data) {
                    Ok(s) => data_blocks.push(format!("```json\n{s}\n```")),
                    Err(e) => warn!(err = %e, "A2A Data part not serialisable"),
                }
            }
        }
    }

    let mut text = text_parts.join("\n");
    if !ref_tokens.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&ref_tokens.join(" "));
    }
    if !data_blocks.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&data_blocks.join("\n\n"));
    }
    IngestedParts { text }
}

/// Decode base64 `bytes`, choose the right bucket from `mime_type`,
/// write the file, return the canonical filename (without leading `@`).
fn ingest_raw(workspace: &Path, bytes_b64: &str, mime_type: &str) -> Result<String> {
    // Tolerate `data:<mime>;base64,<payload>` prefixes — some clients
    // send the whole data URI in the `bytes` field.
    let payload = bytes_b64
        .split(',')
        .next_back()
        .unwrap_or(bytes_b64)
        .trim();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("base64 decode")?;
    let kind = kind_for_mime(mime_type, "");
    let name = a2a_filename(mime_type, "");
    let dir = a2a_dir(workspace, kind);
    let path = dir.join(&name);
    std::fs::write(&path, &raw).with_context(|| format!("write {}", path.display()))?;
    Ok(name)
}

/// Fetch `url`, infer mime + extension, write under the right bucket.
async fn ingest_url(workspace: &Path, url: &str, mime_hint: Option<&str>) -> Result<String> {
    let resp = reqwest::get(url).await.context("HTTP GET")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} fetching {url}");
    }
    let mime = mime_hint
        .map(str::to_owned)
        .or_else(|| {
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned())
        })
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    // Best-effort original filename from the URL path so the extension
    // detection in `kind_for_mime` / `canonical_filename` has something
    // to chew on if the mime is generic.
    let original = url
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .and_then(|tail| tail.split('?').next())
        .unwrap_or("");
    let bytes = resp.bytes().await.context("read body")?;
    let kind = kind_for_mime(&mime, original);
    let name = canonical_filename("a2a", &mime, original);
    let dir = a2a_dir(workspace, kind);
    let path = dir.join(&name);
    std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(name)
}

/// Read a file from disk and pack it as an `A2aPart::Raw`. Used by the
/// reply path to emit images/files produced by the agent runtime back
/// to the A2A caller alongside the text artifact.
pub fn emit_part_from_path(path: &Path) -> Result<A2aPart> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mime = mime_for_path(path);
    let bytes_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(A2aPart::Raw {
        bytes: bytes_b64,
        mime_type: mime,
    })
}

/// Build outbound A2A parts for an agent reply. `text` always becomes the
/// first `A2aPart::Text`. `images` may be data URIs (`data:image/...;base64,…`)
/// or filesystem paths — both are normalised to `A2aPart::Raw`. `files` are
/// expected to be `(filename, mime, path_or_url)`; `Url`-style values
/// pass through as `A2aPart::Url`, everything else is read from disk.
pub fn emit_reply_parts(
    text: &str,
    images: &[String],
    files: &[(String, String, String)],
) -> Vec<A2aPart> {
    let mut parts: Vec<A2aPart> = Vec::new();
    if !text.is_empty() {
        parts.push(A2aPart::Text {
            text: text.to_owned(),
        });
    }
    for img in images {
        match image_to_part(img) {
            Ok(p) => parts.push(p),
            Err(e) => warn!(err = %e, img = %img, "A2A emit: image skipped"),
        }
    }
    for (_name, mime, src) in files {
        if src.starts_with("http://") || src.starts_with("https://") {
            parts.push(A2aPart::Url {
                url: src.clone(),
                mime_type: if mime.is_empty() { None } else { Some(mime.clone()) },
            });
            continue;
        }
        match emit_part_from_path(Path::new(src)) {
            Ok(A2aPart::Raw { bytes, mime_type: _ }) if !mime.is_empty() => {
                // Prefer the runtime-declared mime over the extension-sniffed one.
                parts.push(A2aPart::Raw {
                    bytes,
                    mime_type: mime.clone(),
                });
            }
            Ok(p) => parts.push(p),
            Err(e) => warn!(err = %e, src = %src, "A2A emit: file skipped"),
        }
    }
    parts
}

/// Convert a `reply.images` entry — either a `data:<mime>;base64,<payload>`
/// URI or a filesystem path — to an `A2aPart::Raw`.
fn image_to_part(img: &str) -> Result<A2aPart> {
    if let Some(payload) = img.strip_prefix("data:") {
        // `<mime>;base64,<body>` or `<mime>,<body>` (uncommon, no base64).
        let (mime_chunk, body) = payload
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("malformed data URI"))?;
        let mime = mime_chunk
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_owned();
        return Ok(A2aPart::Raw {
            bytes: body.to_owned(),
            mime_type: mime,
        });
    }
    emit_part_from_path(Path::new(img))
}

fn mime_for_path(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a2a_filename_uses_a2a_prefix_and_correct_kind() {
        let name = a2a_filename("image/png", "");
        assert!(name.starts_with("a2a_i_"), "got {name}");
        assert!(name.ends_with(".png"), "got {name}");

        let name = a2a_filename("video/mp4", "");
        assert!(name.starts_with("a2a_v_"), "got {name}");

        let name = a2a_filename("application/pdf", "report.pdf");
        assert!(name.starts_with("a2a_d_"), "got {name}");
        assert!(name.ends_with(".pdf"), "got {name}");
    }

    #[test]
    fn a2a_dir_creates_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = a2a_dir(tmp.path(), 'i');
        assert!(dir.ends_with("a2a/images"), "got {}", dir.display());
        assert!(dir.exists());
    }

    #[tokio::test]
    async fn ingest_message_parts_text_only_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let parts = vec![A2aPart::Text { text: "hello world".into() }];
        let got = ingest_message_parts(ws, &parts).await;
        assert_eq!(got.text, "hello world");
    }

    #[tokio::test]
    async fn ingest_message_parts_raw_writes_to_bucket_and_emits_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        // 1x1 transparent PNG.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let parts = vec![
            A2aPart::Text { text: "see attached".into() },
            A2aPart::Raw {
                bytes: png_b64.into(),
                mime_type: "image/png".into(),
            },
        ];
        let got = ingest_message_parts(ws, &parts).await;
        assert!(got.text.starts_with("see attached"));
        assert!(got.text.contains("@a2a_i_"), "text: {}", got.text);
        // The bucket should contain exactly one .png file.
        let dir = ws.join("a2a").join("images");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1);
        assert!(files[0].as_ref().unwrap().file_name().to_string_lossy().ends_with(".png"));
    }

    #[tokio::test]
    async fn ingest_message_parts_data_folds_into_json_block() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let parts = vec![
            A2aPart::Text { text: "see structured".into() },
            A2aPart::Data { data: json!({ "k": "v" }) },
        ];
        let got = ingest_message_parts(ws, &parts).await;
        assert!(got.text.contains("see structured"));
        assert!(got.text.contains("```json"), "text: {}", got.text);
        assert!(got.text.contains("\"k\": \"v\""), "text: {}", got.text);
    }

    #[test]
    fn emit_part_from_path_packs_bytes_as_raw() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nhello").unwrap();
        let part = emit_part_from_path(&path).unwrap();
        match part {
            A2aPart::Raw { bytes, mime_type } => {
                assert_eq!(mime_type, "image/png");
                let decoded = base64::engine::general_purpose::STANDARD.decode(&bytes).unwrap();
                assert!(decoded.starts_with(b"\x89PNG"));
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }
}
