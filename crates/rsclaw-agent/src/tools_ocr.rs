//! OCR agent tool. The `OcrClient` itself now lives in the `rsclaw-kb`
//! crate (the KB image/scanned-PDF canonicalizer is its primary user);
//! re-exported here so existing `crate::tools_ocr::OcrClient`
//! call sites keep resolving. This file keeps the `ocr` agent tool
//! handler, which is bound to `AgentRuntime` and therefore stays in root.

use anyhow::{Result, anyhow};
pub use rsclaw_kb::OcrClient;
use serde_json::{Value, json};

impl super::runtime::AgentRuntime {
    /// OCR an image to verbatim text via the `kb.ocr` endpoint. Accepts a
    /// workspace/uploads file path, an http(s) URL, or a data URI.
    pub(crate) async fn tool_ocr(&self, args: Value) -> Result<Value> {
        let image = args["image"].as_str().unwrap_or("").trim();
        if image.is_empty() {
            return Ok(json!({
                "error": "ocr: `image` is required",
                "hint": "Pass a file path, an http(s):// URL, or a data:image/...;base64,... URI."
            }));
        }
        let Some(client) = OcrClient::from_config() else {
            return Ok(json!({
                "error": "ocr: no OCR endpoint configured",
                "hint": "Set `kb.ocr` in rsclaw.json5 (model rsclaw-ocr-v1 needs no baseUrl), then retry."
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
            let workspace = self.default_workspace();
            let path = super::runtime::canonicalize_external_path(image, &workspace);
            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => {
                    return Ok(json!({
                        "error": format!("ocr: cannot read image file `{image}`: {e}"),
                        "hint": "Use a path relative to the workspace, or pass an http(s) URL / data URI."
                    }));
                }
            };
            let mime = rsclaw_kb::canonicalize::detect_mime(&bytes, Some(image));
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
        // OCR transport is sync (block_in_place); run it off the async
        // worker so we don't stall the runtime thread on a slow page.
        let text = tokio::task::spawn_blocking(move || {
            client.ocr_with_options(
                &payload,
                prompt.as_deref(),
                max_tokens,
                None,
                lang.as_deref(),
            )
        })
        .await
        .map_err(|e| anyhow!("ocr: task join failed: {e}"))?;

        match text {
            Ok(t) if !t.trim().is_empty() => Ok(json!({ "text": t.trim() })),
            Ok(_) => {
                Ok(json!({ "text": "", "note": "OCR returned no text (blank or non-text image)." }))
            }
            Err(e) => Ok(json!({
                "error": format!("ocr request failed: {e:#}"),
                "hint": "Endpoint may be down or the image unreadable. Retry once; if it persists tell the user."
            })),
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
    fn max_tokens_rejects_values_that_do_not_fit_the_wire_type() {
        assert_eq!(
            parse_max_tokens(&json!({ "max_tokens": u32::MAX })),
            Ok(Some(u32::MAX))
        );
        assert!(parse_max_tokens(&json!({ "max_tokens": u64::from(u32::MAX) + 1 })).is_err());
    }
}
