//! OCR agent tool. The `OcrClient` itself now lives in the `rsclaw-kb`
//! crate (the KB image/scanned-PDF canonicalizer is its primary user);
//! re-exported here so existing `crate::tools_ocr::OcrClient`
//! call sites keep resolving. This file keeps the `ocr` agent tool
//! handler, which is bound to `AgentRuntime` and therefore stays in root.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Result, anyhow};
pub use rsclaw_kb::OcrClient;
use serde_json::{Value, json};

/// Server-side failures in a row, and the unix second until which the OCR
/// endpoint is treated as down. A model reading a scanned bundle otherwise
/// tries every page against a dead endpoint (50 calls in one case).
static OCR_SERVER_FAILURES: AtomicU32 = AtomicU32::new(0);
static OCR_DOWN_UNTIL: AtomicU64 = AtomicU64::new(0);
const OCR_FAILURES_TO_TRIP: u32 = 3;
const OCR_DOWN_SECS: u64 = 120;
/// Back-off before each retry of a busy or failing endpoint.
const OCR_RETRY_DELAYS_MS: [u64; 3] = [500, 1_000, 2_000];

/// OCR requests in flight at once. Each fleet OCR node takes one image at a
/// time and the gateway spreads requests over ten of them by load, so stay
/// a little under the node count; a rare collision still gets a 502
/// `backend_overload`, which the retries below absorb.
fn ocr_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(8))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Whether an OCR error is the endpoint's fault (down, overloaded,
/// unreachable) rather than the image's.
fn is_server_failure(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("server error") || e.contains("error sending request") || e.contains("timed out")
}

impl super::runtime::AgentRuntime {
    /// OCR an image to verbatim text via the `kb.ocr` endpoint. Accepts a
    /// workspace/uploads file path, an http(s) URL, or a data URI.
    pub(crate) async fn tool_ocr(
        &self,
        ctx: &super::runtime::RunContext,
        args: Value,
    ) -> Result<Value> {
        let image = args["image"].as_str().unwrap_or("").trim();
        if image.is_empty() {
            return Ok(json!({
                "error": "ocr: `image` is required",
                "hint": "Pass a file path, an http(s):// URL, or a data:image/...;base64,... URI."
            }));
        }
        if now_secs() < OCR_DOWN_UNTIL.load(Ordering::Relaxed) {
            return Ok(json!({
                "error": "ocr: the OCR service is unavailable right now (several requests in a row failed on the server)",
                "hint": "Do not call ocr again in this turn. Work with the text you already have, and tell the user which scanned pages could not be read."
            }));
        }
        let Some(client) = OcrClient::from_config() else {
            return Ok(json!({
                "error": "ocr: no OCR endpoint configured",
                "hint": format!(
                    "Set `kb.ocr` in {} (model rsclaw-ocr-v1 needs no baseUrl), then retry.",
                    rsclaw_brand::CONFIG_FILE
                )
            }));
        };

        // Resolve the image to something the endpoint accepts: URLs and
        // data URIs pass through; a local path is read and base64-wrapped.
        let payload = if image.starts_with("http://")
            || image.starts_with("https://")
            || image.starts_with("data:")
        {
            image.to_owned()
        } else {
            // Scope-checked: the bytes are uploaded to the OCR endpoint, so
            // non-owners must not be able to point this at host files.
            let path = match self.resolve_readable_path(ctx, image) {
                Ok(p) => p,
                Err(e) => return Ok(json!({ "error": format!("ocr: {e}") })),
            };
            let bytes = match super::security::read_regular_file_capped(
                &path,
                super::security::MAX_LOCAL_READ_BYTES,
            )
            .await
            {
                Ok(b) => b,
                Err(e) => {
                    return Ok(json!({
                        "error": format!("ocr: cannot read image file `{image}`: {e}"),
                        "hint": "Use a path relative to the workspace, or pass an http(s) URL / data URI."
                    }));
                }
            };
            let mime = rsclaw_kb::canonicalize::detect_mime(&bytes, Some(image));
            // A page rendered at high DPI can pass the endpoint's 16 MB body
            // limit; OCR reads a 2400px long edge fine.
            let (bytes, mime) = tokio::task::spawn_blocking(move || {
                rsclaw_util::downscale_image_for_vision(&bytes, &mime, 4 * 1024 * 1024, 2400, 90)
                    .unwrap_or((bytes, mime))
            })
            .await
            .map_err(|e| anyhow!("ocr: task join failed: {e}"))?;
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            format!("data:{mime};base64,{b64}")
        };

        let lang = args["lang"].as_str().map(str::to_owned);
        let prompt = args["prompt"].as_str().map(str::to_owned);
        let max_tokens = match parse_max_tokens(&args) {
            Ok(max_tokens) => max_tokens,
            Err(message) => return Ok(json!({ "error": message })),
        };
        let _slot = ocr_slots()
            .acquire()
            .await
            .map_err(|e| anyhow!("ocr: slot closed: {e}"))?;
        // OCR transport is sync (block_in_place); run it off the async
        // worker so we don't stall the runtime thread on a slow page. A busy
        // or failing endpoint gets a few backed-off retries first.
        let text = tokio::task::spawn_blocking(move || {
            let mut attempt = 0;
            loop {
                let result = client.ocr_with_options(
                    &payload,
                    prompt.as_deref(),
                    max_tokens,
                    None,
                    lang.as_deref(),
                );
                match result {
                    Err(e)
                        if attempt < OCR_RETRY_DELAYS_MS.len()
                            && is_server_failure(&format!("{e:#}")) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(
                            OCR_RETRY_DELAYS_MS[attempt],
                        ));
                        attempt += 1;
                    }
                    other => break other,
                }
            }
        })
        .await
        .map_err(|e| anyhow!("ocr: task join failed: {e}"))?;

        match text {
            Ok(t) => {
                OCR_SERVER_FAILURES.store(0, Ordering::Relaxed);
                if t.trim().is_empty() {
                    Ok(json!({ "text": "", "note": "OCR returned no text (blank or non-text image)." }))
                } else {
                    Ok(json!({ "text": t.trim() }))
                }
            }
            Err(e) => {
                let error = format!("ocr request failed: {e:#}");
                if is_server_failure(&error)
                    && OCR_SERVER_FAILURES.fetch_add(1, Ordering::Relaxed) + 1 >= OCR_FAILURES_TO_TRIP
                {
                    OCR_SERVER_FAILURES.store(0, Ordering::Relaxed);
                    OCR_DOWN_UNTIL.store(now_secs() + OCR_DOWN_SECS, Ordering::Relaxed);
                }
                Ok(json!({
                    "error": error,
                    "hint": "Endpoint may be down or the image unreadable. Retry once; if it persists tell the user."
                }))
            }
        }
    }
}

fn parse_max_tokens(args: &Value) -> std::result::Result<Option<u32>, &'static str> {
    args["max_tokens"]
        .as_u64()
        .map(u32::try_from)
        .transpose()
        .map_err(|_| "ocr: `max_tokens` must be between 0 and 4294967295")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_failures_are_told_apart_from_bad_images() {
        assert!(is_server_failure(
            "ocr request failed: HTTP status server error (502 Bad Gateway) for url (x)"
        ));
        assert!(!is_server_failure(
            "ocr request failed: HTTP status client error (413 Request Entity Too Large) for url (x)"
        ));
    }

    #[test]
    fn max_tokens_rejects_values_that_do_not_fit_the_wire_type() {
        assert_eq!(
            parse_max_tokens(&json!({ "max_tokens": u32::MAX })),
            Ok(Some(u32::MAX))
        );
        assert!(parse_max_tokens(&json!({ "max_tokens": u64::from(u32::MAX) + 1 })).is_err());
    }
}
